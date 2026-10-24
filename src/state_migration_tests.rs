use super::*;
use crate::paths::TestNibHome;

/// A project laid out the way versions before T082 left it: runtime state,
/// a profile with sessions and state, a legacy worktree and preferences.
struct LegacyProject {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl LegacyProject {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("project");
        let root = directory.path().canonicalize().expect("canonical project");
        let project = Self {
            _directory: directory,
            root,
        };
        for (path, content) in [
            ("command-prefixes.json", "{\"rules\":[]}"),
            ("memory.json", "{\"user\":{\"legacy\":\"memory\"}}"),
            ("plans/plan-1.md", "# Plan"),
            ("subagents/sub-1.json", "{\"id\":\"sub-1\"}"),
            ("subagents/.legacy-lock-migration-v1.json", "{}"),
            ("worktree-ownership/session-a.json", "{\"kind\":\"session\"}"),
            ("daemons/audit.jsonl", "{\"event\":1}\n"),
            ("profiles/default/memory.json", "{\"user\":{\"style\":\"terse\"}}"),
            ("profiles/default/context.json", "{\"notes\":[]}"),
            ("profiles/default/daemons/cron.json", "{\"jobs\":[]}"),
            ("profiles/default/managed-skills/tidy/SKILL.md", "# Tidy"),
            (
                "profiles/default/.legacy-state-migration-v1.json",
                "{\"version\":1,\"profile_id\":\"default\",\"session_count\":1,\"memory_copied\":true}",
            ),
            ("profiles/default/sessions/s-1.json", "{\"id\":\"s-1\"}"),
            ("profiles/default/checkpoints/s-1/index.json", "{}"),
            ("worktrees/sessions/old-1/README.md", "legacy worktree"),
            ("config.toml", "[llm]\nprovider = \"mock\"\n"),
            ("skills/review/SKILL.md", "# Review"),
        ] {
            project.write(path, content);
        }
        fs::create_dir_all(project.legacy().join("process-scopes")).expect("scopes");
        fs::create_dir_all(project.legacy().join("subagent-owner-leases")).expect("leases");
        project
    }

    fn legacy(&self) -> PathBuf {
        NibPaths::project_dir(&self.root)
    }

    fn state(&self) -> PathBuf {
        NibPaths::user_state_dir(&self.root).expect("state directory")
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.legacy().join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("parent directory");
        fs::write(path, content).expect("legacy file");
    }

    fn migrate(&self) -> Result<StateMigration, String> {
        migrate_project_state(&self.root, &[], &["default".to_string()])
    }
}

/// Every file below `directory`, relative to it, with its bytes; a
/// symbolic link is listed, not followed.
fn files_below(directory: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(current) = pending.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.map(|entry| entry.expect("entry")) {
            let path = entry.path();
            let kind = entry.file_type().expect("file type");
            let relative = path
                .strip_prefix(directory)
                .expect("relative")
                .to_path_buf();
            if kind.is_symlink() {
                files.push((relative, b"symbolic link".to_vec()));
            } else if kind.is_dir() {
                pending.push(path);
            } else {
                files.push((relative, fs::read(&path).expect("file")));
            }
        }
    }
    files.sort();
    files
}

/// The legacy files the migration moves: everything but sessions and what
/// belongs to them, legacy worktrees and preferences.
fn movable_files(legacy: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    files_below(legacy)
        .into_iter()
        .filter(|(path, _)| {
            let text = path.to_string_lossy();
            !text.starts_with("worktrees")
                && !text.starts_with("config.toml")
                && !text.starts_with("skills")
                && !text.contains("/sessions/")
                && !text.contains("/checkpoints/")
        })
        .collect()
}

/// The files of a state directory, without the migration's own records.
fn state_files(state: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    files_below(state)
        .into_iter()
        .filter(|(path, _)| {
            ![
                STATE_RECORD_FILE,
                PROJECT_IDENTITY_FILE,
                MIGRATION_LOCK_FILE,
            ]
            .iter()
            .any(|name| path == Path::new(name))
                && !path.to_string_lossy().starts_with(".nib-lock-")
        })
        .collect()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

#[test]
fn a_populated_legacy_layout_moves_to_the_user_home_without_loss() {
    let _home = TestNibHome::isolated();
    let project = LegacyProject::new();
    let before = movable_files(&project.legacy());
    #[cfg(unix)]
    let subagents_inode = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(project.legacy().join("subagents"))
            .expect("subagents")
            .ino()
    };
    assert_eq!(NibPaths::project_state(&project.root), project.legacy());

    let outcome = project.migrate().expect("migration");

    let StateMigration::Moved(moved) = outcome else {
        panic!("expected a move, got {outcome:?}");
    };
    assert!(moved.contains(&"subagents".to_string()), "{moved:?}");
    assert!(moved.contains(&"profiles/default/memory.json".to_string()));
    assert_eq!(NibPaths::project_state(&project.root), project.state());
    // Every byte arrived, under the same relative path.
    assert_eq!(state_files(&project.state()), before);
    // A rename keeps the directory identity subagent receipts are bound to.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let moved_inode = fs::metadata(project.state().join("subagents"))
            .expect("moved subagents")
            .ino();
        assert_eq!(moved_inode, subagents_inode);
    }
    // Sessions, legacy worktrees and preferences stay in the project.
    for kept in [
        "profiles/default/sessions/s-1.json",
        "profiles/default/checkpoints/s-1/index.json",
        "worktrees/sessions/old-1/README.md",
        "config.toml",
        "skills/review/SKILL.md",
    ] {
        assert!(project.legacy().join(kept).is_file(), "{kept} stays");
    }
    for entry in LEGACY_STATE_ENTRIES {
        assert!(!project.legacy().join(entry).exists(), "{entry} moved");
    }
    let record = read_record(&project.state().join(STATE_RECORD_FILE))
        .expect("record")
        .expect("present");
    assert_eq!(record.status, "done");
    assert_eq!(record.root, project.root);
}

#[test]
fn a_second_run_changes_nothing() {
    let _home = TestNibHome::isolated();
    let project = LegacyProject::new();
    project.migrate().expect("first migration");
    let state = files_below(&project.state());
    let legacy = files_below(&project.legacy());

    assert_eq!(project.migrate(), Ok(StateMigration::Current));

    assert_eq!(files_below(&project.state()), state);
    assert_eq!(files_below(&project.legacy()), legacy);
}

#[test]
fn an_interrupted_migration_resumes_and_keeps_identical_leftovers() {
    let _home = TestNibHome::isolated();
    let project = LegacyProject::new();
    let state = project.state();
    ensure_state_directory(&state).expect("state");
    // A run stopped after recording the move and moving `plans`; the
    // remembered prefixes exist on both sides with the same bytes.
    write_record(
        &state.join(STATE_RECORD_FILE),
        &new_record(&project.root, "moving"),
    )
    .expect("record");
    fs::rename(project.legacy().join("plans"), state.join("plans")).expect("moved plans");
    fs::copy(
        project.legacy().join("command-prefixes.json"),
        state.join("command-prefixes.json"),
    )
    .expect("copied prefixes");
    assert_eq!(NibPaths::project_state(&project.root), state);

    let outcome = project.migrate().expect("resumed migration");

    assert!(matches!(outcome, StateMigration::Moved(_)), "{outcome:?}");
    assert_eq!(
        fs::read_to_string(state.join("plans/plan-1.md")).unwrap(),
        "# Plan"
    );
    assert!(state.join("subagents/sub-1.json").is_file());
    let record = read_record(&state.join(STATE_RECORD_FILE))
        .unwrap()
        .unwrap();
    assert_eq!(record.status, "done");
    // The legacy duplicate is kept aside, not deleted.
    assert_eq!(record.kept.len(), 1);
    assert!(record.kept[0].starts_with(project.legacy()));
    assert_eq!(
        fs::read_to_string(record.kept[0].join("command-prefixes.json")).unwrap(),
        "{\"rules\":[]}"
    );
    assert!(!project.legacy().join("command-prefixes.json").exists());
}

#[test]
fn a_differing_destination_stops_the_first_migration_before_any_change() {
    let _home = TestNibHome::isolated();
    let project = LegacyProject::new();
    let state = project.state();
    ensure_state_directory(&state).expect("state");
    fs::write(
        state.join("memory.json"),
        "{\"user\":{\"other\":\"value\"}}",
    )
    .expect("memory");
    let legacy = files_below(&project.legacy());

    let error = project.migrate().expect_err("conflict");

    assert!(error.contains("differs"), "{error}");
    assert!(error.contains("memory.json"), "{error}");
    assert_eq!(files_below(&project.legacy()), legacy);
    assert_eq!(
        fs::read_to_string(state.join("memory.json")).unwrap(),
        "{\"user\":{\"other\":\"value\"}}"
    );
    assert!(!state.join(STATE_RECORD_FILE).exists());
    assert_eq!(NibPaths::project_state(&project.root), project.legacy());
}

#[test]
fn state_in_use_stays_in_the_project_until_it_is_idle() {
    let _home = TestNibHome::isolated();
    let project = LegacyProject::new();
    let lease = project.legacy().join("subagent-owner-leases/owner.lease");
    fs::write(&lease, "{}").expect("lease");

    let outcome = project.migrate().expect("deferred migration");

    let StateMigration::Deferred(reason) = outcome else {
        panic!("expected a deferral, got {outcome:?}");
    };
    assert!(reason.contains("running subagents"), "{reason}");
    assert_eq!(NibPaths::project_state(&project.root), project.legacy());
    assert!(project.legacy().join("subagents/sub-1.json").is_file());

    fs::remove_file(&lease).expect("lease released");
    let heartbeat = project.legacy().join("profiles/default/presence/s-1.json");
    fs::create_dir_all(heartbeat.parent().unwrap()).unwrap();
    fs::write(
        &heartbeat,
        format!(
            "{{\"session_id\":\"s-1\",\"folder\":\"/w\",\"updated_at\":{}}}",
            unix_now()
        ),
    )
    .unwrap();
    assert!(matches!(
        project.migrate(),
        Ok(StateMigration::Deferred(reason)) if reason.contains("session is running")
    ));

    // A heartbeat that stopped long ago no longer counts.
    fs::write(
        &heartbeat,
        "{\"session_id\":\"s-1\",\"folder\":\"/w\",\"updated_at\":1}",
    )
    .unwrap();
    assert!(matches!(project.migrate(), Ok(StateMigration::Moved(_))));
    assert_eq!(NibPaths::project_state(&project.root), project.state());
}

#[test]
fn configured_state_directories_are_never_moved() {
    let _home = TestNibHome::isolated();
    let project = LegacyProject::new();
    let configured = project.legacy().join("profiles/default");

    assert!(matches!(
        migrate_project_state(&project.root, std::slice::from_ref(&configured), &[]),
        Ok(StateMigration::Moved(_))
    ));

    assert!(configured.join("memory.json").is_file());
    assert!(configured.join("daemons/cron.json").is_file());
    assert!(!project.state().join("profiles").exists());
    assert!(project.state().join("subagents/sub-1.json").is_file());
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_in_legacy_state_stops_the_migration_before_any_change() {
    let _home = TestNibHome::isolated();
    let project = LegacyProject::new();
    let target = tempfile::tempdir().expect("target");
    fs::remove_dir_all(project.legacy().join("plans")).unwrap();
    std::os::unix::fs::symlink(target.path(), project.legacy().join("plans")).unwrap();
    let before = files_below(&project.legacy());

    let error = project.migrate().expect_err("symbolic link");

    assert!(error.contains("symbolic link"), "{error}");
    assert_eq!(files_below(&project.legacy()), before);
    assert!(!project.state().join(STATE_RECORD_FILE).exists());
}

#[test]
fn older_versions_find_a_state_moved_marker_and_stop() {
    let _home = TestNibHome::isolated();
    let project = LegacyProject::new();
    project.migrate().expect("migration");
    let marker = project
        .legacy()
        .join("profiles/default/.legacy-state-migration-v1.json");

    // The profile migration of older versions parses this marker, requires
    // version 1 and stops on anything else, before it touches any state.
    let parsed: serde_json::Value =
        serde_json::from_slice(&fs::read(&marker).expect("marker")).expect("json");
    assert_eq!(parsed["version"], 2);
    assert_eq!(parsed["profile_id"], "default");
    assert_eq!(parsed["session_count"], 0);
    assert_eq!(parsed["memory_copied"], false);
    assert_eq!(parsed["state_moved_to"].as_str(), project.state().to_str());
    assert!(is_moved_marker(&marker));
    // The marker itself is never taken for legacy state.
    assert_eq!(project.migrate(), Ok(StateMigration::Current));

    // State an older version writes after the move is left where it is, and
    // the project keeps using the user home.
    project.write("memory.json", "{\"user\":{\"from\":\"old nib\"}}");
    assert_eq!(NibPaths::project_state(&project.root), project.state());
    assert!(matches!(
        project.migrate(),
        Ok(StateMigration::Deferred(reason)) if reason.contains("memory.json")
    ));
    assert_eq!(
        fs::read_to_string(project.legacy().join("memory.json")).unwrap(),
        "{\"user\":{\"from\":\"old nib\"}}"
    );
    assert_eq!(
        fs::read_to_string(project.state().join("memory.json")).unwrap(),
        "{\"user\":{\"legacy\":\"memory\"}}"
    );
    // A reappeared entry the user home lacks still moves.
    fs::remove_dir_all(project.state().join("worktree-ownership")).unwrap();
    project.write("worktree-ownership/session-b.json", "{}");
    assert!(matches!(project.migrate(), Ok(StateMigration::Moved(_))));
    assert!(project
        .state()
        .join("worktree-ownership/session-b.json")
        .is_file());
}

#[test]
fn a_fresh_project_records_its_state_in_the_user_home() {
    let home = TestNibHome::isolated();
    let directory = tempfile::tempdir().expect("project");
    let root = directory.path().canonicalize().unwrap();

    assert_eq!(
        migrate_project_state(&root, &[], &["default".to_string()]),
        Ok(StateMigration::Current)
    );

    let state = NibPaths::user_state_dir(&root).unwrap();
    assert!(state.starts_with(home.directory.path()));
    assert_eq!(NibPaths::project_state(&root), state);
    assert!(state.join(STATE_RECORD_FILE).is_file());
    assert!(state.join(PROJECT_IDENTITY_FILE).is_file());
    assert!(!NibPaths::project_dir(&root).exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for directory in [home.directory.path().join("projects"), state.clone()] {
            let mode = fs::metadata(&directory).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", directory.display());
        }
        let mode = fs::metadata(state.join(STATE_RECORD_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn managed_worktrees_keep_their_state_inside_themselves() {
    let home = TestNibHome::isolated();
    let project = tempfile::tempdir().expect("project");
    let root = project.path().canonicalize().unwrap();
    let state = NibPaths::user_state_dir(&root).unwrap();
    let worktree = state.join("worktrees/subagents/sub-1");
    fs::create_dir_all(&worktree).unwrap();
    let legacy_worktree = NibPaths::project_dir(&root).join("worktrees/sessions/s-1");
    fs::create_dir_all(&legacy_worktree).unwrap();
    // Without an identity record the worktree's project is unknown.
    assert_eq!(NibPaths::managed_worktree_project(&worktree), None);
    record_identity(&state, &root).unwrap();

    for worktree in [&worktree, &legacy_worktree] {
        assert!(NibPaths::is_managed_worktree(worktree));
        assert_eq!(
            NibPaths::project_state(worktree),
            NibPaths::project_dir(worktree)
        );
        assert_eq!(
            migrate_project_state(worktree, &[], &[]),
            Ok(StateMigration::NotApplicable)
        );
        assert_eq!(
            NibPaths::managed_worktree_project(worktree),
            Some(root.clone())
        );
    }
    assert!(NibPaths::subagent_worktrees(&root).starts_with(home.directory.path()));
    assert_eq!(
        NibPaths::subagent_worktree_roots(&root),
        vec![
            state.join("worktrees/subagents"),
            NibPaths::project_dir(&root).join("worktrees/subagents")
        ]
    );
    // A recorded root that does not own the key is not accepted.
    let other = tempfile::tempdir().unwrap();
    fs::write(
        state.join(PROJECT_IDENTITY_FILE),
        format!(
            "version = 1\nroot = \"{}\"\n",
            other.path().canonicalize().unwrap().display()
        ),
    )
    .unwrap();
    assert_eq!(NibPaths::managed_worktree_project(&worktree), None);
}
