//! A process that opened a legacy store while migration was deferred keeps
//! writing after another process moves the state (T089).

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::ExecutionConfig;
use crate::daemons::workload::{DurableTaskStore, DurableTerminalRequest};
use crate::paths::{NibPaths, TestNibHome};
use crate::session::SessionStore;
use crate::state_migration::{migrate_project_state, StateMigration};

struct HeldProject {
    _home: TestNibHome,
    _directory: tempfile::TempDir,
    root: PathBuf,
    sessions: SessionStore,
    tasks: DurableTaskStore,
}

fn held_project() -> HeldProject {
    let home = TestNibHome::isolated();
    let directory = tempfile::tempdir().expect("project");
    let root = directory.path().canonicalize().expect("canonical project");
    fs::create_dir_all(root.join(".nib/profiles/default/sessions")).expect("sessions");
    write_presence(&root);
    let sessions = SessionStore::at_dir(root.join(".nib/profiles/default/sessions"));
    sessions.create_session_with_id("held");
    sessions
        .record_event("held", "before", serde_json::json!({"n": 1}))
        .expect("event before migration");
    let tasks = DurableTaskStore::at_daemon_dir(root.join(".nib/profiles/default/daemons"))
        .expect("daemon store");
    tasks
        .prepare_terminal(terminal_request(&root, sessions.sessions_dir()))
        .expect("prepared task");
    HeldProject {
        _home: home,
        _directory: directory,
        root,
        sessions,
        tasks,
    }
}

fn write_presence(root: &Path) {
    let presence = root.join(".nib/profiles/default/presence");
    fs::create_dir_all(&presence).expect("presence");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let folder = serde_json::to_string(&root).expect("folder");
    fs::write(
        presence.join("live.json"),
        format!("{{\"session_id\":\"live\",\"folder\":{folder},\"updated_at\":{now}}}"),
    )
    .expect("heartbeat");
}

fn terminal_request(root: &Path, sessions_dir: &Path) -> DurableTerminalRequest {
    DurableTerminalRequest {
        id: "held-task".to_string(),
        command: "true".to_string(),
        cwd: root.to_path_buf(),
        project_root: root.to_path_buf(),
        profile_id: "default".to_string(),
        sessions_dir: sessions_dir.to_path_buf(),
        session_id: "held".to_string(),
        execution: ExecutionConfig::default(),
        timeout_secs: 10,
        max_output_bytes: 1024,
    }
}

fn migrate(root: &Path) -> StateMigration {
    migrate_project_state(root, &[], &["default".to_string()]).expect("migration")
}

fn event_kinds(store: &SessionStore) -> Vec<String> {
    store
        .load("held")
        .expect("session")
        .events
        .into_iter()
        .map(|event| event.kind)
        .collect()
}

fn live_session_store(root: &Path) -> SessionStore {
    SessionStore::at_dir(NibPaths::profile_sessions(root, "default"))
}

fn task_file(root: &Path) -> PathBuf {
    NibPaths::profile_state(root, "default").join("daemons/tasks/held-task.json")
}

fn legacy_sessions(root: &Path) -> PathBuf {
    root.join(".nib/profiles/default/sessions")
}

fn legacy_daemons(root: &Path) -> PathBuf {
    root.join(".nib/profiles/default/daemons")
}

/// Session JSON kept beside the project after import.
fn backup_session(root: &Path) -> String {
    let mut pending = vec![root.join(".nib")];
    while let Some(current) = pending.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            if path.file_name() == Some("held.json".as_ref())
                && path.components().any(|component| {
                    component
                        .as_os_str()
                        .to_string_lossy()
                        .starts_with("legacy-")
                })
            {
                return fs::read_to_string(&path).expect("backup session");
            }
        }
    }
    panic!("imported session backup was not kept");
}

#[test]
fn held_stores_follow_a_migration_without_losing_or_duplicating_writes() {
    let project = held_project();
    assert!(
        matches!(migrate(&project.root), StateMigration::Deferred(_)),
        "a live heartbeat defers migration"
    );
    project
        .sessions
        .record_event("held", "deferred", serde_json::json!({"n": 2}))
        .expect("write while migration is deferred");
    assert!(legacy_sessions(&project.root).join("held.json").is_file());
    assert!(!NibPaths::user_state_dir(&project.root)
        .expect("user state")
        .join("state.toml")
        .exists());

    fs::remove_dir_all(project.root.join(".nib/profiles/default/presence")).expect("presence");
    assert!(
        matches!(migrate(&project.root), StateMigration::Moved(_)),
        "the second process migrates once the project is idle"
    );

    project
        .sessions
        .record_event("held", "after", serde_json::json!({"n": 3}))
        .expect("session write after migration");
    project
        .sessions
        .record_event("held", "after", serde_json::json!({"n": 4}))
        .expect("second session write after migration");
    let failed = project
        .tasks
        .fail_prepared("held-task", "stopped".to_string())
        .expect("task write after migration");

    assert!(!legacy_sessions(&project.root).exists());
    assert!(!legacy_daemons(&project.root).exists());
    assert_eq!(
        event_kinds(&project.sessions),
        ["before", "deferred", "after", "after"]
    );
    assert_eq!(
        event_kinds(&live_session_store(&project.root)),
        ["before", "deferred", "after", "after"]
    );
    let backup = backup_session(&project.root);
    assert!(backup.contains("\"deferred\""), "{backup}");
    assert!(!backup.contains("\"after\""), "{backup}");
    assert_eq!(failed.status, "failed");
    assert_eq!(failed.error.as_deref(), Some("stopped"));
    project
        .tasks
        .audit_compensation_failure("held-task", "stop", "primary", "compensation")
        .expect("audit after migration");
    let audit = NibPaths::profile_state(&project.root, "default").join("daemons/audit.jsonl");
    let audit_bytes = fs::read_to_string(&audit).expect("migrated audit");
    assert_eq!(audit_bytes.matches("compensation_failed").count(), 1);
    assert!(!legacy_daemons(&project.root).exists());
    let task_bytes = fs::read_to_string(task_file(&project.root)).expect("migrated task");
    assert_eq!(task_bytes.matches("stopped").count(), 1);
    let reread = DurableTaskStore::at_daemon_dir(
        NibPaths::profile_state(&project.root, "default").join("daemons"),
    )
    .expect("reread store")
    .get("held-task")
    .expect("read task")
    .expect("task");
    assert_eq!(reread.status, "failed");
    assert_eq!(reread.error.as_deref(), Some("stopped"));
}

#[test]
fn missing_session_directory_without_a_migration_record_is_not_redirected() {
    let project = held_project();
    fs::remove_dir_all(legacy_sessions(&project.root)).expect("remove sessions");
    let error = project
        .sessions
        .record_event("held", "not-redirected", serde_json::json!({}))
        .expect_err("a missing directory with no migration record fails closed");
    assert!(
        error.to_string().contains("state directory changed")
            || error.to_string().contains("identity")
            || error.to_string().contains("not found"),
        "{error}"
    );
    let recreated = legacy_sessions(&project.root).join("held.json");
    if recreated.is_file() {
        assert!(!fs::read_to_string(&recreated)
            .expect("recreated session")
            .contains("not-redirected"));
    }
    let state = NibPaths::user_state_dir(&project.root).expect("user state");
    assert!(!state.join("state.toml").exists());
    let redirected = SessionStore::at_dir(
        NibPaths::database_sessions_dir(
            &NibPaths::state_key(&project.root).expect("key"),
            "default",
        )
        .expect("database sessions"),
    );
    assert!(redirected.load_result("held").expect("load").is_none());
}

#[cfg(unix)]
#[test]
fn symlink_and_replacement_still_fail_closed() {
    let project = held_project();
    let sessions = legacy_sessions(&project.root);
    let displaced_sessions = project.root.join("sessions-displaced");
    fs::rename(&sessions, &displaced_sessions).expect("displace sessions");
    let session_target = project.root.join("session-replacement");
    fs::create_dir(&session_target).expect("session replacement");
    fs::write(session_target.join("sentinel"), b"session").expect("sentinel");
    std::os::unix::fs::symlink(&session_target, &sessions).expect("session symlink");
    let error = project
        .sessions
        .record_event("held", "swapped", serde_json::json!({}))
        .expect_err("symlink swap fails closed");
    assert!(
        error.to_string().contains("state directory changed")
            || error.to_string().contains("identity changed")
            || error.to_string().contains("unsafe"),
        "{error}"
    );
    assert_eq!(
        fs::read(session_target.join("sentinel")).unwrap(),
        b"session"
    );
    assert!(!fs::read_to_string(displaced_sessions.join("held.json"))
        .unwrap()
        .contains("swapped"));
    fs::remove_file(&sessions).expect("remove session symlink");

    let tasks = project.root.join(".nib/profiles/default/daemons/tasks");
    let daemon = project.root.join(".nib/profiles/default/daemons");
    let displaced_tasks = project.root.join("tasks-displaced");
    let before_daemon = directory_names(&daemon);
    fs::rename(&tasks, &displaced_tasks).expect("displace tasks");
    fs::create_dir(&tasks).expect("replacement tasks");
    fs::write(tasks.join("sentinel"), b"tasks").expect("task sentinel");
    let error = project
        .tasks
        .fail_prepared("held-task", "replaced".to_string())
        .expect_err("replaced task directory fails closed");
    assert!(
        error.contains("identity changed") || error.contains("state directory changed"),
        "{error}"
    );
    assert_eq!(fs::read(tasks.join("sentinel")).unwrap(), b"tasks");
    assert_eq!(directory_names(&tasks), vec![OsString::from("sentinel")]);
    assert_eq!(directory_names(&daemon), before_daemon);
    assert!(!fs::read_to_string(displaced_tasks.join("held-task.json"))
        .unwrap()
        .contains("replaced"));
}

#[cfg(unix)]
#[test]
fn unsafe_or_foreign_migrated_daemon_directory_does_not_recreate_legacy() {
    let project = held_project();
    fs::remove_dir_all(project.root.join(".nib/profiles/default/presence")).expect("presence");
    assert!(matches!(migrate(&project.root), StateMigration::Moved(_)));
    let live = NibPaths::profile_state(&project.root, "default").join("daemons");
    assert!(live.is_dir(), "the renamed daemon directory exists");
    let displaced = project.root.join("live-daemons-displaced");
    fs::rename(&live, &displaced).expect("displace daemons");
    let target = project.root.join("daemon-replacement");
    fs::create_dir(&target).expect("replacement");
    fs::write(target.join("sentinel"), b"daemon").expect("sentinel");
    std::os::unix::fs::symlink(&target, &live).expect("daemon symlink");

    let error = project
        .tasks
        .audit_compensation_failure("held-task", "stop", "primary", "compensation")
        .expect_err("unsafe migrated daemon directory fails closed");
    assert!(
        error.contains("unsafe") || error.contains("symlink") || error.contains("state directory"),
        "{error}"
    );
    assert!(!legacy_daemons(&project.root).exists());
    assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"daemon");
    assert!(!fs::read_to_string(displaced.join("tasks/held-task.json"))
        .unwrap()
        .contains("compensation"));

    fs::remove_file(&live).expect("remove symlink");
    fs::create_dir(&live).expect("foreign daemon directory");
    fs::write(live.join("sentinel"), b"foreign").expect("foreign sentinel");
    let error = project
        .tasks
        .fail_prepared("held-task", "foreign".to_string())
        .expect_err("different inode fails closed");
    assert!(
        error.contains("identity changed") || error.contains("state directory changed"),
        "{error}"
    );
    assert!(!legacy_daemons(&project.root).exists());
    assert_eq!(directory_names(&live), vec![OsString::from("sentinel")]);
    assert!(!fs::read_to_string(displaced.join("tasks/held-task.json"))
        .unwrap()
        .contains("foreign"));
}

fn directory_names(path: &Path) -> Vec<OsString> {
    let mut names = fs::read_dir(path)
        .expect("directory")
        .map(|entry| entry.expect("entry").file_name())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[cfg(unix)]
#[test]
fn unsafe_migrated_session_directory_does_not_receive_a_second_write() {
    let project = held_project();
    fs::remove_dir_all(project.root.join(".nib/profiles/default/presence")).expect("presence");
    assert!(matches!(migrate(&project.root), StateMigration::Moved(_)));
    let live = NibPaths::profile_sessions(&project.root, "default");
    drop(SessionStore::at_dir(live.clone()));
    assert!(live.is_dir(), "the migrated session directory exists");
    let displaced = project.root.join("live-sessions-displaced");
    fs::rename(&live, &displaced).expect("displace live sessions");
    let target = project.root.join("live-replacement");
    fs::create_dir(&target).expect("replacement");
    fs::write(target.join("sentinel"), b"live").expect("sentinel");
    std::os::unix::fs::symlink(&target, &live).expect("live symlink");

    let error = project
        .sessions
        .record_event("held", "unsafe", serde_json::json!({}))
        .expect_err("unsafe migrated directory fails closed");
    assert!(
        error.to_string().contains("unsafe") || error.to_string().contains("symlink"),
        "{error}"
    );
    assert!(!legacy_sessions(&project.root).exists());
    assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"live");
    fs::remove_file(&live).expect("remove symlink");
    fs::rename(&displaced, &live).expect("restore live sessions");
    assert_eq!(event_kinds(&live_session_store(&project.root)), ["before"]);
}
