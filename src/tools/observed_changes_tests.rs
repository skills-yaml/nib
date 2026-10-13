use super::*;

fn changes(before: &Snapshot, root: &Path) -> ObservedChanges {
    Snapshot::take(root).changes_since(before)
}

#[test]
fn creations_deletions_and_rewrites_are_observed() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("kept.txt"), "same").unwrap();
    std::fs::write(root.path().join("gone.txt"), "x").unwrap();
    std::fs::write(root.path().join("edited.txt"), "one").unwrap();
    let before = Snapshot::take(root.path());

    std::fs::remove_file(root.path().join("gone.txt")).unwrap();
    std::fs::write(root.path().join("edited.txt"), "two!").unwrap();
    std::fs::create_dir(root.path().join("new")).unwrap();
    std::fs::write(root.path().join("new/file.txt"), "").unwrap();

    assert_eq!(
        changes(&before, root.path()),
        ObservedChanges {
            paths: vec![
                "edited.txt".to_string(),
                "gone.txt".to_string(),
                "new".to_string(),
                "new/file.txt".to_string(),
            ],
            complete: true,
        }
    );
}

#[test]
fn an_unchanged_project_has_no_changes() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "fn main() {}").unwrap();
    let before = Snapshot::take(root.path());
    let observed = changes(&before, root.path());
    assert!(observed.paths.is_empty(), "{observed:?}");
    assert!(observed.complete);
}

#[test]
fn state_and_build_directories_are_not_observed() {
    let root = tempfile::tempdir().unwrap();
    for directory in [".git", ".nib", "target", "node_modules", "crate/target"] {
        std::fs::create_dir_all(root.path().join(directory)).unwrap();
    }
    let before = Snapshot::take(root.path());
    for directory in [".git", ".nib", "target", "node_modules", "crate/target"] {
        std::fs::write(root.path().join(directory).join("out"), "x").unwrap();
    }
    let observed = changes(&before, root.path());
    assert!(observed.paths.is_empty(), "{observed:?}");
}

#[test]
fn a_cut_short_snapshot_is_incomplete() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..5 {
        std::fs::write(root.path().join(format!("f{index}")), "").unwrap();
    }
    let before = Snapshot::take_bounded(root.path(), 3);
    let observed = Snapshot::take(root.path()).changes_since(&before);
    assert!(!observed.complete);
}

#[cfg(unix)]
#[test]
fn a_retargeted_link_is_observed_without_following_it() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "one").unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("out")).unwrap();
    std::os::unix::fs::symlink("a", root.path().join("link")).unwrap();
    let before = Snapshot::take(root.path());

    // A change behind a link to a directory is not walked.
    std::fs::write(outside.path().join("secret"), "two").unwrap();
    std::fs::remove_file(root.path().join("link")).unwrap();
    std::os::unix::fs::symlink("b", root.path().join("link")).unwrap();
    assert_eq!(
        changes(&before, root.path()).paths,
        vec!["link".to_string()]
    );
}
