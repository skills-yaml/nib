use super::*;

fn ledger(state: &Path) -> ReadLedger {
    ReadLedger::for_session(&state.join("sessions"), "session-1").unwrap()
}

fn tracked(root: &Path, path: &str) -> Vec<TrackedPath> {
    tracked_patch_paths(root, Path::new(""), &[path.to_string()]).unwrap()
}

fn read(ledger: &ReadLedger, root: &Path, path: &str) {
    let fingerprint = fingerprint(&root.join(path)).unwrap();
    ledger.record(&[(path.to_string(), fingerprint)]).unwrap();
}

#[test]
fn an_unread_existing_file_cannot_be_edited() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "one\n").unwrap();
    let error = ledger(state.path())
        .check_before_edit(&tracked(root.path(), "a.txt"))
        .unwrap_err();
    assert!(
        error.contains("read a.txt with read_file before editing it"),
        "{error}"
    );
}

#[test]
fn a_file_changed_after_the_read_cannot_be_edited() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let ledger = ledger(state.path());
    std::fs::write(root.path().join("a.txt"), "one\n").unwrap();
    read(&ledger, root.path(), "a.txt");
    ledger
        .check_before_edit(&tracked(root.path(), "a.txt"))
        .unwrap();

    std::fs::write(root.path().join("a.txt"), "two\n").unwrap();
    let error = ledger
        .check_before_edit(&tracked(root.path(), "a.txt"))
        .unwrap_err();
    assert_eq!(
        error,
        "file changed since you read it; re-read a.txt and retry"
    );
}

#[test]
fn new_files_need_no_read_even_when_a_deleted_file_was_read_before() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let ledger = ledger(state.path());
    ledger
        .check_before_edit(&tracked(root.path(), "new.txt"))
        .unwrap();

    // A file the session saw that was then deleted (by a rewind, a command
    // or another session) can be created again: nothing is overwritten,
    // and a re-read of a missing file could never succeed.
    std::fs::write(root.path().join("gone.txt"), "x").unwrap();
    read(&ledger, root.path(), "gone.txt");
    std::fs::remove_file(root.path().join("gone.txt")).unwrap();
    ledger
        .check_before_edit(&tracked(root.path(), "gone.txt"))
        .unwrap();
}

#[test]
fn an_edit_records_the_new_contents_and_a_deletion_forgets_the_file() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let ledger = ledger(state.path());
    std::fs::write(root.path().join("a.txt"), "one\n").unwrap();
    read(&ledger, root.path(), "a.txt");

    std::fs::write(root.path().join("a.txt"), "edited\n").unwrap();
    ledger
        .record_after_edit(&tracked(root.path(), "a.txt"))
        .unwrap();
    ledger
        .check_before_edit(&tracked(root.path(), "a.txt"))
        .unwrap();

    std::fs::remove_file(root.path().join("a.txt")).unwrap();
    ledger
        .record_after_edit(&tracked(root.path(), "a.txt"))
        .unwrap();
    // Recreating the file is a creation again, not a stale edit.
    ledger
        .check_before_edit(&tracked(root.path(), "a.txt"))
        .unwrap();
}

#[test]
fn sessions_do_not_share_reads() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "one\n").unwrap();
    read(&ledger(state.path()), root.path(), "a.txt");
    let other = ReadLedger::for_session(&state.path().join("sessions"), "session-2").unwrap();
    assert!(other
        .check_before_edit(&tracked(root.path(), "a.txt"))
        .is_err());
}

#[test]
fn keys_are_relative_to_the_workspace_top() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("x")).unwrap();
    std::fs::write(root.path().join("x/y.rs"), "").unwrap();
    let paths = tracked_patch_paths(
        root.path(),
        Path::new("sub"),
        &["./x/y.rs".to_string(), "x/new.rs".to_string()],
    )
    .unwrap();
    assert_eq!(paths[0].key, "sub/x/y.rs");
    assert_eq!(paths[1].key, "sub/x/new.rs");
    assert_eq!(ledger_key(Path::new("../x")), None);
    assert_eq!(ledger_key(Path::new("/abs")), None);
    assert_eq!(ledger_key(Path::new("")), None);
}

#[test]
fn paths_that_cannot_be_keyed_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    for path in ["f\u{FFFD}.txt", "../outside.txt", "/abs.txt"] {
        let error = tracked_patch_paths(root.path(), Path::new(""), &[path.to_string()])
            .err()
            .unwrap_or_else(|| panic!("{path} was keyed"));
        assert!(error.starts_with("cannot check "), "{error}");
    }
}

#[test]
fn large_files_are_fingerprinted_from_metadata() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("big.bin");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(MAX_HASHED_BYTES + 1).unwrap();
    drop(file);
    let first = fingerprint(&path).unwrap().unwrap();
    assert!(first.starts_with("meta:"), "{first}");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(MAX_HASHED_BYTES + 2)
        .unwrap();
    assert_ne!(fingerprint(&path).unwrap().unwrap(), first);
}

#[test]
fn a_damaged_record_starts_over() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let ledger = ledger(state.path());
    std::fs::create_dir_all(state.path().join(LEDGER_DIR)).unwrap();
    std::fs::write(state.path().join(LEDGER_DIR).join("session-1.json"), "{").unwrap();
    std::fs::write(root.path().join("a.txt"), "one\n").unwrap();
    read(&ledger, root.path(), "a.txt");
    ledger
        .check_before_edit(&tracked(root.path(), "a.txt"))
        .unwrap();
}

#[test]
fn the_oldest_reads_are_forgotten_beyond_the_cap() {
    let state = tempfile::tempdir().unwrap();
    let ledger = ledger(state.path());
    let seen: Vec<_> = (0..=MAX_ENTRIES)
        .map(|index| (format!("f{index}"), Some("hash".to_string())))
        .collect();
    ledger.record(&seen).unwrap();
    let saved = ledger.load().unwrap();
    assert_eq!(saved.files.len(), MAX_ENTRIES);
    assert!(!saved.files.contains_key("f0"));
    assert!(saved.files.contains_key(&format!("f{MAX_ENTRIES}")));
}

#[test]
fn invalid_session_ids_are_rejected() {
    let state = tempfile::tempdir().unwrap();
    assert!(ReadLedger::for_session(&state.path().join("sessions"), "../x").is_err());
    assert!(ReadLedger::for_session(&state.path().join("sessions"), "").is_err());
}

#[cfg(unix)]
#[test]
fn links_are_fingerprinted_by_target_and_fifos_are_never_opened() {
    let root = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("a", root.path().join("link")).unwrap();
    let first = fingerprint(&root.path().join("link")).unwrap().unwrap();
    std::fs::remove_file(root.path().join("link")).unwrap();
    std::os::unix::fs::symlink("b", root.path().join("link")).unwrap();
    let second = fingerprint(&root.path().join("link")).unwrap().unwrap();
    assert!(first.starts_with("link:"));
    assert_ne!(first, second);

    let fifo = root.path().join("fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert_eq!(fingerprint(&fifo).unwrap().as_deref(), Some("special"));
}
