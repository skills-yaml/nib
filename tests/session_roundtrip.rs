//! Integration tests for session persistence (T009).

use chrono::Utc;
use nib::session::memory::{MemoryEntryMetadata, MemoryStore};
use nib::session::{SessionEvent, SessionStore};
use std::fs;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::tempdir;

const CHILD_MODE: &str = "NIB_STORE_CHILD_MODE";
const CHILD_ROOT: &str = "NIB_STORE_CHILD_ROOT";
const CHILD_SESSION: &str = "NIB_STORE_CHILD_SESSION";
const CHILD_WRITER: &str = "NIB_STORE_CHILD_WRITER";
const CHILD_ITERATIONS: &str = "NIB_STORE_CHILD_ITERATIONS";
const CHILD_COORDINATION: &str = "NIB_STORE_CHILD_COORDINATION";

#[test]
fn session_roundtrip_integration() {
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path());

    let session = store.create_session();
    store.append_message(&session.id, "user", "explore the repo");
    store.append_message(&session.id, "assistant", "listing files");

    let loaded = store.load(&session.id).expect("session must exist");
    assert_eq!(loaded.messages.len(), 2);
    assert_eq!(loaded.messages[0].index, 0);
    assert_eq!(loaded.messages[1].index, 1);
    assert_eq!(loaded.messages[0].content, "explore the repo");
    loaded.validate_message_sequence().expect("valid roles");

    let on_disk = fs::read_to_string(store.sessions_dir().join(format!("{}.json", session.id)))
        .expect("session file");
    assert!(on_disk.contains("\"role\": \"user\""));
    assert!(on_disk.contains("explore the repo"));
}

#[test]
fn session_compat_legacy_json_fixture() {
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path());

    let fixture = r#"{
  "id": "fixture-id",
  "started_at": "2026-06-20T12:00:00+00:00",
  "messages": [
    {"index": 0, "role": "user", "content": "goal", "timestamp": "2026-06-20T12:00:01+00:00"},
    {"index": 1, "role": "assistant", "content": "done"}
  ],
  "tool_calls": [
    {
      "id": "tool-1",
      "session_id": "fixture-id",
      "tool_name": "list_directory",
      "arguments": {"path": "."},
      "result": {"entries": []}
    }
  ]
}"#;

    fs::write(store.sessions_dir().join("fixture-id.json"), fixture).expect("write fixture");

    let loaded = store.load("fixture-id").expect("load fixture");
    assert_eq!(loaded.messages.len(), 2);
    assert_eq!(loaded.messages[0].index, 0);
    assert_eq!(loaded.messages[1].index, 1);
    assert!(loaded.events.is_empty());
    assert!(loaded.active_skills.is_empty());
    assert!(loaded.skill_usage.is_empty());
    assert_eq!(loaded.tool_calls.len(), 1);
    assert_eq!(
        loaded.tool_calls[0].tool_name.as_deref(),
        Some("list_directory")
    );
}

#[test]
fn corrupt_session_loads_and_mutations_fail_closed() {
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path());
    let session = store.create_session();
    store
        .try_append_message(&session.id, "user", "preserve me")
        .expect("initial message");
    let mut valid_snapshot = store.load(&session.id).expect("valid session");
    let path = store.sessions_dir().join(format!("{}.json", session.id));

    fs::write(&path, b"{not valid json").expect("write corruption");
    let corrupt_bytes = fs::read(&path).expect("read corrupt bytes");

    assert!(store.load_result(&session.id).is_err());
    assert!(store
        .try_append_message(&session.id, "assistant", "must not overwrite")
        .is_err());
    assert!(store
        .record_event(&session.id, "must_not_overwrite", serde_json::json!({}))
        .is_err());
    assert!(store.save(&mut valid_snapshot).is_err());
    assert_eq!(
        fs::read(&path).expect("read preserved corruption"),
        corrupt_bytes
    );
}

#[test]
fn invalid_persisted_role_and_index_sequences_are_rejected() {
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path());
    let role_path = store.sessions_dir().join("bad-role.json");
    fs::write(
        &role_path,
        r#"{
  "id": "bad-role",
  "messages": [
    {"index": 0, "role": "user", "content": "one"},
    {"index": 1, "role": "user", "content": "two"}
  ]
}"#,
    )
    .expect("write invalid roles");
    let role_error = store
        .load_result("bad-role")
        .expect_err("duplicate role must fail");
    assert!(role_error.to_string().contains("role transition"));

    let index_path = store.sessions_dir().join("bad-index.json");
    fs::write(
        &index_path,
        r#"{
  "id": "bad-index",
  "messages": [
    {"index": 4, "role": "user", "content": "wrong index"}
  ]
}"#,
    )
    .expect("write invalid index");
    let index_error = store
        .load_result("bad-index")
        .expect_err("invalid index must fail");
    assert!(index_error.to_string().contains("message index"));
}

#[test]
fn save_rejects_invalid_message_indices_and_roles_without_replacing_disk_state() {
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path());
    let session = store.create_session();
    store
        .try_append_message(&session.id, "user", "one")
        .expect("user message");
    store
        .try_append_message(&session.id, "assistant", "two")
        .expect("assistant message");
    let path = store.sessions_dir().join(format!("{}.json", session.id));
    let original = fs::read(&path).expect("original session");

    let mut invalid_index = store.load(&session.id).expect("valid session");
    invalid_index.messages[1].index = 9;
    assert!(store.save(&mut invalid_index).is_err());
    assert_eq!(
        fs::read(&path).expect("session after index error"),
        original
    );

    let mut invalid_role = store.load(&session.id).expect("valid session");
    invalid_role.messages[1].role = "user".to_string();
    assert!(store.save(&mut invalid_role).is_err());
    assert_eq!(fs::read(&path).expect("session after role error"), original);
}

#[test]
fn session_ids_cannot_escape_the_sessions_directory() {
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path());
    for id in ["", ".", "..", "../escape", "a/b", r"a\b", "space name"] {
        let error = store
            .load_result(id)
            .expect_err("unsafe session id must fail");
        assert!(error.to_string().contains("invalid session id"), "{error}");
        assert!(store.try_create_session_with_id(id).is_err());
    }
    assert!(!dir.path().join(".nib/escape.json").exists());
}

#[test]
fn concurrent_session_updates_do_not_lose_messages_or_events() {
    const WRITERS: usize = 16;

    let dir = tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let store = SessionStore::new(&root);
    let session = store.create_session();
    store
        .try_append_message(&session.id, "user", "initial")
        .expect("initial message");

    let barrier = Arc::new(Barrier::new(WRITERS));
    let mut threads = Vec::new();
    for writer in 0..WRITERS {
        let root = root.clone();
        let session_id = session.id.clone();
        let barrier = Arc::clone(&barrier);
        threads.push(thread::spawn(move || {
            let store = SessionStore::new(&root);
            barrier.wait();
            store
                .update_session(&session_id, |session| {
                    let assistant_index = session.messages.len();
                    session.messages.push(nib::session::SessionMessage {
                        index: assistant_index,
                        role: "assistant".to_string(),
                        content: format!("assistant-{writer}"),
                        timestamp: None,
                    });
                    let user_index = session.messages.len();
                    session.messages.push(nib::session::SessionMessage {
                        index: user_index,
                        role: "user".to_string(),
                        content: format!("user-{writer}"),
                        timestamp: None,
                    });
                    Ok(())
                })
                .expect("atomic message update");
            store
                .record_event(
                    &session_id,
                    "concurrent_event",
                    serde_json::json!({"writer": writer}),
                )
                .expect("concurrent event");
        }));
    }
    for handle in threads {
        handle.join().expect("writer thread");
    }

    let loaded = store
        .load_result(&session.id)
        .expect("valid final session")
        .expect("final session exists");
    assert_eq!(loaded.messages.len(), 1 + WRITERS * 2);
    assert_eq!(loaded.events.len(), WRITERS);
    loaded.validate().expect("final invariants");
    for writer in 0..WRITERS {
        assert!(loaded
            .messages
            .iter()
            .any(|message| message.content == format!("assistant-{writer}")));
        assert!(loaded
            .messages
            .iter()
            .any(|message| message.content == format!("user-{writer}")));
        assert!(loaded.events.iter().any(|event| {
            event.details.get("writer").and_then(|value| value.as_u64()) == Some(writer as u64)
        }));
    }
}

#[test]
fn child_process_updates_do_not_lose_session_events_or_memory() {
    const WRITERS: usize = 6;
    const ITERATIONS: usize = 10;

    let dir = tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let coordination = root.join("coordination");
    fs::create_dir_all(&coordination).expect("coordination directory");
    let session_store = SessionStore::new(&root);
    let session = session_store.create_session_with_id("process-concurrency");

    let mut children = (0..WRITERS)
        .map(|writer| {
            spawn_store_child(
                "writer",
                &root,
                &session.id,
                &coordination,
                Some(writer),
                Some(ITERATIONS),
            )
        })
        .collect::<Vec<_>>();
    wait_for_ready_children(&mut children, &coordination, WRITERS);
    fs::write(coordination.join("start"), b"start").expect("release child writers");

    for child in children {
        let output = child.wait_with_output().expect("wait for child writer");
        assert!(
            output.status.success(),
            "child failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let session = session_store
        .load_result(&session.id)
        .expect("load cross-process session")
        .expect("cross-process session exists");
    assert_eq!(session.events.len(), WRITERS * ITERATIONS);
    session
        .validate()
        .expect("cross-process session invariants");
    let memory = MemoryStore::new(&root)
        .load_result()
        .expect("load cross-process memory");
    assert_eq!(memory.environment.len(), WRITERS * ITERATIONS);
    for writer in 0..WRITERS {
        for iteration in 0..ITERATIONS {
            let key = format!("writer_{writer}_{iteration}");
            assert_eq!(
                memory.environment.get(&key).map(String::as_str),
                Some("saved")
            );
        }
    }
}

#[test]
fn killed_child_releases_session_lock() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let coordination = root.join("coordination");
    fs::create_dir_all(&coordination).expect("coordination directory");
    let store = SessionStore::new(&root);
    let session = store.create_session_with_id("crash-release");

    let mut child = spawn_store_child(
        "hold-session-lock",
        &root,
        &session.id,
        &coordination,
        None,
        None,
    );
    wait_for_path(&coordination.join("lock-held"), Duration::from_secs(20));
    child.kill().expect("kill lock holder");
    let _ = child.wait().expect("reap lock holder");

    store
        .record_event(&session.id, "after_crash", serde_json::json!({}))
        .expect("kernel lock must be released after child exit");
    assert_eq!(store.load(&session.id).unwrap().events.len(), 1);
}

#[cfg(any(unix, windows))]
#[test]
fn child_process_rejects_session_file_replacement_during_update() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let coordination = root.join("coordination");
    fs::create_dir_all(&coordination).expect("coordination directory");
    let store = SessionStore::new(&root);
    let session = store.create_session_with_id("process-file-replacement");
    let path = store.sessions_dir().join(format!("{}.json", session.id));
    let displaced = store.sessions_dir().join("displaced-session.json");
    let mut replacement = session.clone();
    replacement.summary = Some("replacement-file".to_string());
    let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("serialize");

    let child = spawn_store_child(
        "pause-session-update",
        &root,
        &session.id,
        &coordination,
        None,
        None,
    );
    wait_for_path(&coordination.join("lock-held"), Duration::from_secs(20));
    fs::rename(&path, &displaced).expect("displace session file");
    fs::write(&path, &replacement_bytes).expect("write replacement session");
    fs::write(coordination.join("continue"), b"continue").expect("release child");

    let output = child.wait_with_output().expect("wait for child");
    assert_child_failed(output, "session file replacement");
    assert_eq!(
        fs::read(path).expect("replacement bytes"),
        replacement_bytes
    );
}

#[cfg(any(unix, windows))]
#[test]
fn child_process_rejects_whole_session_directory_replacement() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let coordination = root.join("coordination");
    fs::create_dir_all(&coordination).expect("coordination directory");
    let store = SessionStore::new(&root);
    let session = store.create_session_with_id("process-directory-replacement");
    let sessions_dir = store.sessions_dir().to_path_buf();
    let displaced = root.join(".nib/displaced-sessions");
    #[cfg(unix)]
    let (replacement_path, replacement_bytes) = {
        let replacement_path = sessions_dir.join(format!("{}.json", session.id));
        let mut replacement = session.clone();
        replacement.summary = Some("replacement-directory".to_string());
        let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("serialize");
        (replacement_path, replacement_bytes)
    };
    drop(store);

    let child = spawn_store_child(
        "pause-session-update",
        &root,
        &session.id,
        &coordination,
        None,
        None,
    );
    wait_for_path(&coordination.join("lock-held"), Duration::from_secs(20));

    #[cfg(unix)]
    {
        fs::rename(&sessions_dir, &displaced).expect("displace session directory");
        fs::create_dir(&sessions_dir).expect("create replacement session directory");
        fs::write(&replacement_path, &replacement_bytes).expect("write replacement session");
        fs::write(coordination.join("continue"), b"continue").expect("release child");

        let output = child.wait_with_output().expect("wait for child");
        assert_child_failed(output, "session directory replacement");
        assert_eq!(
            fs::read(&replacement_path).expect("replacement bytes"),
            replacement_bytes
        );
        assert!(SessionStore::new(&root).load_result(&session.id).is_err());
    }

    #[cfg(windows)]
    {
        let rename = fs::rename(&sessions_dir, &displaced);
        fs::write(coordination.join("continue"), b"continue").expect("release child");
        let output = child.wait_with_output().expect("wait for child");

        rename.expect_err("live Windows session lock must pin the sessions directory");
        assert!(
            output.status.success(),
            "child failed after pinned-directory update:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(sessions_dir.is_dir());
        assert!(!displaced.exists());
        assert_eq!(
            SessionStore::new(&root)
                .load_result(&session.id)
                .expect("load original session")
                .expect("original session remains present")
                .summary
                .as_deref(),
            Some("child-update")
        );
    }
}

#[cfg(any(unix, windows))]
#[test]
fn child_process_rejects_replacement_lock_as_a_contender() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let coordination = root.join("coordination");
    fs::create_dir_all(&coordination).expect("coordination directory");
    let store = SessionStore::new(&root);
    let session = store.create_session_with_id("process-lock-replacement");

    let holder = spawn_store_child(
        "pause-session-update",
        &root,
        &session.id,
        &coordination,
        None,
        None,
    );
    wait_for_path(&coordination.join("lock-held"), Duration::from_secs(20));
    let lock_path = fs::read_dir(store.sessions_dir())
        .expect("list locks")
        .map(|entry| entry.expect("lock entry").path())
        .find(|path| {
            path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with(".session-lock-") && name.ends_with(".lock")
            })
        })
        .expect("session stripe lock");
    let displaced = store.sessions_dir().join("displaced-session.lock");
    fs::rename(&lock_path, &displaced).expect("displace session lock");
    fs::write(&lock_path, b"replacement-lock").expect("write replacement lock");

    let contender = spawn_store_child("record-once", &root, &session.id, &coordination, None, None);
    let contender_output = contender.wait_with_output().expect("wait for contender");
    assert_child_failed(contender_output, "replacement lock contender");
    fs::write(coordination.join("continue"), b"continue").expect("release holder");
    let holder_output = holder.wait_with_output().expect("wait for holder");
    assert_child_failed(holder_output, "replacement lock holder");
    assert_eq!(
        fs::read(lock_path).expect("replacement lock bytes"),
        b"replacement-lock"
    );
}

#[test]
fn child_process_store_worker() {
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        return;
    };
    let root = std::path::PathBuf::from(std::env::var(CHILD_ROOT).expect("child root"));
    let session_id = std::env::var(CHILD_SESSION).expect("child session");
    let coordination =
        std::path::PathBuf::from(std::env::var(CHILD_COORDINATION).expect("coordination"));
    let session_store = SessionStore::new(&root);

    if mode == "hold-session-lock" {
        session_store
            .update_session(&session_id, |_session| {
                fs::write(coordination.join("lock-held"), b"held").map_err(|error| {
                    nib::session::SessionError::InvalidMutation(error.to_string())
                })?;
                thread::sleep(Duration::from_secs(120));
                Ok(())
            })
            .expect("hold session lock");
        return;
    }

    if mode == "pause-session-update" {
        session_store
            .update_session(&session_id, |session| {
                fs::write(coordination.join("lock-held"), b"held").map_err(|error| {
                    nib::session::SessionError::InvalidMutation(error.to_string())
                })?;
                wait_for_path(&coordination.join("continue"), Duration::from_secs(20));
                session.summary = Some("child-update".to_string());
                Ok(())
            })
            .expect("paused session update must publish");
        return;
    }

    if mode == "record-once" {
        session_store
            .record_event(&session_id, "contender", serde_json::json!({}))
            .expect("single child update");
        return;
    }

    let writer = std::env::var(CHILD_WRITER)
        .expect("child writer")
        .parse::<usize>()
        .expect("numeric writer");
    let iterations = std::env::var(CHILD_ITERATIONS)
        .expect("child iterations")
        .parse::<usize>()
        .expect("numeric iterations");
    fs::write(coordination.join(format!("ready-{writer}")), b"ready")
        .expect("write child readiness");
    wait_for_path(&coordination.join("start"), Duration::from_secs(20));

    let memory_store = MemoryStore::new(&root);
    for iteration in 0..iterations {
        session_store
            .update_session(&session_id, |session| {
                thread::sleep(Duration::from_millis(4));
                session.events.push(SessionEvent {
                    index: session.events.len(),
                    kind: "child_event".to_string(),
                    details: serde_json::json!({
                        "writer": writer,
                        "iteration": iteration,
                    }),
                    timestamp: Some(Utc::now()),
                });
                Ok(())
            })
            .expect("child session update");
        memory_store
            .update(|memory| {
                thread::sleep(Duration::from_millis(4));
                let key = format!("writer_{writer}_{iteration}");
                memory.environment.insert(key.clone(), "saved".to_string());
                memory.metadata.environment.insert(
                    key,
                    MemoryEntryMetadata {
                        updated_at: Utc::now(),
                    },
                );
                Ok(())
            })
            .expect("child memory update");
    }
}

fn spawn_store_child(
    mode: &str,
    root: &std::path::Path,
    session_id: &str,
    coordination: &std::path::Path,
    writer: Option<usize>,
    iterations: Option<usize>,
) -> Child {
    let mut command = Command::new(std::env::current_exe().expect("current test executable"));
    command
        .arg("--exact")
        .arg("child_process_store_worker")
        .arg("--nocapture")
        .env(CHILD_MODE, mode)
        .env(CHILD_ROOT, root)
        .env(CHILD_SESSION, session_id)
        .env(CHILD_COORDINATION, coordination)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(writer) = writer {
        command.env(CHILD_WRITER, writer.to_string());
    }
    if let Some(iterations) = iterations {
        command.env(CHILD_ITERATIONS, iterations.to_string());
    }
    command.spawn().expect("spawn store child")
}

fn wait_for_ready_children(children: &mut [Child], coordination: &std::path::Path, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let ready = fs::read_dir(coordination)
            .expect("read coordination directory")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("ready-"))
            .count();
        if ready == count {
            return;
        }
        for child in children.iter_mut() {
            if let Some(status) = child.try_wait().expect("poll child") {
                panic!("child exited before readiness: {status}");
            }
        }
        assert!(Instant::now() < deadline, "children did not become ready");
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_path(path: &std::path::Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn assert_child_failed(output: std::process::Output, context: &str) {
    assert!(
        !output.status.success(),
        "{context} unexpectedly succeeded:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn invalid_role_transition_is_rejected_without_losing_audit_state() {
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path());
    let session = store.create_session();

    store
        .try_append_message(&session.id, "user", "first")
        .expect("first user message");
    let error = store
        .try_append_message(&session.id, "user", "invalid duplicate")
        .expect_err("duplicate user role must fail");
    assert!(error.to_string().contains("role transition"));

    store.append_message(&session.id, "user", "audited duplicate");
    store
        .try_append_message(&session.id, "assistant", "response")
        .expect("assistant response");
    store
        .record_skill_usage(&session.id, "rust-safety", Some("matched goal".to_string()))
        .expect("skill usage");

    let loaded = store.load(&session.id).expect("session");
    assert_eq!(loaded.messages.len(), 2);
    assert_eq!(loaded.messages[1].index, 1);
    assert_eq!(loaded.events.len(), 1);
    assert_eq!(loaded.events[0].kind, "role_violation");
    assert_eq!(loaded.active_skills, ["rust-safety"]);
    assert_eq!(
        loaded.skill_usage[0].reason.as_deref(),
        Some("matched goal")
    );
    loaded
        .validate_message_sequence()
        .expect("valid persisted roles");
}
