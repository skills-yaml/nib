use super::*;
use crate::session::{MessageOrigin, SessionDeleteOutcome, SessionStore, ToolCallRecord};
use std::fs;

const CHILD_SESSIONS_DIR: &str = "NIB_TEST_SESSION_DB_CHILD_DIR";
const CHILD_SESSION_ID: &str = "NIB_TEST_SESSION_DB_CHILD_ID";
const CHILD_MODE: &str = "NIB_TEST_SESSION_DB_CHILD_MODE";
const CHILD_READY: &str = "NIB_TEST_SESSION_DB_CHILD_READY";
const CHILD_RELEASE: &str = "NIB_TEST_SESSION_DB_CHILD_RELEASE";

/// The default session directory of a new project key in the test home,
/// whose sessions live in the session database.
fn database_sessions_dir() -> PathBuf {
    let key = format!("db-test-{}", uuid::Uuid::new_v4().simple());
    crate::paths::NibPaths::database_sessions_dir(&key, "default").expect("test home")
}

fn database_store() -> SessionStore {
    let store = SessionStore::at_dir(database_sessions_dir());
    assert!(
        store.database.is_some(),
        "store must use the session database"
    );
    store
}

fn raw_connection(store: &SessionStore) -> Connection {
    let database = store.database.as_ref().expect("database store");
    Connection::open(database.database.path()).expect("raw database connection")
}

fn scope_of(store: &SessionStore) -> String {
    store
        .database
        .as_ref()
        .expect("database store")
        .scope
        .scope
        .clone()
}

#[test]
fn default_user_home_sessions_live_in_the_database_and_others_in_files() {
    let store = database_store();
    let session = store.create_session_with_id("db-located");
    store.append_message(&session.id, "user", "hello");

    assert!(
        !store.path(&session.id).exists(),
        "a database session must not leave a JSON file"
    );
    let database = crate::paths::NibPaths::session_database().expect("database path");
    assert!(database.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&database)
            .expect("database")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "nib.db must be owner-only");
    }
    let journal: String = raw_connection(&store)
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("journal mode");
    assert_eq!(journal.to_ascii_lowercase(), "wal");

    let outside = tempfile::tempdir().expect("outside");
    let file_store = SessionStore::at_dir(outside.path().join("sessions"));
    assert!(file_store.database.is_none());
    let file_session = file_store.create_session_with_id("file-located");
    assert!(file_store.path(&file_session.id).is_file());

    // A managed worktree or any other directory below the user home keeps
    // its sessions as files.
    let worktree_sessions = database_sessions_dir()
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("state")
        .join("worktrees/subagents/child/.nib/profiles/default/sessions");
    assert!(SessionScope::for_sessions_dir(&worktree_sessions).is_none());
}

#[test]
fn database_sessions_roundtrip_messages_events_and_exact_floats() {
    let store = database_store();
    let session = store.create_session_with_id("db-roundtrip");
    store
        .try_append_message_with_origin(&session.id, "user", "hello", MessageOrigin::HumanRequest)
        .expect("user message");
    store.append_message(&session.id, "assistant", "hi");
    store
        .record_event(&session.id, "custom", serde_json::json!({"n": 1}))
        .expect("event");
    let duration = 1.551_978_736_000_000_1_f64;
    store
        .record_tool_call(ToolCallRecord {
            session_id: Some(session.id.clone()),
            tool_name: Some("probe".to_string()),
            arguments: serde_json::json!({"nested_duration": duration}),
            duration_seconds: Some(duration),
            ..ToolCallRecord::default()
        })
        .expect("tool call");

    let loaded = store.load(&session.id).expect("session");
    assert_eq!(loaded.messages.len(), 2);
    assert_eq!(loaded.messages[1].content, "hi");
    assert_eq!(loaded.events.len(), 1);
    assert_eq!(loaded.events[0].details["n"], 1);
    assert_eq!(loaded.message_provenance.len(), 1);
    assert_eq!(
        loaded.tool_calls[0]
            .duration_seconds
            .expect("duration")
            .to_bits(),
        duration.to_bits()
    );
    assert_eq!(loaded.revision, 4);
    assert_eq!(store.list(), vec![session.id.clone()]);

    let connection = raw_connection(&store);
    let scope = scope_of(&store);
    let count = |table: &str| -> i64 {
        connection
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE scope = ?1 AND session_id = ?2"),
                params![scope, session.id],
                |row| row.get(0),
            )
            .expect("row count")
    };
    assert_eq!(count("session_messages"), 2);
    assert_eq!(count("session_events"), 1);
    let (project_key, profile_id): (String, String) = connection
        .query_row(
            "SELECT project_key, profile_id FROM sessions WHERE scope = ?1 AND id = ?2",
            params![scope, session.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("session row");
    assert!(project_key.starts_with("db-test-"));
    assert_eq!(profile_id, "default");
}

#[test]
fn appending_a_message_writes_only_the_new_row() {
    let store = database_store();
    let session = store.create_session_with_id("db-append-only");
    store.append_message(&session.id, "user", "first");
    let scope = scope_of(&store);
    let trigger = format!("observe_{}", uuid::Uuid::new_v4().simple());
    // A trigger in the database observes every write of the store's own
    // connections to this session's earlier rows.
    raw_connection(&store)
        .execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS test_rewritten_rows (scope TEXT, idx INTEGER);
             CREATE TRIGGER {trigger} AFTER UPDATE ON session_messages WHEN old.scope = '{scope}'
             BEGIN INSERT INTO test_rewritten_rows VALUES (old.scope, old.idx); END;"
        ))
        .expect("observe rewrites");

    store.append_message(&session.id, "assistant", "second");
    store.append_message(&session.id, "user", "third");

    let connection = raw_connection(&store);
    let rewritten: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM test_rewritten_rows WHERE scope = ?1",
            params![scope],
            |row| row.get(0),
        )
        .expect("rewrites");
    connection
        .execute_batch(&format!("DROP TRIGGER {trigger}"))
        .expect("drop trigger");
    assert_eq!(rewritten, 0, "earlier message rows must not be rewritten");
    let messages = store.load(&session.id).expect("session").messages;
    let contents: Vec<&str> = messages
        .iter()
        .map(|message| message.content.as_str())
        .collect();
    assert_eq!(contents, ["first", "second", "third"]);
}

#[test]
fn a_stale_snapshot_is_rejected_by_the_revision_guard() {
    let store = database_store();
    let session = store.create_session_with_id("db-revision-cas");
    let mut first = store.load(&session.id).expect("first snapshot");
    let mut stale = first.clone();
    first.summary = Some("first writer".to_string());
    stale.summary = Some("stale writer".to_string());

    store.save(&mut first).expect("first revision commit");
    let error = store.save(&mut stale).expect_err("stale snapshot");
    assert!(
        error.to_string().contains("stale session revision"),
        "{error}"
    );

    // Below the store's locks, a row that changed after it was read is
    // rejected by the transaction itself, and nothing is written.
    let database = store.database.as_ref().expect("database store");
    let stored = database
        .database
        .load(&database.scope, &session.id)
        .expect("load")
        .expect("stored");
    raw_connection(&store)
        .execute(
            "UPDATE sessions SET revision = revision + 1 WHERE scope = ?1 AND id = ?2",
            params![database.scope.scope, session.id],
        )
        .expect("concurrent writer");
    let mut next = stored.session.clone();
    next.summary = Some("lost update".to_string());
    next.revision += 1;
    let error = database
        .database
        .write(
            &database.scope,
            &next,
            EncodedSession::encode(&next).expect("encode"),
            Some(&stored.row),
            || Ok(()),
        )
        .expect_err("a changed row must reject the write");
    assert!(
        error.to_string().contains("stale session revision"),
        "{error}"
    );
    let document: String = raw_connection(&store)
        .query_row(
            "SELECT document FROM sessions WHERE scope = ?1 AND id = ?2",
            params![database.scope.scope, session.id],
            |row| row.get(0),
        )
        .expect("document");
    assert!(document.contains("first writer"));
}

#[test]
fn a_failing_commit_check_rolls_the_whole_write_back() {
    let store = database_store();
    let session = store.create_session_with_id("db-rollback");
    let database = store.database.as_ref().expect("database store");
    let stored = database
        .database
        .load(&database.scope, &session.id)
        .expect("load")
        .expect("stored");
    let mut next = stored.session.clone();
    next.messages.push(crate::session::SessionMessage {
        index: 0,
        role: "user".to_string(),
        content: "never committed".to_string(),
        timestamp: None,
        attachments: Vec::new(),
    });
    next.revision += 1;
    let error = database
        .database
        .write(
            &database.scope,
            &next,
            EncodedSession::encode(&next).expect("encode"),
            Some(&stored.row),
            || {
                Err(SessionError::InvalidMutation(
                    "deadline elapsed".to_string(),
                ))
            },
        )
        .expect_err("the commit check fails");
    assert!(error.to_string().contains("deadline elapsed"), "{error}");
    let reloaded = store.load(&session.id).expect("session");
    assert_eq!(reloaded, stored.session);
}

#[test]
fn deleting_a_database_session_removes_its_rows() {
    let store = database_store();
    let session = store.create_session_with_id("db-delete");
    store.append_message(&session.id, "user", "bye");

    assert_eq!(
        store.delete_if(&session.id, |_, _| false).expect("retain"),
        SessionDeleteOutcome::Retained
    );
    let outcome = store
        .delete_if(&session.id, |_, metadata| {
            metadata.modified().is_ok() && !metadata.is_empty()
        })
        .expect("delete");
    assert_eq!(outcome, SessionDeleteOutcome::Deleted);
    assert!(store.load_result(&session.id).expect("load").is_none());
    let orphans: i64 = raw_connection(&store)
        .query_row(
            "SELECT COUNT(*) FROM session_messages WHERE scope = ?1",
            params![scope_of(&store)],
            |row| row.get(0),
        )
        .expect("orphans");
    assert_eq!(orphans, 0);
    assert_eq!(
        store.delete_if(&session.id, |_, _| true).expect("missing"),
        SessionDeleteOutcome::Missing
    );
}

#[test]
fn corrupt_rows_fail_loudly_instead_of_reading_as_missing() {
    let store = database_store();
    let session = store.create_session_with_id("db-corrupt");
    raw_connection(&store)
        .execute(
            "UPDATE sessions SET document = 'not json' WHERE scope = ?1 AND id = ?2",
            params![scope_of(&store), session.id],
        )
        .expect("corrupt the document");
    assert!(store.load_result(&session.id).is_err());
    assert!(store.list_result().is_err());
    let mut replacement = Session::new(session.id.clone());
    assert!(
        store.save(&mut replacement).is_err(),
        "a corrupt session must not be overwritten"
    );
}

#[test]
fn a_newer_schema_is_refused() {
    let home = tempfile::tempdir().expect("home");
    let path = home.path().join("nib.db");
    {
        let connection = Connection::open(&path).expect("database");
        connection
            .execute_batch(
                "CREATE TABLE schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);
                 INSERT INTO schema_version VALUES (99, 'future');",
            )
            .expect("future schema");
    }
    let error = SessionDatabase::open(&path).expect_err("newer schema");
    assert!(error.to_string().contains("newer than this nib"), "{error}");

    let fresh = home.path().join("fresh.db");
    SessionDatabase::open(&fresh).expect("fresh database");
    let version: i64 = Connection::open(&fresh)
        .expect("fresh")
        .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
            row.get(0)
        })
        .expect("version");
    assert_eq!(version, SCHEMA_VERSION);
}

#[test]
fn relinking_moves_every_session_of_a_project_key() {
    let store = database_store();
    let database = store.database.as_ref().expect("database store").clone();
    let session = store.create_session_with_id("db-relink");
    store.append_message(&session.id, "user", "kept");
    let from = database.scope.project_key.clone();
    let to = format!("db-test-{}", uuid::Uuid::new_v4().simple());

    let moved = database
        .database
        .relink_project(&from, &to)
        .expect("relink");
    assert_eq!(moved, 1);
    let relinked = SessionStore::at_dir(
        crate::paths::NibPaths::database_sessions_dir(&to, "default").expect("dir"),
    );
    let loaded = relinked.load(&session.id).expect("relinked session");
    assert_eq!(loaded.messages[0].content, "kept");
    assert!(store.load_result(&session.id).expect("old").is_none());

    // A session id under both keys is a conflict and nothing moves.
    store.create_session_with_id("db-relink");
    let error = database
        .database
        .relink_project(&from, &to)
        .expect_err("conflict");
    assert!(error.to_string().contains("nothing was moved"), "{error}");
    assert!(store.load_result("db-relink").expect("kept").is_some());
}

/// Two processes write the same database session: the run lease is
/// exclusive across them, and the second writer of a stale snapshot is
/// rejected on the revision conflict.
#[cfg(unix)]
#[test]
fn database_sessions_are_guarded_across_processes() {
    if let Some(sessions_dir) = std::env::var_os(CHILD_SESSIONS_DIR) {
        run_database_child(Path::new(&sessions_dir));
        return;
    }
    let sessions_dir = database_sessions_dir();
    let store = SessionStore::at_dir(sessions_dir.clone());
    let session = store.create_session_with_id("db-cross-process");
    let scratch = tempfile::tempdir().expect("scratch");

    let lease = store.try_acquire_run_lease(&session.id).expect("lease");
    let status = spawn_database_child(&sessions_dir, &session.id, "lease", scratch.path(), false)
        .wait()
        .expect("lease child");
    assert!(status.success(), "the child must see the lease held");
    drop(lease);

    let ready = scratch.path().join("ready");
    let release = scratch.path().join("release");
    let mut child = spawn_database_child(&sessions_dir, &session.id, "stale", scratch.path(), true);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !ready.exists() {
        assert!(std::time::Instant::now() < deadline, "child not ready");
        assert!(child.try_wait().expect("child").is_none(), "child exited");
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut newer = store.load(&session.id).expect("snapshot");
    newer.summary = Some("parent wins".to_string());
    store.save(&mut newer).expect("parent commit");
    fs::write(&release, b"go").expect("release");
    let status = child.wait().expect("stale child");
    assert!(status.success(), "the child must be rejected as stale");
    assert_eq!(
        store.load(&session.id).expect("session").summary.as_deref(),
        Some("parent wins")
    );
}

#[cfg(unix)]
fn spawn_database_child(
    sessions_dir: &Path,
    id: &str,
    mode: &str,
    scratch: &Path,
    with_release: bool,
) -> std::process::Child {
    let mut command =
        std::process::Command::new(std::env::current_exe().expect("current test binary"));
    command
        .args([
            "--exact",
            "session::database::tests::database_sessions_are_guarded_across_processes",
            "--nocapture",
        ])
        .env(CHILD_SESSIONS_DIR, sessions_dir)
        .env(CHILD_SESSION_ID, id)
        .env(CHILD_MODE, mode)
        .env(CHILD_READY, scratch.join("ready"));
    if with_release {
        command.env(CHILD_RELEASE, scratch.join("release"));
    }
    command.spawn().expect("spawn database child")
}

#[cfg(unix)]
fn run_database_child(sessions_dir: &Path) {
    let id = std::env::var(CHILD_SESSION_ID).expect("child id");
    let mode = std::env::var(CHILD_MODE).expect("child mode");
    let store = SessionStore::at_dir(sessions_dir.to_path_buf());
    assert!(store.database.is_some(), "child must share the database");
    match mode.as_str() {
        "lease" => {
            let error = store
                .try_acquire_run_lease(&id)
                .err()
                .expect("the lease is held by the parent");
            assert!(matches!(error, SessionError::RunLeaseHeld(_)), "{error}");
        }
        "stale" => {
            let mut snapshot = store.load(&id).expect("child snapshot");
            fs::write(std::env::var_os(CHILD_READY).expect("ready"), b"ready").expect("ready");
            let release = PathBuf::from(std::env::var_os(CHILD_RELEASE).expect("release"));
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while !release.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "parent never released"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            snapshot.summary = Some("child loses".to_string());
            let error = store.save(&mut snapshot).expect_err("stale child write");
            assert!(
                error.to_string().contains("stale session revision"),
                "{error}"
            );
        }
        other => panic!("unknown database child mode {other}"),
    }
}
