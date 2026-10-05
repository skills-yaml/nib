#[path = "support/workspace_governance/mod.rs"]
mod governance;

use serde_json::json;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn write(root: &Path, name: &str, text: &str) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("parents");
    std::fs::write(path, text).expect("fixture");
}

fn fixture() -> TempDir {
    let root = tempfile::tempdir().expect("fixture");
    for state in governance::STATES {
        std::fs::create_dir_all(root.path().join("workspace/specs").join(state)).expect("state");
    }
    write(
        root.path(),
        "Cargo.toml",
        "[package]\nname = 'example'\nversion = '0.1.0'\n",
    );
    write(root.path(), "workspace/specs/development/example/T001_example.md", "# T001: Example\n\nState: development\nPrimary Feature: example\n\n## Scope\nExample.\n## Acceptance Criteria\nExample.\n## Affected Areas\nExample.\n## Validation Gates\nExample.\n\n## Version Impact\n\n| Component | Impact | Release | Rationale |\n| --- | --- | --- | --- |\n| example | patch | example-next | Compatible fix. |\n\n## Memory Impact\n\nStatus: none\nRationale: No new durable context.\n");
    write(root.path(), "workspace/specs/README.md", "# Specs\n\n## Primary Features\n\n- `example`\n\n## Status Catalog\n\n<!-- SPEC-CATALOG:START -->\n| Spec | Primary feature | State | Status rationale |\n| --- | --- | --- | --- |\n| [T001](development/example/T001_example.md) | example | development | Local work. |\n<!-- SPEC-CATALOG:END -->\n");
    let ledger = json!({"schema_version":1,"historical_specs":[],"releases":[{
        "id":"example-next","component":"example","baseline":"0.1.0","target":"0.1.1",
        "impact":"patch","timing":"merge","status":"planned","owner":"example-owner",
        "version_source":"Cargo.toml","specs":["workspace/specs/development/example/T001_example.md"],
        "evidence":"Reserve compatible fix at merge."
    }]});
    write(root.path(), "workspace/releases.json", &ledger.to_string());
    root
}

#[test]
fn current_repository_satisfies_every_workspace_module() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    governance::validate(root, "all").expect("current repository");
    assert!(governance::validate(root, "unknown").is_err());
}

#[test]
fn forced_crlf_checkout_preserves_full_governance_and_raw_integrity() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"));
    let checkout = tempfile::tempdir().expect("checkout fixture");
    let shallow_source = tempfile::tempdir().expect("shallow source fixture");
    let git = |root: &Path, args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_CONFIG")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_ATTR_SOURCE")
            .env_remove("GIT_SHALLOW_FILE")
            .env_remove("GIT_NAMESPACE")
            .output()
            .expect("Git fixture command");
        assert!(
            output.status.success(),
            "Git fixture {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    let revision = String::from_utf8(git(source, &["rev-parse", "HEAD"])).expect("source revision");
    git(shallow_source.path(), &["init", "--quiet"]);
    git(
        shallow_source.path(),
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            "--depth=1",
            "--update-shallow",
            source.to_str().expect("source path"),
            revision.trim(),
        ],
    );
    git(
        shallow_source.path(),
        &["checkout", "--quiet", "--detach", revision.trim()],
    );
    assert_eq!(
        git(
            shallow_source.path(),
            &["rev-parse", "--is-shallow-repository"]
        ),
        b"true\n"
    );
    git(checkout.path(), &["init", "--quiet"]);
    git(checkout.path(), &["config", "core.autocrlf", "true"]);
    git(checkout.path(), &["config", "core.eol", "crlf"]);
    git(
        checkout.path(),
        &[
            "config",
            "core.symlinks",
            if cfg!(windows) { "false" } else { "true" },
        ],
    );
    git(
        checkout.path(),
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            "--update-shallow",
            shallow_source.path().to_str().expect("shallow source path"),
            revision.trim(),
        ],
    );
    git(
        checkout.path(),
        &["checkout", "--quiet", "--detach", revision.trim()],
    );
    assert_eq!(
        git(checkout.path(), &["rev-parse", "HEAD"]),
        revision.as_bytes()
    );

    let control = std::fs::read(checkout.path().join("README.md")).expect("ordinary text");
    assert!(
        control.windows(2).any(|pair| pair == b"\r\n"),
        "ordinary text must exercise CRLF conversion"
    );
    governance::validate(checkout.path(), "all").expect("forced-CRLF full governance");

    let package_file = "workspace/instructions/standards/workspace-docs/AGENT_MIGRATION.md";
    let path = checkout.path().join(package_file);
    let original = std::fs::read(&path).expect("upstream bytes");
    let mut changed = original.clone();
    changed.push(b'\n');
    std::fs::write(&path, changed).expect("tamper upstream bytes");
    assert_eq!(
        governance::validate(checkout.path(), "structure")
            .expect_err("raw integrity must reject changed bytes"),
        "modified source package: AGENT_MIGRATION.md"
    );
    std::fs::write(path, original).expect("restore upstream bytes");

    write(
        checkout.path(),
        ".gitattributes",
        "# Fixture removes checkout protection.\n",
    );
    for name in ["AGENTS.md", package_file] {
        std::fs::remove_file(checkout.path().join(name)).expect("remove protected checkout");
    }
    git(
        checkout.path(),
        &["checkout-index", "--force", "--", "AGENTS.md", package_file],
    );
    let unprotected = std::fs::read(checkout.path().join(package_file)).expect("unprotected bytes");
    assert!(unprotected.windows(2).any(|pair| pair == b"\r\n"));
    assert!(
        governance::validate(checkout.path(), "all").is_err(),
        "unprotected CRLF checkout must fail governance"
    );
}

#[test]
fn catalog_rejects_duplicates_missing_features_and_orphan_rows() {
    for replacement in [
        " | unknown | development |",
        " | example | done |",
        " | example | development |  |",
    ] {
        let root = fixture();
        governance::validate(root.path(), "catalog").expect("valid catalog");
        let path = root.path().join("workspace/specs/README.md");
        let original = std::fs::read_to_string(&path).expect("catalog");
        let changed = if replacement.ends_with("|  |") {
            original.replace(" | example | development | Local work. |", replacement)
        } else {
            original.replace(" | example | development |", replacement)
        };
        std::fs::write(path, changed).expect("change");
        assert!(governance::validate(root.path(), "catalog").is_err());
    }
    let root = fixture();
    let path = root.path().join("workspace/specs/README.md");
    let original = std::fs::read_to_string(&path).expect("catalog");
    let row = original
        .lines()
        .find(|line| line.starts_with("| [T001]"))
        .expect("row");
    std::fs::write(&path, original.replace(row, &format!("{row}\n{row}"))).expect("duplicate");
    assert!(governance::validate(root.path(), "catalog").is_err());
}

#[test]
fn memory_rejects_missing_pending_done_and_unlinked_updates() {
    let none = "## Memory Impact\n\nStatus: none\nRationale: No new durable context.\n";
    governance::validate_memory_text(none, "done").expect("none");
    assert!(governance::validate_memory_text("", "development").is_err());
    assert!(governance::validate_memory_text(&none.replace("none", "pending"), "done").is_err());
    governance::validate_memory_text(&none.replace("none", "pending"), "development")
        .expect("active pending");
    assert!(
        governance::validate_memory_text(&none.replace("none", "updated"), "development").is_err()
    );
    let linked = none.replace("none", "updated").replace("No new durable context.", "Decision in workspace/agents/memory/decisions.md and workspace/agents/memory/changelog.md.");
    governance::validate_memory_text(&linked, "done").expect("linked updated");
    assert!(governance::validate_memory_text(&format!("{none}\n{none}"), "done").is_err());
}

#[test]
fn blocked_requires_resumable_previous_stage_and_reason() {
    let valid = "Previous State: development\nBlock Kind: impediment\nBlock Reason: Dependency unavailable.\nResume Condition: Dependency restored.\n";
    governance::validate_blocked_text(valid).expect("blocked");
    for changed in [
        valid.replace("development", "done"),
        valid.replace("impediment", "paused"),
        valid.replace("Resume Condition: Dependency restored.\n", ""),
    ] {
        assert!(governance::validate_blocked_text(&changed).is_err());
    }
}

#[test]
fn versions_reject_collisions_bad_arithmetic_and_native_version_drift() {
    let root = fixture();
    governance::validate(root.path(), "versions").expect("valid reservation");
    let path = root.path().join("workspace/releases.json");
    let original: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("ledger")).expect("json");
    for (key, value) in [
        ("target", "0.1.2"),
        ("target", "0.01.1"),
        ("status", "applied"),
        ("timing", "development-start"),
    ] {
        let mut changed = original.clone();
        changed["releases"][0][key] = json!(value);
        std::fs::write(&path, changed.to_string()).expect("change");
        assert!(
            governance::validate(root.path(), "versions").is_err(),
            "{key}: {value}"
        );
    }
    let mut changed = original.clone();
    let mut collision = changed["releases"][0].clone();
    collision["id"] = json!("independent-release");
    changed["releases"]
        .as_array_mut()
        .expect("releases")
        .push(collision);
    std::fs::write(&path, changed.to_string()).expect("collision");
    assert!(governance::validate(root.path(), "versions").is_err());
    std::fs::write(&path, original.to_string()).expect("restore");
    write(root.path(), "Cargo.toml", "[package]\nversion = '0.2.0'\n");
    assert!(governance::validate(root.path(), "versions").is_err());
}

#[test]
fn integration_rejects_unapplied_version_and_missing_evidence() {
    let root = fixture();
    let old = root
        .path()
        .join("workspace/specs/development/example/T001_example.md");
    let text = std::fs::read_to_string(&old).expect("spec");
    std::fs::remove_file(old).expect("move");
    write(
        root.path(),
        "workspace/specs/test/example/T001_example.md",
        &text.replace("State: development", "State: test"),
    );
    let path = root.path().join("workspace/specs/README.md");
    let catalog = std::fs::read_to_string(&path).expect("catalog");
    std::fs::write(
        path,
        catalog
            .replace("development/example", "test/example")
            .replace(" | development |", " | test |"),
    )
    .expect("catalog move");
    assert!(governance::validate(root.path(), "catalog").is_err());
    let path = root.path().join("workspace/releases.json");
    let ledger = std::fs::read_to_string(&path).expect("ledger");
    std::fs::write(path, ledger.replace("development/example", "test/example"))
        .expect("member move");
    assert!(governance::validate(root.path(), "versions").is_err());
}

#[test]
fn coordination_rejects_missing_sections_and_escaping_scopes() {
    let mut valid = "---\nschema_version: 1\ncoordination_id: sample-task\nbase_revision: \"0123456789012345678901234567890123456789\"\nagent_id: reviewer\nrole: implementer\nstatus: active\ntask_ref: detached\nbranch_authorization: none\nscope:\n  - src/context\n  - tests\nupdated_at: 2026-10-02T00:00:00Z\n---\n".to_owned();
    for heading in [
        "Assignment",
        "Actions",
        "Validation",
        "Blockers and Dependencies",
        "Next Step",
        "Handoff",
    ] {
        valid.push_str(&format!("\n## {heading}\n\nCurrent evidence.\n"));
    }
    governance::validate_coordination_text(&valid).expect("WIP");
    assert!(
        governance::validate_coordination_text(&valid.replace("src/context", "../outside"))
            .is_err()
    );
    assert!(
        governance::validate_coordination_text(&valid.replace("## Handoff", "## Missing")).is_err()
    );
    assert!(!governance::safe_relative("/absolute"));
}
