//! T080 phase 3 fixtures for permission rules, layers and trust.

use super::*;
use crate::tools::models::{PolicyEffect, ToolCall, ToolInvocationId};
use serde_json::json;
use serial_test::serial;

fn write_config(home: &TestNibHome, text: &str) {
    std::fs::write(home.directory.path().join("config.toml"), text).unwrap();
}

fn call(tool: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        invocation_id: ToolInvocationId::new(),
        tool_name: tool.to_string(),
        arguments,
        session_id: None,
        project_root: None,
    }
}

fn terminal(command: &str) -> ToolCall {
    call("run_terminal", json!({ "command": command }))
}

fn rules(entries: &[(RuleEffect, &str)]) -> RuleSet {
    let mut set = RuleSet::default();
    for (effect, text) in entries {
        set.push_rules(*effect, RuleLayer::User, &[(*text).to_string()]);
    }
    assert!(set.problems.is_empty(), "{:?}", set.problems);
    set
}

fn effect_of(set: &RuleSet, call: &ToolCall) -> Option<PolicyEffect> {
    set.evaluate(call, Path::new("/work/project"))
        .map(|rule| rule.effect)
}

#[test]
fn rules_parse_aliases_prefixes_and_reject_shell_syntax() {
    assert_eq!(
        parse_rule("terminal(cargo test *)").unwrap(),
        (
            "run_terminal".to_string(),
            Some(Specifier::Command {
                words: vec!["cargo".into(), "test".into()],
                prefix: true
            })
        )
    );
    assert_eq!(
        parse_rule("Bash(npm run build:*)").unwrap().1,
        Some(Specifier::Command {
            words: vec!["npm".into(), "run".into(), "build".into()],
            prefix: true
        })
    );
    assert_eq!(
        parse_rule("read(./.env)").unwrap(),
        (
            "read_file".to_string(),
            Some(Specifier::Path(".env".into()))
        )
    );
    assert_eq!(
        parse_rule("edit").unwrap(),
        ("apply_patch".to_string(), None)
    );
    assert_eq!(parse_rule("run_terminal(*)").unwrap().1, None);
    for invalid in [
        "",
        "terminal(cargo test",
        "terminal(echo $HOME *)",
        "terminal(a && b)",
        "terminal(\"quoted\")",
        "web search",
        "manage_task(x)",
    ] {
        assert!(parse_rule(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn suggested_rules_use_a_subcommand_prefix_or_the_exact_command() {
    for (command, expected) in [
        ("cargo test -p nib", Some("cargo test *")),
        ("cargo test", Some("cargo test *")),
        ("npm run build -- --watch", Some("npm run build *")),
        ("task test:interactive", Some("task test:interactive *")),
        ("npm run", Some("npm run")),
        ("cargo --version", Some("cargo --version")),
        ("python3 -m pytest", Some("python3 -m pytest")),
        ("sudo apt install x", Some("sudo apt install x")),
        ("./build.sh release", Some("./build.sh release")),
        ("FOO=1 cargo test", Some("FOO=1 cargo test")),
        ("make", None),
        ("cargo test && cargo fmt", None),
        ("echo \"x\" y", None),
        ("cargo test\ncargo fmt", None),
    ] {
        assert_eq!(
            suggested_terminal_rule(command).as_deref(),
            expected,
            "{command}"
        );
    }
}

#[test]
fn compound_commands_are_judged_part_by_part() {
    let set = rules(&[(RuleEffect::Allow, "terminal(cargo test *)")]);
    for (command, allowed) in [
        ("cargo test", true),
        ("cargo test -p nib -- --nocapture", true),
        ("cargo test && git status", true),
        ("git log; git status && cargo test", true),
        ("cargo testx", false),
        ("cargo build", false),
        ("cargo test && rm -rf target", false),
        ("cargo test; touch x", false),
        ("cargo test & touch x", false),
        ("cargo test > out.txt", false),
        ("cargo test $(touch x)", false),
        ("cargo test \"name\"", false),
        ("/usr/bin/cargo test", false),
        ("git status", false),
    ] {
        assert_eq!(
            effect_of(&set, &terminal(command)) == Some(PolicyEffect::Allow),
            allowed,
            "{command}"
        );
    }
    let exact = rules(&[(RuleEffect::Allow, "terminal(cargo test)")]);
    assert_eq!(
        effect_of(&exact, &terminal("cargo test")),
        Some(PolicyEffect::Allow)
    );
    assert_eq!(effect_of(&exact, &terminal("cargo test -p x")), None);
    let everything = rules(&[(RuleEffect::Allow, "terminal")]);
    assert_eq!(
        effect_of(&everything, &terminal("echo \"x\" > y")),
        Some(PolicyEffect::Allow)
    );
}

#[test]
fn deny_beats_ask_beats_allow_and_matches_any_part() {
    let set = rules(&[
        (RuleEffect::Allow, "terminal(cargo *)"),
        (RuleEffect::Ask, "terminal(cargo publish *)"),
        (RuleEffect::Deny, "terminal(rm *)"),
    ]);
    assert_eq!(
        effect_of(&set, &terminal("cargo build")),
        Some(PolicyEffect::Allow)
    );
    assert_eq!(
        effect_of(&set, &terminal("cargo publish --dry-run")),
        Some(PolicyEffect::RequireApproval)
    );
    for denied in [
        "rm -rf target",
        "cargo build && rm x",
        "/bin/rm x",
        "echo \"x\" && rm y",
    ] {
        assert_eq!(
            effect_of(&set, &terminal(denied)),
            Some(PolicyEffect::Deny),
            "{denied}"
        );
    }
}

#[test]
fn path_rules_use_gitignore_style_globs() {
    assert!(glob_matches(".env", ".env"));
    assert!(glob_matches(".env", "config/.env"));
    assert!(glob_matches("*.pem", "keys/server.pem"));
    assert!(glob_matches("src/**", "src/tools/mod.rs"));
    assert!(glob_matches("src", "src/main.rs"));
    assert!(glob_matches("src/**/*.rs", "src/a/b/c.rs"));
    assert!(!glob_matches("src/**", "tests/src.rs"));
    assert!(glob_matches("/src", "src"));
    assert!(!glob_matches("/.env", "config/.env"));
    assert!(!glob_matches(".env", ".envrc"));

    let set = rules(&[
        (RuleEffect::Deny, "read(.env)"),
        (RuleEffect::Allow, "edit(src/**)"),
    ]);
    for path in [".env", "./config/.env", "a/../.env", "/work/project/.env"] {
        assert_eq!(
            effect_of(&set, &call("read_file", json!({ "path": path }))),
            Some(PolicyEffect::Deny),
            "{path}"
        );
    }
    assert_eq!(
        effect_of(&set, &call("read_file", json!({ "path": "README.md" }))),
        None
    );
    let patch = |paths: &[&str]| {
        let mut text = String::new();
        for path in paths {
            text.push_str(&format!(
                "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-a\n+b\n"
            ));
        }
        call("apply_patch", json!({ "patch": text }))
    };
    assert_eq!(
        effect_of(&set, &patch(&["src/main.rs"])),
        Some(PolicyEffect::Allow)
    );
    assert_eq!(effect_of(&set, &patch(&["src/main.rs", "README.md"])), None);
    assert_eq!(
        patch_paths("--- /dev/null\n+++ b/new.txt\n"),
        vec!["new.txt".to_string()]
    );
}

#[test]
#[serial]
fn layers_load_from_nib_home_and_trust_is_per_project() {
    let home = TestNibHome::isolated();
    let project = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    write_config(
        &home,
        "[llm]\nignored = true\n\n[permissions]\nallow = [\"terminal(task check *)\"]\ndeny = [\"read(.env)\", \"terminal(bad\"]\n",
    );

    let set = RuleSet::load(project.path());
    assert!(!set.trusted);
    assert_eq!(set.rules.len(), 2, "{:?}", set.rules);
    assert_eq!(set.problems.len(), 1, "{:?}", set.problems);

    assert!(add_project_rule(project.path(), RuleEffect::Allow, "terminal(cargo test *)").unwrap());
    assert!(
        !add_project_rule(project.path(), RuleEffect::Allow, "terminal(cargo test *)").unwrap()
    );
    assert!(add_project_rule(project.path(), RuleEffect::Deny, "terminal(rm").is_err());
    assert!(set_project_trust(project.path(), true).unwrap());
    assert!(!set_project_trust(project.path(), true).unwrap());

    let set = RuleSet::load(project.path());
    assert!(set.trusted && project_trusted(project.path()));
    assert!(
        set.rules
            .iter()
            .any(|rule| rule.layer == RuleLayer::ProjectLocal
                && rule.text == "terminal(cargo test *)")
    );
    assert_eq!(
        set.evaluate(&terminal("cargo test"), project.path())
            .map(|rule| rule.effect),
        Some(PolicyEffect::Allow)
    );

    let unrelated = RuleSet::load(other.path());
    assert!(!unrelated.trusted && !project_trusted(other.path()));
    assert_eq!(unrelated.rules.len(), 2);

    assert!(remove_project_rule(project.path(), "terminal(cargo test *)").unwrap());
    assert!(!remove_project_rule(project.path(), "terminal(cargo test *)").unwrap());
    assert!(set_project_trust(project.path(), false).unwrap());
    let saved = std::fs::read_to_string(home.directory.path().join(PERMISSIONS_FILE)).unwrap();
    assert!(
        !saved.contains(&project.path().display().to_string()),
        "{saved}"
    );
}

#[test]
#[serial]
fn a_damaged_permissions_file_is_reported_and_never_overwritten() {
    let home = TestNibHome::isolated();
    let project = tempfile::tempdir().unwrap();
    let path = home.directory.path().join(PERMISSIONS_FILE);
    std::fs::write(&path, "{not json").unwrap();
    let set = RuleSet::load(project.path());
    assert!(!set.trusted);
    assert_eq!(set.problems.len(), 1);
    assert!(set_project_trust(project.path(), true).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");
}
