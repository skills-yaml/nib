//! T080 phase 3 fixtures for permission rules, layers and trust.

use super::*;
use crate::tools::models::{PolicyEffect, ToolCall, ToolInvocationId};
use crate::tools::permission_matching::{
    directory_reaches, git_patch_paths, glob_matches, parse_numstat, parse_rule, Specifier,
};
use serde_json::json;

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

fn effect_in(set: &RuleSet, call: &ToolCall, root: &Path) -> Option<PolicyEffect> {
    set.evaluate(call, &[root]).map(|rule| rule.effect)
}

fn effect_of(set: &RuleSet, call: &ToolCall) -> Option<PolicyEffect> {
    effect_in(set, call, Path::new("/work/project"))
}

fn patch(paths: &[&str]) -> ToolCall {
    let mut text = String::new();
    for path in paths {
        text.push_str(&format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-a\n+b\n"
        ));
    }
    call("apply_patch", json!({ "patch": text }))
}

#[test]
fn rules_parse_aliases_prefixes_and_reject_unsafe_specifiers() {
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
        ("read".to_string(), Some(Specifier::Path(".env".into())))
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
        "read(/etc/passwd)",
        "edit(../outside)",
    ] {
        assert!(parse_rule(invalid).is_err(), "{invalid}");
    }
    assert_eq!(
        canonical_rule("Bash(cargo test:*)").unwrap(),
        "run_terminal(cargo test *)"
    );
    assert_eq!(canonical_rule("read(./.env/)").unwrap(), "read(.env)");
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
        ("git rebase main", Some("git rebase main")),
        ("docker compose up", Some("docker compose up")),
        ("uv run python x.py", Some("uv run python x.py")),
        ("pnpm dlx cowsay", Some("pnpm dlx cowsay *")),
        ("./build.sh release", Some("./build.sh release")),
        ("FOO=1 cargo test", Some("FOO=1 cargo test")),
        // Review F3: glob characters would be saved as a prefix rule.
        ("sudo rm *", None),
        ("env *", None),
        ("ls src/[ab].rs", None),
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

/// Review F4: deny rules see through wrappers, assignments, quoting, program
/// paths and case.
#[test]
fn deny_beats_ask_beats_allow_and_is_hard_to_step_around() {
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
        "FOO=1 rm -rf x",
        "sudo -u root rm x",
        "env -i rm x",
        "timeout 5 rm x",
        "xargs rm < list",
        "RM -rf x",
        "r''m -rf x",
    ] {
        assert_eq!(
            effect_of(&set, &terminal(denied)),
            Some(PolicyEffect::Deny),
            "{denied}"
        );
    }
    for allowed in ["echo information", "cargo build --features rm"] {
        assert_ne!(
            effect_of(&set, &terminal(allowed)),
            Some(PolicyEffect::Deny),
            "{allowed}"
        );
    }
}

#[test]
fn path_globs_follow_gitignore_rules_and_ignore_case() {
    assert!(glob_matches(".env", ".env"));
    assert!(glob_matches(".env", "config/.env"));
    assert!(glob_matches("*.pem", "keys/server.pem"));
    assert!(glob_matches("src/**", "src/tools/mod.rs"));
    assert!(glob_matches("src", "src/main.rs"));
    assert!(glob_matches("src/**/*.rs", "src/a/b/c.rs"));
    assert!(glob_matches("secrets/**", "Secrets/key"));
    assert!(glob_matches("/src", "src"));
    assert!(!glob_matches("src/**", "tests/src.rs"));
    assert!(!glob_matches("/.env", "config/.env"));
    assert!(!glob_matches(".env", ".envrc"));
    assert!(directory_reaches("secrets/**", ""));
    assert!(directory_reaches("secrets/**", "secrets"));
    assert!(!directory_reaches("secrets/**", "src"));
    assert!(!directory_reaches(".env", ""));
}

/// Review F5: `read(...)` covers every read tool, searches that reach a
/// denied folder, absolute paths, `..` and symbolic links.
#[test]
fn read_rules_cover_every_read_tool_and_resolved_paths() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::create_dir_all(root.join("secrets")).unwrap();
    std::fs::write(root.join("secrets/key"), "k").unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("secrets"), root.join("src/link")).unwrap();
    let set = rules(&[
        (RuleEffect::Deny, "read(.env)"),
        (RuleEffect::Deny, "read(secrets/**)"),
    ]);
    let root_text = root.display().to_string();
    for (tool, path) in [
        ("read_file", ".env".to_string()),
        ("read_file", "./config/.env".to_string()),
        ("read_file", "a/../.env".to_string()),
        ("read_file", format!("{root_text}/.env")),
        ("read_file", "SECRETS/key".to_string()),
        ("grep", "secrets".to_string()),
        ("grep", ".".to_string()),
        ("list_directory", "secrets".to_string()),
    ] {
        assert_eq!(
            effect_in(&set, &call(tool, json!({ "path": path })), root),
            Some(PolicyEffect::Deny),
            "{tool} {path}"
        );
    }
    #[cfg(unix)]
    assert_eq!(
        effect_in(
            &set,
            &call("read_file", json!({ "path": "src/link/key" })),
            root
        ),
        Some(PolicyEffect::Deny)
    );
    for (tool, path) in [
        ("read_file", "README.md"),
        ("grep", "src"),
        ("list_directory", "."),
    ] {
        assert_eq!(
            effect_in(&set, &call(tool, json!({ "path": path })), root),
            None,
            "{tool} {path}"
        );
    }
}

/// Review F1 and F2: patch paths come from Git, so quoted names and
/// non-standard prefixes cannot hide a path from rules.
#[test]
fn patch_paths_come_from_git() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    assert_eq!(
        git_patch_paths(
            "--- \"a/\\056nib/config.toml\"\n+++ \"b/\\056nib/config.toml\"\n@@ -1 +1 @@\n-a\n+b\n",
            root
        )
        .unwrap(),
        vec![".nib/config.toml".to_string()]
    );
    assert_eq!(
        git_patch_paths(
            "--- src/AGENTS.md\n+++ src/AGENTS.md\n@@ -1 +1 @@\n-a\n+b\n",
            root
        )
        .unwrap(),
        vec!["AGENTS.md".to_string()]
    );
    assert!(git_patch_paths("not a patch", root).is_err());
    assert_eq!(
        parse_numstat(b"1\t1\tsrc/a.rs\0-\t-\tbin.dat\x000\t0\t\0old.rs\0new.rs\0"),
        vec!["bin.dat", "new.rs", "old.rs", "src/a.rs"]
    );

    let set = rules(&[(RuleEffect::Allow, "edit(src/**)")]);
    assert_eq!(
        effect_in(&set, &patch(&["src/main.rs"]), root),
        Some(PolicyEffect::Allow)
    );
    assert_eq!(
        effect_in(&set, &patch(&["src/main.rs", "README.md"]), root),
        None
    );
    let escape = call(
        "apply_patch",
        json!({"patch": "--- src/AGENTS.md\n+++ src/AGENTS.md\n@@ -1 +1 @@\n-a\n+b\n"}),
    );
    assert_eq!(effect_in(&set, &escape, root), None);
}

#[test]
fn layers_load_from_nib_home_and_trust_is_per_project() {
    let home = TestNibHome::isolated();
    let project = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    write_config(
        &home,
        "[llm]\nignored = true\n\n[permissions]\nallow = [\"terminal(task check *)\"]\ndeny = [\"read(.env)\", \"terminal(bad\"]\n",
    );

    let set = RuleSet::load(project.path());
    assert!(!set.trusted && !set.unreadable);
    assert_eq!(set.rules.len(), 2, "{:?}", set.rules);
    assert_eq!(set.problems.len(), 1, "{:?}", set.problems);

    assert!(add_project_rule(project.path(), RuleEffect::Allow, "Bash(cargo test:*)").unwrap());
    assert!(
        !add_project_rule(project.path(), RuleEffect::Allow, "terminal(cargo test *)").unwrap()
    );
    assert!(add_project_rule(project.path(), RuleEffect::Deny, "terminal(rm").is_err());
    assert!(set_project_trust(project.path(), true).unwrap());
    assert!(!set_project_trust(project.path(), true).unwrap());

    let set = RuleSet::load(project.path());
    assert!(set.trusted && project_trusted(project.path()));
    assert!(set
        .rules
        .iter()
        .any(|rule| rule.layer == RuleLayer::ProjectLocal
            && rule.text == "run_terminal(cargo test *)"));
    assert_eq!(
        set.evaluate(&terminal("cargo test"), &[project.path()])
            .map(|rule| rule.effect),
        Some(PolicyEffect::Allow)
    );

    let unrelated = RuleSet::load(other.path());
    assert!(!unrelated.trusted && !project_trusted(other.path()));
    assert_eq!(unrelated.rules.len(), 2);

    assert!(remove_project_rule(project.path(), "terminal(cargo test:*)").unwrap());
    assert!(!remove_project_rule(project.path(), "terminal(cargo test *)").unwrap());
    assert!(set_project_trust(project.path(), false).unwrap());
    let saved = std::fs::read_to_string(home.directory.path().join(PERMISSIONS_FILE)).unwrap();
    assert!(
        !saved.contains(&project.path().display().to_string()),
        "{saved}"
    );
}

/// Review F11: an unreadable rules file is never overwritten, and every
/// action asks while it cannot be read.
#[test]
fn a_damaged_rules_file_fails_closed_and_is_never_overwritten() {
    let home = TestNibHome::isolated();
    let project = tempfile::tempdir().unwrap();
    let path = home.directory.path().join(PERMISSIONS_FILE);
    std::fs::write(&path, "{not json").unwrap();
    let set = RuleSet::load(project.path());
    assert!(!set.trusted && set.unreadable);
    assert_eq!(set.problems.len(), 1);
    assert_eq!(
        set.evaluate(&call("read_file", json!({"path": "a"})), &[project.path()])
            .map(|rule| rule.effect),
        Some(PolicyEffect::RequireApproval)
    );
    assert!(set_project_trust(project.path(), true).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");

    std::fs::remove_file(&path).unwrap();
    write_config(&home, "[permissions\n");
    assert!(RuleSet::load(project.path()).unreadable);
}

/// Review F12: settings for folders that no longer exist are forgotten.
#[test]
fn saving_forgets_missing_projects() {
    let home = TestNibHome::isolated();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        home.directory.path().join(PERMISSIONS_FILE),
        r#"{"version":1,"projects":{"/no/such/project":{"trusted":true}}}"#,
    )
    .unwrap();
    set_project_trust(project.path(), true).unwrap();
    let saved = std::fs::read_to_string(home.directory.path().join(PERMISSIONS_FILE)).unwrap();
    assert!(!saved.contains("/no/such/project"), "{saved}");
}

fn git(cwd: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Review F8: session and subagent worktrees share their project's trust and
/// rules; a copied `.git` pointer without Git's back-link does not.
#[test]
fn linked_worktrees_use_their_main_project_settings() {
    let _home = TestNibHome::isolated();
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("project");
    std::fs::create_dir(&project).unwrap();
    git(&project, &["init", "-q"]);
    git(
        &project,
        &[
            "-c",
            "user.name=f",
            "-c",
            "user.email=f@example.invalid",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "start",
        ],
    );
    let worktree = directory.path().join("worktree");
    git(
        &project,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            worktree.to_str().unwrap(),
        ],
    );
    set_project_trust(&project, true).unwrap();
    assert!(project_trusted(&worktree));

    let impostor = directory.path().join("impostor");
    std::fs::create_dir(&impostor).unwrap();
    std::fs::copy(worktree.join(".git"), impostor.join(".git")).unwrap();
    assert!(!project_trusted(&impostor));
}
