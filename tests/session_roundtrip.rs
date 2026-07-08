//! Integration tests for session persistence (T009).

use nib::session::SessionStore;
use std::fs;
use tempfile::tempdir;

#[test]
fn session_roundtrip_integration() {
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path());

    let session = store.create_session();
    store.append_message(&session.id, "user", "explore the repo");
    store.append_message(&session.id, "assistant", "listing files");

    let loaded = store.load(&session.id).expect("session must exist");
    assert_eq!(loaded.messages.len(), 2);
    assert_eq!(loaded.messages[0].content, "explore the repo");

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
    {"role": "user", "content": "goal", "timestamp": "2026-06-20T12:00:01+00:00"},
    {"role": "assistant", "content": "done"}
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
    assert_eq!(loaded.tool_calls.len(), 1);
    assert_eq!(
        loaded.tool_calls[0].tool_name.as_deref(),
        Some("list_directory")
    );
}
