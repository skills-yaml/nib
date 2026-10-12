use super::*;

struct Fixture {
    _state: tempfile::TempDir,
    workspace: tempfile::TempDir,
    store: CheckpointStore,
}

impl Fixture {
    fn new() -> Self {
        let state = tempfile::tempdir().expect("state");
        let sessions = state.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");
        let workspace = tempfile::tempdir().expect("workspace");
        let store = CheckpointStore::for_session(&sessions, "session-1").expect("store");
        Self {
            _state: state,
            workspace,
            store,
        }
    }

    fn root(&self) -> PathBuf {
        self.workspace.path().canonicalize().expect("root")
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.workspace.path().join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(path, content).expect("write");
    }

    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.workspace.path().join(path)).ok()
    }

    fn before_edit(&self, turn: usize, paths: &[&str]) {
        let paths: Vec<String> = paths.iter().map(|path| path.to_string()).collect();
        self.store
            .record_before_edit(&marker(turn), self.workspace.path(), &paths)
            .expect("record");
    }
}

fn marker(turn: usize) -> TurnMarker {
    TurnMarker {
        turn,
        label: format!("request {turn}"),
    }
}

#[test]
fn rewind_restores_contents_creations_and_deletions() {
    let fixture = Fixture::new();
    fixture.write("src/lib.rs", "original");
    fixture.write("gone.txt", "keep me");

    fixture.before_edit(1, &["src/lib.rs", "new.txt", "gone.txt"]);
    fixture.write("src/lib.rs", "edited");
    fixture.write("new.txt", "created");
    std::fs::remove_file(fixture.workspace.path().join("gone.txt")).expect("delete");

    let report = fixture.store.rewind(1, &[fixture.root()]).expect("rewind");

    assert_eq!(fixture.read("src/lib.rs").as_deref(), Some("original"));
    assert_eq!(fixture.read("gone.txt").as_deref(), Some("keep me"));
    assert_eq!(fixture.read("new.txt"), None);
    assert_eq!(report.restored, vec!["gone.txt", "src/lib.rs"]);
    assert_eq!(report.removed, vec!["new.txt"]);
    assert!(report.skipped.is_empty());
    assert!(!report.commands);
}

#[test]
fn one_checkpoint_per_turn_keeps_the_first_image() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "v1");
    fixture.before_edit(1, &["a.txt"]);
    fixture.write("a.txt", "v2");
    fixture.before_edit(1, &["a.txt"]);
    fixture.write("a.txt", "v3");
    fixture.before_edit(2, &["a.txt"]);
    fixture.write("a.txt", "v4");

    let checkpoints = fixture.store.list().expect("list");
    assert_eq!(checkpoints.len(), 2);
    assert_eq!(checkpoints[0].files.len(), 1);
    assert_eq!(checkpoints[0].label, "request 1");

    fixture
        .store
        .rewind(2, &[fixture.root()])
        .expect("rewind to 2");
    assert_eq!(fixture.read("a.txt").as_deref(), Some("v3"));
    fixture
        .store
        .rewind(1, &[fixture.root()])
        .expect("rewind to 1");
    assert_eq!(fixture.read("a.txt").as_deref(), Some("v1"));
}

#[test]
fn a_rewind_can_be_undone() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "before");
    fixture.before_edit(1, &["a.txt"]);
    fixture.write("a.txt", "after");

    let report = fixture.store.rewind(1, &[fixture.root()]).expect("rewind");
    assert_eq!(fixture.read("a.txt").as_deref(), Some("before"));
    let undo = report.undo_number.expect("undo checkpoint");
    let checkpoints = fixture.store.list().expect("list");
    let last = checkpoints.last().expect("undo entry");
    assert_eq!(last.kind, CheckpointKind::Rewind);
    assert_eq!(last.label, "before /rewind to #1");

    fixture.store.rewind(undo, &[fixture.root()]).expect("undo");
    assert_eq!(fixture.read("a.txt").as_deref(), Some("after"));
}

#[test]
fn edits_after_a_rewind_open_a_new_checkpoint_in_the_same_turn() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "v1");
    fixture.before_edit(1, &["a.txt"]);
    fixture.write("a.txt", "v2");
    fixture.store.rewind(1, &[fixture.root()]).expect("rewind");
    fixture.before_edit(1, &["a.txt"]);

    let checkpoints = fixture.store.list().expect("list");
    assert_eq!(checkpoints.len(), 3);
    assert_eq!(checkpoints[2].kind, CheckpointKind::Turn);
    assert_eq!(checkpoints[2].number, 3);
}

#[test]
fn commands_are_flagged_not_restored() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "v1");
    fixture
        .store
        .record_command(&marker(1), fixture.workspace.path())
        .expect("command");
    fixture.before_edit(1, &["a.txt"]);
    fixture.write("a.txt", "v2");

    let checkpoints = fixture.store.list().expect("list");
    assert_eq!(checkpoints.len(), 1);
    assert!(checkpoints[0].commands);
    let report = fixture.store.rewind(1, &[fixture.root()]).expect("rewind");
    assert!(report.commands);
    assert_eq!(fixture.read("a.txt").as_deref(), Some("v1"));
}

#[test]
fn rewind_refuses_a_workspace_that_is_not_the_sessions() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "v1");
    fixture.before_edit(1, &["a.txt"]);
    fixture.write("a.txt", "v2");
    let other = tempfile::tempdir().expect("other");

    let error = fixture
        .store
        .rewind(1, &[other.path().to_path_buf()])
        .expect_err("foreign workspace");
    assert!(error.contains("not this session's workspace"), "{error}");
    assert_eq!(fixture.read("a.txt").as_deref(), Some("v2"));
    assert!(fixture.store.rewind(9, &[fixture.root()]).is_err());
}

#[test]
fn git_and_nib_paths_are_never_saved_or_written() {
    for path in [
        ".git/config",
        ".nib/settings",
        "a/.GIT/x",
        "../escape",
        "/abs",
        "x/.nib./y",
    ] {
        assert_eq!(safe_relative(path), None, "{path}");
    }
    assert_eq!(safe_relative("src/a.rs"), Some(PathBuf::from("src/a.rs")));

    let fixture = Fixture::new();
    fixture.before_edit(1, &[".git/hooks/pre-commit"]);
    let checkpoints = fixture.store.list().expect("list");
    assert!(matches!(
        checkpoints[0].files[0].image,
        FileImage::Unavailable { .. }
    ));
    fixture.write(".git/hooks/pre-commit", "planted");
    let report = fixture.store.rewind(1, &[fixture.root()]).expect("rewind");
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(
        fixture.read(".git/hooks/pre-commit").as_deref(),
        Some("planted")
    );
}

#[cfg(unix)]
#[test]
fn rewind_does_not_write_through_a_symlinked_directory() {
    let fixture = Fixture::new();
    fixture.write("dir/a.txt", "v1");
    fixture.before_edit(1, &["dir/a.txt"]);
    std::fs::remove_dir_all(fixture.workspace.path().join("dir")).expect("remove dir");
    let elsewhere = fixture.workspace.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("elsewhere");
    std::os::unix::fs::symlink(&elsewhere, fixture.workspace.path().join("dir")).expect("link");

    let report = fixture.store.rewind(1, &[fixture.root()]).expect("rewind");
    assert_eq!(report.skipped.len(), 1, "{report:?}");
    assert!(!elsewhere.join("a.txt").exists());
}

#[cfg(unix)]
#[test]
fn rewind_restores_the_executable_bit_and_symlinks() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.write("run.sh", "#!/bin/sh\n");
    let script = fixture.workspace.path().join("run.sh");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::os::unix::fs::symlink("run.sh", fixture.workspace.path().join("link")).expect("link");
    fixture.before_edit(1, &["run.sh", "link"]);
    std::fs::write(&script, "changed").expect("write");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    std::fs::remove_file(fixture.workspace.path().join("link")).expect("unlink");

    fixture.store.rewind(1, &[fixture.root()]).expect("rewind");
    let mode = std::fs::metadata(&script)
        .expect("meta")
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111);
    assert_eq!(
        std::fs::read_link(fixture.workspace.path().join("link")).expect("link"),
        PathBuf::from("run.sh")
    );
}

#[test]
fn damaged_blobs_are_skipped() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "v1");
    fixture.before_edit(1, &["a.txt"]);
    fixture.write("a.txt", "v2");
    let blobs = fixture.store.dir.join(BLOB_DIR);
    for entry in std::fs::read_dir(&blobs).expect("blobs") {
        std::fs::write(entry.expect("entry").path(), "tampered").expect("tamper");
    }

    let report = fixture.store.rewind(1, &[fixture.root()]).expect("rewind");
    assert_eq!(report.skipped.len(), 1);
    assert!(report.skipped[0].1.contains("damaged"));
    assert_eq!(fixture.read("a.txt").as_deref(), Some("v2"));
}

#[test]
fn old_checkpoints_and_their_blobs_are_pruned() {
    let fixture = Fixture::new();
    for turn in 0..(MAX_CHECKPOINTS + 5) {
        fixture.write("a.txt", &format!("v{turn}"));
        fixture.before_edit(turn, &["a.txt"]);
    }
    let checkpoints = fixture.store.list().expect("list");
    assert_eq!(checkpoints.len(), MAX_CHECKPOINTS);
    assert_eq!(checkpoints[0].number, 6);
    let blobs = std::fs::read_dir(fixture.store.dir.join(BLOB_DIR))
        .expect("blobs")
        .count();
    assert_eq!(blobs, MAX_CHECKPOINTS);
}

#[test]
fn labels_are_one_short_line() {
    assert_eq!(short_label("fix it\nplease"), "fix it");
    let long = "x".repeat(200);
    assert_eq!(short_label(&long).chars().count(), MAX_LABEL_CHARS);
    assert!(CheckpointStore::for_session(Path::new("/s/sessions"), "../x").is_err());
}
