use super::*;

fn write_heartbeat(presence: &Presence, session_id: &str, folder: &Path, age: Duration) {
    std::fs::create_dir_all(&presence.dir).unwrap();
    let heartbeat = Heartbeat {
        session_id: session_id.to_string(),
        folder: folder.to_path_buf(),
        updated_at: unix_seconds(SystemTime::now() - age),
    };
    std::fs::write(
        presence.file(session_id),
        serde_json::to_vec(&heartbeat).unwrap(),
    )
    .unwrap();
}

#[test]
fn sessions_in_one_folder_see_each_other_and_leave_on_drop() {
    let state = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let sessions = state.path().join("sessions");
    let now = Instant::now();
    let mut first = Presence::new(&sessions, folder.path());
    let mut second = Presence::new(&sessions, folder.path());
    assert_eq!(first.tick("one", now), 0);
    assert_eq!(second.tick("two", now), 1);
    // The first session re-counts on its next scan.
    assert_eq!(first.tick("one", now + SCAN_INTERVAL), 1);

    drop(second);
    assert_eq!(first.tick("one", now + SCAN_INTERVAL * 2), 0);
}

#[test]
fn other_folders_and_stale_heartbeats_do_not_count() {
    let state = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let mut presence = Presence::new(&state.path().join("sessions"), folder.path());
    let own_folder = presence.folder.clone();
    write_heartbeat(&presence, "elsewhere", elsewhere.path(), Duration::ZERO);
    write_heartbeat(&presence, "crashed", &own_folder, PRESENCE_TIMEOUT);
    write_heartbeat(&presence, "live", &own_folder, Duration::from_secs(10));
    write_heartbeat(&presence, "ancient", &own_folder, STALE_FILE_AGE);
    assert_eq!(presence.tick("me", Instant::now()), 1);
    // A day-old heartbeat file is cleaned up; a recently crashed one stays.
    assert!(!presence.file("ancient").exists());
    assert!(presence.file("crashed").exists());
}

#[test]
fn switching_sessions_moves_the_heartbeat() {
    let state = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let mut presence = Presence::new(&state.path().join("sessions"), folder.path());
    let now = Instant::now();
    presence.tick("one", now);
    assert!(presence.file("one").exists());
    presence.tick("two", now);
    assert!(!presence.file("one").exists());
    assert!(presence.file("two").exists());
}

#[test]
fn invalid_session_ids_write_nothing() {
    let state = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let mut presence = Presence::new(&state.path().join("sessions"), folder.path());
    assert_eq!(presence.tick("../escape", Instant::now()), 0);
    assert!(!presence.dir.exists());
}

#[test]
fn a_background_heartbeat_is_visible_until_dropped() {
    let state = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let sessions = state.path().join("sessions");
    let heartbeat = PresenceHeartbeat::start(&sessions, folder.path(), "headless");
    let mut watcher = Presence::new(&sessions, folder.path());
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut start = Instant::now();
    while watcher.tick("tui", start) == 0 {
        assert!(
            Instant::now() < deadline,
            "the background heartbeat never appeared"
        );
        std::thread::sleep(Duration::from_millis(20));
        start += SCAN_INTERVAL;
    }
    drop(heartbeat);
    assert_eq!(watcher.tick("tui", start + SCAN_INTERVAL), 0);
}

#[test]
fn labels_name_the_other_sessions() {
    assert_eq!(other_sessions_label(0), None);
    assert_eq!(
        other_sessions_label(1).as_deref(),
        Some("1 other session in this folder")
    );
    assert_eq!(
        other_sessions_label(3).as_deref(),
        Some("3 other sessions in this folder")
    );
}
