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
fn rewind_skips_a_workspace_that_is_not_the_sessions() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "v1");
    fixture.before_edit(1, &["a.txt"]);
    fixture.write("a.txt", "v2");
    let other = tempfile::tempdir().expect("other");

    let report = fixture
        .store
        .rewind(1, &[other.path().to_path_buf()])
        .expect("rewind reports the foreign workspace");
    assert_eq!(report.skipped.len(), 1, "{report:?}");
    assert!(report.skipped[0].1.contains("no longer this session's"));
    assert_eq!(report.undo_number, None);
    assert_eq!(fixture.read("a.txt").as_deref(), Some("v2"));
    assert!(fixture.store.rewind(9, &[fixture.root()]).is_err());
}

/// Review finding 1: the undo checkpoint must not prune the data the
/// rewind is about to restore.
#[test]
fn rewinding_to_the_oldest_checkpoint_keeps_its_data() {
    let fixture = Fixture::new();
    for turn in 0..MAX_CHECKPOINTS {
        fixture.write("a.txt", &format!("v{turn}"));
        fixture.before_edit(turn, &["a.txt"]);
    }
    fixture.write("a.txt", "latest");
    let oldest = fixture.store.list().expect("list")[0].number;

    let report = fixture
        .store
        .rewind(oldest, &[fixture.root()])
        .expect("rewind");
    assert!(report.skipped.is_empty(), "{report:?}");
    assert_eq!(fixture.read("a.txt").as_deref(), Some("v0"));
    let checkpoints = fixture.store.list().expect("list");
    assert_eq!(checkpoints.len(), MAX_CHECKPOINTS);
    assert_eq!(
        checkpoints.last().expect("undo").kind,
        CheckpointKind::Rewind
    );
}

/// Review finding 6: a file that already matches is left alone.
#[test]
fn unchanged_files_are_not_rewritten() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "same");
    fixture.before_edit(1, &["a.txt"]);

    let report = fixture.store.rewind(1, &[fixture.root()]).expect("rewind");
    assert!(report.restored.is_empty(), "{report:?}");
    assert!(report.skipped.is_empty(), "{report:?}");
}

/// Review finding 7: a name Git reported lossily is not claimed as saved.
#[test]
fn lossy_names_are_unavailable() {
    let fixture = Fixture::new();
    fixture.before_edit(1, &["bad\u{fffd}name"]);
    let checkpoints = fixture.store.list().expect("list");
    assert!(matches!(
        checkpoints[0].files[0].image,
        FileImage::Unavailable { .. }
    ));
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

/// Review finding 2: a link placed where a file was is replaced, never
/// written through, even when it points at Git metadata inside the
/// workspace (which cap-std alone would allow).
#[cfg(unix)]
#[test]
fn rewind_replaces_a_link_instead_of_following_it() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "v1");
    fixture.write(".git/config", "untouched");
    fixture.before_edit(1, &["a.txt"]);
    std::fs::remove_file(fixture.workspace.path().join("a.txt")).expect("remove");
    std::os::unix::fs::symlink(".git/config", fixture.workspace.path().join("a.txt"))
        .expect("link");

    let report = fixture.store.rewind(1, &[fixture.root()]).expect("rewind");
    assert_eq!(report.restored, vec!["a.txt"], "{report:?}");
    assert_eq!(fixture.read(".git/config").as_deref(), Some("untouched"));
    let restored = fixture.workspace.path().join("a.txt");
    assert!(!std::fs::symlink_metadata(&restored)
        .expect("meta")
        .is_symlink());
    assert_eq!(fixture.read("a.txt").as_deref(), Some("v1"));
}

/// Review finding 2: a parent directory swapped for a link to `.git` is not
/// entered.
#[cfg(unix)]
#[test]
fn rewind_does_not_enter_a_parent_linked_to_git_metadata() {
    let fixture = Fixture::new();
    fixture.write("hooks/pre-commit", "v1");
    fixture.write(".git/hooks/keep", "x");
    fixture.before_edit(1, &["hooks/pre-commit"]);
    std::fs::remove_dir_all(fixture.workspace.path().join("hooks")).expect("remove");
    std::os::unix::fs::symlink(".git/hooks", fixture.workspace.path().join("hooks")).expect("link");

    let report = fixture.store.rewind(1, &[fixture.root()]).expect("rewind");
    assert_eq!(report.skipped.len(), 1, "{report:?}");
    assert!(!fixture
        .workspace
        .path()
        .join(".git/hooks/pre-commit")
        .exists());
}

/// Review finding 2: capture saves a link as a link and never reads
/// through it into `.nib`.
#[cfg(unix)]
#[test]
fn capture_never_reads_through_a_link() {
    let fixture = Fixture::new();
    fixture.write(".nib/credentials", "secret");
    std::os::unix::fs::symlink(".nib/credentials", fixture.workspace.path().join("a.txt"))
        .expect("link");
    fixture.before_edit(1, &["a.txt"]);

    let checkpoints = fixture.store.list().expect("list");
    assert_eq!(
        checkpoints[0].files[0].image,
        FileImage::Symlink {
            target: ".nib/credentials".to_string()
        }
    );
    assert!(!fixture.store.dir.join(BLOB_DIR).exists());
}

/// Review note 3: a private file comes back private.
#[cfg(unix)]
#[test]
fn rewind_restores_permission_bits() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fixture.write("private.txt", "v1");
    let path = fixture.workspace.path().join("private.txt");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    fixture.before_edit(1, &["private.txt"]);
    std::fs::remove_file(&path).expect("remove");

    fixture.store.rewind(1, &[fixture.root()]).expect("rewind");
    let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

/// Review note 1: a temporary file left by an interrupted rewind is
/// removed by the next one in that directory.
#[test]
fn leftover_temporary_files_are_swept() {
    let fixture = Fixture::new();
    fixture.write("a.txt", "v1");
    fixture.before_edit(1, &["a.txt"]);
    fixture.write("a.txt", "v2");
    let leftover = format!(".nib-rewind-{}", "0".repeat(32));
    fixture.write(&leftover, "partial");
    fixture.write(".nib-rewind-notes", "user file");

    fixture.store.rewind(1, &[fixture.root()]).expect("rewind");
    assert_eq!(fixture.read(&leftover), None);
    assert_eq!(
        fixture.read(".nib-rewind-notes").as_deref(),
        Some("user file")
    );
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
