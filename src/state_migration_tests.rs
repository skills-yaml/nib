use super::*;
use crate::paths::TestNibHome;
use sha2::Digest;
use std::cell::Cell;

thread_local! {
    /// Runs once after a session directory's import, before its rename.
    pub(super) static AFTER_IMPORT: Cell<Option<fn(&Path)>> = const { Cell::new(None) };
    /// Stops the run once after a session directory's rename, as an
    /// interruption would.
    pub(super) static STOP_AFTER_IMPORT_RENAME: Cell<bool> = const { Cell::new(false) };
}

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
            ("profiles/default/sessions/s-1.json", &legacy_session("s-1", "first")),
            ("profiles/default/sessions/s-2.json", &legacy_session("s-2", "second")),
            ("profiles/default/sessions/broken.json", "{\"id\": \"broken\", "),
            ("profiles/default/sessions/.session-directory.identity", ""),
            ("profiles/default/sessions/.session-lock-07.lock", ""),
            ("profiles/default/sessions/.session-run-s-1.lock", ""),
            (
                "profiles/default/.nib-lock-8-sessions-.session-directory.identity.anchor",
                "",
            ),
            ("profiles/default/checkpoints/s-1/index.json", "{}"),
            ("profiles/default/file_reads/s-1.json", "{\"files\":[]}"),
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

/// A session file as older versions wrote it, with messages and an event.
fn legacy_session(id: &str, text: &str) -> String {
    let mut session = crate::session::Session::new(id.to_string());
    session.revision = 3;
    session.summary = Some(format!("summary of {text}"));
    for (index, (role, content)) in [("user", text), ("assistant", "done")].iter().enumerate() {
        session.messages.push(crate::session::SessionMessage {
            index,
            role: (*role).to_string(),
            content: (*content).to_string(),
            timestamp: None,
            attachments: Vec::new(),
        });
    }
    session.events.push(crate::session::SessionEvent {
        index: 0,
        kind: "run_started".to_string(),
        details: serde_json::json!({"text": text, "duration": 1.551_978_736_000_000_1_f64}),
        timestamp: None,
    });
    serde_json::to_string_pretty(&session).expect("legacy session")
}

/// The sessions the database holds for a project's default profile.
fn database_sessions(state: &Path) -> Vec<crate::session::Session> {
    let key = state
        .file_name()
        .and_then(|name| name.to_str())
        .expect("key");
    let store = crate::session::SessionStore::at_dir(
        NibPaths::database_sessions_dir(key, "default").expect("sessions dir"),
    );
    store
        .list()
        .iter()
        .map(|id| store.load(id).expect("session"))
        .collect()
}

/// The `legacy-<time>` directories of a project's `.nib`.
fn legacy_copies(legacy: &Path) -> Vec<PathBuf> {
    let mut copies: Vec<PathBuf> = fs::read_dir(legacy)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("legacy-"))
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_default();
    copies.sort();
    copies
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

/// The legacy files the migration moves: everything but the session
/// directories (imported, then kept as a legacy copy) with their lock
/// anchors, legacy worktrees and preferences.
fn movable_files(legacy: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    files_below(legacy)
        .into_iter()
        .filter(|(path, _)| {
            let text = path.to_string_lossy();
            !text.starts_with("worktrees")
                && !text.starts_with("config.toml")
                && !text.starts_with("skills")
                && !text.contains("/sessions/")
                && !text.contains("/.nib-lock-8-sessions-")
        })
        .collect()
}

/// The session directories and their lock anchors, which the migration
/// keeps as a legacy copy once their sessions are in the database.
fn session_files(legacy: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    files_below(legacy)
        .into_iter()
        .filter(|(path, _)| {
            let text = path.to_string_lossy();
            text.starts_with("profiles/")
                && (text.contains("/sessions/") || text.contains("/.nib-lock-8-sessions-"))
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
    let sessions_before = session_files(&project.legacy());
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
    // Every readable session is in the database with the same content.
    let imported = database_sessions(&project.state());
    let expected: Vec<crate::session::Session> = ["s-1", "s-2"]
        .iter()
        .map(|id| {
            let original = sessions_before
                .iter()
                .find(|(path, _)| path.ends_with(format!("{id}.json")))
                .expect("original session");
            serde_json::from_slice(&original.1).expect("original session JSON")
        })
        .collect();
    assert_eq!(imported, expected);
    assert!(moved
        .iter()
        .any(|entry| entry.starts_with("profiles/default/sessions")));
    // The original session directory, with the file nib could not read, its
    // locks and identity marker, is kept byte for byte in a legacy copy.
    assert!(!project.legacy().join("profiles/default/sessions").exists());
    let copies = legacy_copies(&project.legacy());
    assert_eq!(copies.len(), 1, "{copies:?}");
    let kept: Vec<(PathBuf, Vec<u8>)> = files_below(&copies[0].join("sessions"));
    assert_eq!(kept, sessions_before);
    // Legacy worktrees and preferences stay in the project.
    for kept in [
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
    let sessions = database_sessions(&project.state());
    let state = files_below(&project.state());
    let legacy = files_below(&project.legacy());

    assert_eq!(project.migrate(), Ok(StateMigration::Current));

    assert_eq!(files_below(&project.state()), state);
    assert_eq!(files_below(&project.legacy()), legacy);
    assert_eq!(database_sessions(&project.state()), sessions);
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
    let duplicates: Vec<&PathBuf> = record
        .kept
        .iter()
        .filter(|kept| kept.ends_with("state"))
        .collect();
    assert_eq!(duplicates.len(), 1);
    assert!(duplicates[0].starts_with(project.legacy()));
    assert_eq!(
        fs::read_to_string(duplicates[0].join("command-prefixes.json")).unwrap(),
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
    let _home = TestNibHome::isolated();
    let directory = tempfile::tempdir().expect("project");
    let root = directory.path().canonicalize().unwrap();

    assert_eq!(
        migrate_project_state(&root, &[], &["default".to_string()]),
        Ok(StateMigration::Current)
    );

    let state = NibPaths::user_state_dir(&root).unwrap();
    let home = NibPaths::state_home().unwrap().canonicalize().unwrap();
    assert!(state.starts_with(&home));
    assert_eq!(NibPaths::project_state(&root), state);
    assert!(state.join(STATE_RECORD_FILE).is_file());
    assert!(state.join(PROJECT_IDENTITY_FILE).is_file());
    assert!(!NibPaths::project_dir(&root).exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Only the directory this test creates: other tests share the
        // process home and its `projects` directory.
        let mode = fs::metadata(&state).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", state.display());
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
    let _home = TestNibHome::isolated();
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
    assert!(NibPaths::subagent_worktrees(&root)
        .starts_with(NibPaths::state_home().unwrap().canonicalize().unwrap()));
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

#[cfg(unix)]
#[test]
fn a_symbolic_link_for_the_project_directory_is_neither_used_nor_moved() {
    let _home = TestNibHome::isolated();
    // Another project's state, which a planted `.nib` link points at.
    let other = LegacyProject::new();
    let directory = tempfile::tempdir().expect("project");
    let root = directory.path().canonicalize().unwrap();
    std::os::unix::fs::symlink(other.legacy(), NibPaths::project_dir(&root)).unwrap();
    let before = files_below(&other.legacy());

    assert!(!NibPaths::has_legacy_state(&root));
    assert_eq!(
        NibPaths::project_state(&root),
        NibPaths::user_state_dir(&root).unwrap()
    );
    assert_eq!(
        migrate_project_state(&root, &[], &["default".to_string()]),
        Ok(StateMigration::Current)
    );

    assert_eq!(files_below(&other.legacy()), before);
}

#[test]
fn loading_profiles_moves_the_state_and_resolves_the_moved_profile() {
    let _home = TestNibHome::isolated();
    let project = LegacyProject::new();

    let registry = crate::profile::ProfileRegistry::load(
        &project.root,
        &crate::config::ProfilesConfig::default(),
    )
    .expect("profiles");

    let profile = registry.default_profile();
    assert_eq!(
        profile.state_dir(),
        project.state().join("profiles/default")
    );
    assert_eq!(
        profile.memory_store().user("style").as_deref(),
        Some("terse")
    );
    // The sessions are in the session database, beside the moved state.
    assert_eq!(
        profile.sessions_dir(),
        project.state().join("profiles/default/sessions")
    );
    profile.ensure_state_dirs().expect("state directories");
    let store = crate::session::SessionStore::at_dir(profile.sessions_dir().to_path_buf());
    assert_eq!(store.list(), vec!["s-1".to_string(), "s-2".to_string()]);
    assert_eq!(store.load("s-1").expect("s-1").messages[0].content, "first");
    assert!(!profile.sessions_dir().join("s-1.json").exists());
    assert!(project.state().join("subagents/sub-1.json").is_file());
    assert!(is_moved_marker(
        &project
            .legacy()
            .join("profiles/default/.legacy-state-migration-v1.json")
    ));
    // Loading again finds nothing left to move.
    let again = crate::profile::ProfileRegistry::load(
        &project.root,
        &crate::config::ProfilesConfig::default(),
    )
    .expect("profiles again");
    assert_eq!(again.default_profile(), profile);
}

#[test]
fn an_interrupted_session_import_resumes_without_duplicates() {
    let _home = TestNibHome::isolated();
    let project = LegacyProject::new();
    let sessions_before = session_files(&project.legacy());
    // A run stopped after recording the move and importing `s-1`.
    let state = project.state();
    ensure_state_directory(&state).expect("state");
    write_record(
        &state.join(STATE_RECORD_FILE),
        &new_record(&project.root, "moving"),
    )
    .expect("record");
    let key = state.file_name().unwrap().to_str().unwrap();
    let scope = crate::session::database::SessionScope::new(key, "default");
    let database = crate::session::database::SessionDatabase::for_user_home().expect("database");
    let first_bytes =
        fs::read_to_string(project.legacy().join("profiles/default/sessions/s-1.json")).unwrap();
    let first: crate::session::Session = serde_json::from_str(&first_bytes).unwrap();
    let sha256: String = sha2::Sha256::digest(first_bytes.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    database
        .import(
            &scope,
            &first,
            &project.legacy().join("profiles/default/sessions/s-1.json"),
            &sha256,
            std::time::SystemTime::now(),
        )
        .expect("partial import");

    let outcome = project.migrate().expect("resumed migration");

    assert!(matches!(outcome, StateMigration::Moved(_)), "{outcome:?}");
    let imported = database_sessions(&state);
    let ids: Vec<&str> = imported.iter().map(|session| session.id.as_str()).collect();
    assert_eq!(ids, ["s-1", "s-2"]);
    assert_eq!(imported[0], first);
    let copies = legacy_copies(&project.legacy());
    assert_eq!(files_below(&copies[0].join("sessions")), sessions_before);
    assert_eq!(project.migrate(), Ok(StateMigration::Current));
    assert_eq!(database_sessions(&state).len(), 2);
}

#[test]
fn a_session_the_database_holds_differently_stops_the_first_migration() {
    let _home = TestNibHome::isolated();
    let project = LegacyProject::new();
    let state = project.state();
    let key = state.file_name().unwrap().to_str().unwrap().to_string();
    let other = crate::session::SessionStore::at_dir(
        NibPaths::database_sessions_dir(&key, "default").unwrap(),
    );
    let mut different = other.create_session_with_id("s-2");
    different.summary = Some("written elsewhere".to_string());
    other.save(&mut different).expect("different session");
    let legacy = files_below(&project.legacy());

    let error = project.migrate().expect_err("conflict");

    assert!(error.contains("s-2.json"), "{error}");
    assert!(error.contains("nothing was moved"), "{error}");
    assert_eq!(files_below(&project.legacy()), legacy);
    assert!(!state.join(STATE_RECORD_FILE).exists());
    assert_eq!(
        other.load("s-2").unwrap().summary.as_deref(),
        Some("written elsewhere")
    );
    assert!(other.load_result("s-1").unwrap().is_none());
}

#[test]
fn a_session_run_in_progress_defers_the_session_import() {
    let _home = TestNibHome::isolated();
    let project = LegacyProject::new();
    let sessions = project.legacy().join("profiles/default/sessions");
    let lease = crate::daemons::state::try_acquire_file_lock_in(
        &sessions.join(".session-run-s-1.lock"),
        &sessions,
    )
    .expect("run lease");

    let outcome = project.migrate().expect("deferred");

    assert!(
        matches!(&outcome, StateMigration::Deferred(reason) if reason.contains("session run is active")),
        "{outcome:?}"
    );
    assert!(sessions.join("s-1.json").is_file());
    assert_eq!(NibPaths::project_state(&project.root), project.legacy());
    drop(lease);
    assert!(matches!(project.migrate(), Ok(StateMigration::Moved(_))));
    assert_eq!(database_sessions(&project.state()).len(), 2);
}

#[test]
fn pre_profile_sessions_reach_the_session_database() {
    let _home = TestNibHome::isolated();
    let directory = tempfile::tempdir().expect("project");
    let root = directory.path().canonicalize().unwrap();
    let legacy = NibPaths::project_dir(&root);
    fs::create_dir_all(legacy.join("sessions")).unwrap();
    fs::write(
        legacy.join("sessions/old-1.json"),
        legacy_session("old-1", "before profiles"),
    )
    .unwrap();
    assert!(NibPaths::has_legacy_state(&root));

    let registry =
        crate::profile::ProfileRegistry::load(&root, &crate::config::ProfilesConfig::default())
            .expect("profiles");

    let state = NibPaths::user_state_dir(&root).unwrap();
    assert_eq!(
        registry.default_profile().sessions_dir(),
        state.join("profiles/default/sessions")
    );
    let sessions = database_sessions(&state);
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].messages[0].content, "before profiles");
    // Once imported, the original files are kept in the legacy copy, and
    // the project holds no legacy state any more.
    let copies = legacy_copies(&legacy);
    assert_eq!(copies.len(), 1);
    assert!(copies[0].join("sessions/sessions/old-1.json").is_file());
    assert!(!legacy.join("sessions").exists());
    assert!(!NibPaths::has_legacy_state(&root));
    let report = relink::layout_report(&root).expect("layout");
    assert!(
        report
            .legacy
            .iter()
            .all(|line| !line.contains("runtime state")),
        "{:?}",
        report.legacy
    );
    assert_eq!(
        migrate_project_state(&root, &[], &["default".to_string()]),
        Ok(StateMigration::Current)
    );
}

#[test]
fn a_recorded_legacy_session_directory_reaches_the_imported_sessions() {
    let _home = TestNibHome::isolated();
    let project = LegacyProject::new();
    let legacy_sessions = project.legacy().join("profiles/default/sessions");
    project.migrate().expect("migration");

    // A scheduled task recorded the session directory before the import.
    let store = crate::session::SessionStore::at_dir(legacy_sessions.clone());

    assert!(store.database_path().is_some());
    assert_eq!(store.load("s-1").expect("s-1").messages[0].content, "first");
    store
        .record_event(
            "s-1",
            "scheduled_agent_run_completed",
            serde_json::json!({}),
        )
        .expect("event");
    assert!(
        !legacy_sessions.exists(),
        "no second session store starts in the project"
    );
    // A configured or never-imported directory is not redirected.
    assert_eq!(
        moved_sessions_dir(&project.legacy().join("profiles/other/sessions")),
        None
    );
}

#[test]
fn a_run_stopped_after_the_last_rename_is_finished_by_the_next_one() {
    let _home = TestNibHome::isolated();
    let project = LegacyProject::new();
    let legacy_sessions = project.legacy().join("profiles/default/sessions");
    let state = project.state();
    STOP_AFTER_IMPORT_RENAME.set(true);

    project.migrate().expect_err("interrupted");

    assert!(!legacy_sessions.exists());
    let record = read_record(&state.join(STATE_RECORD_FILE))
        .unwrap()
        .unwrap();
    assert_eq!(record.status, "moving");
    // Every step was recorded before it ran.
    assert!(record.moved.contains(&"memory.json".to_string()));
    assert!(record
        .imported
        .iter()
        .any(|entry| entry.starts_with("profiles/default/sessions:")));

    assert_eq!(project.migrate(), Ok(StateMigration::Current));

    let record = read_record(&state.join(STATE_RECORD_FILE))
        .unwrap()
        .unwrap();
    assert_eq!(record.status, "done");
    assert!(record.completed_at.is_some());
    assert!(is_moved_marker(
        &project
            .legacy()
            .join("profiles/default")
            .join(LEGACY_PROFILE_STATE_ENTRIES[0])
    ));
    let store = crate::session::SessionStore::at_dir(legacy_sessions.clone());
    assert!(store.database_path().is_some());
    assert_eq!(
        store.load("s-2").expect("s-2").messages[0].content,
        "second"
    );
    assert!(
        !legacy_sessions.exists(),
        "no second session store starts in the project"
    );
    let report = relink::layout_report(&project.root).expect("layout");
    assert_eq!(report.state_status.as_deref(), Some("done"));
}

#[test]
fn a_session_saved_between_its_import_and_the_rename_is_reported() {
    let _home = TestNibHome::isolated();
    let project = LegacyProject::new();
    AFTER_IMPORT.set(Some(|source: &Path| {
        fs::write(source.join("s-2.json"), legacy_session("s-2", "saved late")).unwrap();
        fs::write(
            source.join("s-3.json"),
            legacy_session("s-3", "created late"),
        )
        .unwrap();
    }));

    let outcome = project.migrate().expect("migration");

    assert!(matches!(outcome, StateMigration::Moved(_)), "{outcome:?}");
    let kept = legacy_copies(&project.legacy())[0].join("sessions/profiles/default/sessions");
    let record = read_record(&project.state().join(STATE_RECORD_FILE))
        .unwrap()
        .unwrap();
    assert_eq!(
        record.not_imported,
        [
            kept.join("broken.json"),
            kept.join("s-2.json"),
            kept.join("s-3.json")
        ]
    );
    // The database keeps what was imported; the late saves stay on disk.
    let sessions = database_sessions(&project.state());
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[1].messages[0].content, "second");
    assert!(fs::read_to_string(kept.join("s-2.json"))
        .unwrap()
        .contains("saved late"));
    assert_eq!(
        relink::layout_report(&project.root)
            .expect("layout")
            .not_imported,
        record.not_imported
    );
}

#[test]
fn a_session_directory_over_the_entry_limit_stops_the_migration() {
    let _home = TestNibHome::isolated();
    let project = LegacyProject::new();
    for index in 0..sessions::MAX_SESSION_DIR_ENTRIES {
        project.write(&format!("profiles/default/sessions/extra-{index}.txt"), "");
    }
    let legacy = files_below(&project.legacy());

    let error = project.migrate().expect_err("too many entries");

    assert!(error.contains("more than"), "{error}");
    assert_eq!(files_below(&project.legacy()), legacy);
    assert!(!project.state().join(STATE_RECORD_FILE).exists());
}
