use super::*;
use std::fs::File;
use tempfile::tempdir;

fn removal_deadline() -> std::time::Instant {
    std::time::Instant::now() + std::time::Duration::from_secs(5)
}

#[test]
fn file_identity_matches_reopened_file_and_hard_link() {
    let root = tempdir().expect("tempdir");
    let original = root.path().join("original");
    let linked = root.path().join("linked");
    std::fs::write(&original, b"identity").expect("write original");
    std::fs::hard_link(&original, &linked).expect("create hard link");

    let original_identity = FileIdentity::from_file(File::open(&original).expect("open original"))
        .expect("identify original");
    let reopened_identity =
        FileIdentity::from_file(File::open(&original).expect("reopen original"))
            .expect("identify reopened original");
    let linked_identity = FileIdentity::from_file(File::open(&linked).expect("open hard link"))
        .expect("identify hard link");

    assert_eq!(original_identity, reopened_identity);
    assert_eq!(original_identity, linked_identity);
}

#[test]
fn file_identity_distinguishes_same_sized_files() {
    let root = tempdir().expect("tempdir");
    let first = root.path().join("first");
    let second = root.path().join("second");
    std::fs::write(&first, b"same-size-a").expect("write first");
    std::fs::write(&second, b"same-size-b").expect("write second");

    let first_identity =
        FileIdentity::from_file(File::open(first).expect("open first")).expect("identify first");
    let second_identity =
        FileIdentity::from_file(File::open(second).expect("open second")).expect("identify second");

    assert_ne!(first_identity, second_identity);
}

#[test]
fn creates_local_components_and_rejects_parent_traversal() {
    let root = tempdir().expect("tempdir");
    let path = root.path().join("one/two");
    assert_eq!(
        ensure_directory_without_symlinks(&path).unwrap(),
        path.canonicalize().unwrap()
    );
    assert!(path.is_dir());
    assert!(ensure_directory_without_symlinks(&root.path().join("one/../escape")).is_err());
}

#[test]
fn capability_bound_removal_deletes_only_the_opened_child_tree() {
    let root = tempdir().expect("tempdir");
    let child = root.path().join("child");
    std::fs::create_dir(&child).expect("child directory");
    std::fs::write(child.join("nested"), b"fixture").expect("nested file");

    remove_directory_tree_capability_bound(root.path(), &child, removal_deadline())
        .expect("capability-bound removal");

    assert!(!child.exists());
    assert!(root.path().is_dir());
}

#[test]
fn receipt_bound_removal_preserves_a_visible_replacement() {
    let root = tempdir().expect("tempdir");
    let child = root.path().join("child");
    let displaced = root.path().join("displaced-owned-child");
    std::fs::create_dir(&child).expect("owned child directory");
    std::fs::write(child.join("owned"), b"owned").expect("owned file");
    let receipt = capture_directory_removal_receipt(root.path(), &child)
        .expect("directory ownership receipt");
    std::fs::rename(&child, &displaced).expect("displace owned directory");
    std::fs::create_dir(&child).expect("replacement directory");
    std::fs::write(child.join("sentinel"), b"replacement").expect("replacement sentinel");

    let error = remove_directory_tree_capability_bound_if_matches(
        root.path(),
        &child,
        receipt,
        removal_deadline(),
    )
    .expect_err("receipt mismatch must preserve the replacement");

    assert!(error.to_string().contains("ownership receipt"), "{error}");
    assert_eq!(
        std::fs::read(child.join("sentinel")).expect("preserved replacement"),
        b"replacement"
    );
    assert_eq!(
        std::fs::read(displaced.join("owned")).expect("preserved owned directory"),
        b"owned"
    );
}

#[test]
fn capability_bound_removal_preserves_a_top_level_replacement() {
    let root = tempdir().expect("tempdir");
    let child = root.path().join("child");
    let replacement = root.path().join("replacement");
    std::fs::create_dir(&child).expect("child directory");
    std::fs::write(child.join("owned"), b"owned").expect("owned file");
    std::fs::create_dir(&replacement).expect("replacement directory");
    std::fs::write(replacement.join("replacement"), b"replacement").expect("replacement file");
    REPLACE_AFTER_REMOVAL_QUARANTINE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(child.clone(), replacement);

    let error = remove_directory_tree_capability_bound(root.path(), &child, removal_deadline())
        .expect_err("replacement must make removal fail closed");

    assert!(
        error.to_string().contains("replaced after quarantine"),
        "unexpected error: {error}"
    );
    assert_eq!(
        std::fs::read(child.join("replacement")).expect("preserved replacement"),
        b"replacement"
    );
    let quarantines = std::fs::read_dir(root.path())
        .expect("read root")
        .map(|entry| entry.expect("root entry").path())
        .filter(|path| {
            path.file_name()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|name| name.starts_with(".nib-cleanup-"))
        })
        .collect::<Vec<_>>();
    assert_eq!(quarantines.len(), 1, "owned tree remains quarantined");
    assert_eq!(
        std::fs::read(quarantines[0].join("owned")).expect("quarantined owned file"),
        b"owned"
    );

    let retry = remove_directory_tree_capability_bound(root.path(), &child, removal_deadline())
        .expect_err("ambiguous source and quarantine must remain preserved");
    assert!(retry.to_string().contains("preserving both"), "{retry}");
    let quarantine_count = std::fs::read_dir(root.path())
        .expect("read root after retry")
        .map(|entry| entry.expect("root entry after retry").file_name())
        .filter(|name| {
            name.to_str()
                .is_some_and(|name| name.starts_with(".nib-cleanup-"))
        })
        .count();
    assert_eq!(quarantine_count, 1, "retry created another quarantine");
}

#[test]
fn capability_bound_removal_preserves_an_unproven_quarantine_only_state() {
    let root = tempdir().expect("tempdir");
    let child = root.path().join("child");
    let quarantine = root
        .path()
        .join(removal_quarantine_name(Path::new("child")));
    std::fs::create_dir(&quarantine).expect("forged quarantine");
    std::fs::write(quarantine.join("sentinel"), b"unproven").expect("sentinel");

    let error = remove_directory_tree_capability_bound(root.path(), &child, removal_deadline())
        .expect_err("quarantine-only state must be unproven");

    assert!(error.to_string().contains("unproven"), "{error}");
    assert_eq!(
        std::fs::read(quarantine.join("sentinel")).expect("preserved sentinel"),
        b"unproven"
    );
    assert!(!child.exists());
}

#[test]
fn capability_bound_removal_honors_an_expired_deadline_without_mutation() {
    let root = tempdir().expect("tempdir");
    let child = root.path().join("child");
    std::fs::create_dir(&child).expect("child directory");
    std::fs::write(child.join("sentinel"), b"preserved").expect("sentinel");

    let error =
        remove_directory_tree_capability_bound(root.path(), &child, std::time::Instant::now())
            .expect_err("expired deadline must fail");

    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(
        std::fs::read(child.join("sentinel")).expect("preserved sentinel"),
        b"preserved"
    );
}

#[cfg(unix)]
#[test]
fn unix_entry_replacement_after_quarantine_is_preserved() {
    let root = tempdir().expect("tempdir");
    let child = root.path().join("child");
    let victim = PathBuf::from("z-victim");
    let replacement = PathBuf::from("a-replacement");
    std::fs::create_dir(&child).expect("child directory");
    std::fs::write(child.join(&victim), b"owned").expect("owned file");
    std::fs::write(child.join(&replacement), b"replacement").expect("replacement file");
    REPLACE_AFTER_REMOVAL_ENTRY_QUARANTINE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(victim.clone(), replacement);

    let error = remove_directory_tree_capability_bound(root.path(), &child, removal_deadline())
        .expect_err("entry replacement must fail closed");

    assert!(
        error.to_string().contains("replacement preserved"),
        "{error}"
    );
    let top_quarantine = root
        .path()
        .join(removal_quarantine_name(Path::new("child")));
    assert_eq!(
        std::fs::read(top_quarantine.join(&victim)).expect("preserved replacement"),
        b"replacement"
    );
    let entry_quarantines = std::fs::read_dir(&top_quarantine)
        .expect("read top quarantine")
        .map(|entry| entry.expect("top quarantine entry").path())
        .filter(|path| {
            path.file_name()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|name| name.starts_with(".nib-entry-cleanup-"))
        })
        .collect::<Vec<_>>();
    assert_eq!(entry_quarantines.len(), 1);
    assert_eq!(
        std::fs::read(&entry_quarantines[0]).expect("preserved owned entry"),
        b"owned"
    );
}

#[cfg(unix)]
#[test]
fn unix_quarantine_replacement_before_unlink_is_preserved() {
    let root = tempdir().expect("tempdir");
    let child = root.path().join("child");
    let victim = PathBuf::from("z-final-victim");
    let replacement = PathBuf::from("a-final-replacement");
    std::fs::create_dir(&child).expect("child directory");
    std::fs::write(child.join(&victim), b"owned").expect("owned file");
    std::fs::write(child.join(&replacement), b"replacement").expect("replacement file");
    REPLACE_REMOVAL_QUARANTINE_BEFORE_UNLINK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(victim.clone(), replacement);

    let error = remove_directory_tree_capability_bound(root.path(), &child, removal_deadline())
        .expect_err("quarantine substitution must fail closed");

    assert!(error.to_string().contains("identity changed"), "{error}");
    let top_quarantine = root
        .path()
        .join(removal_quarantine_name(Path::new("child")));
    let entry_quarantine = top_quarantine.join(removal_entry_quarantine_name(&victim));
    assert_eq!(
        std::fs::read(entry_quarantine).expect("preserved quarantine replacement"),
        b"replacement"
    );
    let displaced = std::fs::read_dir(&top_quarantine)
        .expect("read top quarantine")
        .map(|entry| entry.expect("top quarantine entry").path())
        .find(|path| {
            path.file_name()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|name| name.starts_with(".nib-test-displaced-entry-"))
        })
        .expect("displaced opened entry");
    assert_eq!(
        std::fs::read(displaced).expect("preserved displaced entry"),
        b"owned"
    );
}

#[cfg(unix)]
#[test]
fn unix_symlink_quarantine_replacement_before_unlink_is_preserved() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("tempdir");
    let child = root.path().join("child");
    let victim = PathBuf::from("z-final-link");
    let replacement = PathBuf::from("a-link-replacement");
    std::fs::create_dir(&child).expect("child directory");
    symlink("target", child.join(&victim)).expect("owned symlink");
    std::fs::write(child.join(&replacement), b"replacement").expect("replacement file");
    REPLACE_REMOVAL_QUARANTINE_BEFORE_UNLINK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(victim.clone(), replacement);

    let error = remove_directory_tree_capability_bound(root.path(), &child, removal_deadline())
        .expect_err("symlink quarantine substitution must fail closed");

    assert!(error.to_string().contains("identity changed"), "{error}");
    let top_quarantine = root
        .path()
        .join(removal_quarantine_name(Path::new("child")));
    let entry_quarantine = top_quarantine.join(removal_entry_quarantine_name(&victim));
    assert_eq!(
        std::fs::read(entry_quarantine).expect("preserved quarantine replacement"),
        b"replacement"
    );
    let displaced = std::fs::read_dir(&top_quarantine)
        .expect("read top quarantine")
        .map(|entry| entry.expect("top quarantine entry").path())
        .find(|path| {
            path.file_name()
                .and_then(std::ffi::OsStr::to_str)
                .is_some_and(|name| name.starts_with(".nib-test-displaced-entry-"))
        })
        .expect("displaced opened symlink");
    assert!(std::fs::symlink_metadata(&displaced)
        .expect("displaced symlink metadata")
        .file_type()
        .is_symlink());
    assert_eq!(
        std::fs::read_link(displaced).expect("symlink target"),
        Path::new("target")
    );
}

#[cfg(unix)]
#[test]
fn unix_open_directory_removal_preserves_a_boundary_replacement() {
    let root = tempdir().expect("tempdir");
    let child = root.path().join("child");
    let replacement = root.path().join("replacement");
    std::fs::create_dir(&child).expect("child directory");
    std::fs::create_dir(&replacement).expect("replacement directory");
    std::fs::write(replacement.join("sentinel"), b"replacement").expect("replacement sentinel");
    REPLACE_DIRECTORY_BEFORE_HANDLE_DELETE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(child.clone(), PathBuf::from("replacement"));

    let error = remove_directory_tree_capability_bound(root.path(), &child, removal_deadline())
        .expect_err("directory boundary substitution must fail closed");

    assert!(
        error.to_string().contains("replacement preserved"),
        "{error}"
    );
    let top_quarantine = root
        .path()
        .join(removal_quarantine_name(Path::new("child")));
    assert_eq!(
        std::fs::read(top_quarantine.join("sentinel")).expect("preserved boundary replacement"),
        b"replacement"
    );
    assert!(!child.exists());
    assert!(!replacement.exists());
    assert!(!std::fs::read_dir(root.path())
        .expect("read root")
        .map(|entry| entry.expect("root entry").file_name())
        .any(|name| {
            name.to_str()
                .is_some_and(|name| name.starts_with(".nib-test-displaced-directory-"))
        }));
}

#[cfg(unix)]
#[test]
fn unix_preexisting_entry_quarantine_is_preserved_as_ambiguous() {
    let root = tempdir().expect("tempdir");
    let child = root.path().join("child");
    let victim = PathBuf::from("victim");
    let collision = removal_entry_quarantine_name(&victim);
    std::fs::create_dir(&child).expect("child directory");
    std::fs::write(child.join(&victim), b"owned").expect("owned file");
    std::fs::write(child.join(&collision), b"unproven").expect("unproven collision");

    let error = remove_directory_tree_capability_bound(root.path(), &child, removal_deadline())
        .expect_err("per-entry quarantine collision must fail closed");

    assert!(error.to_string().contains("unproven per-entry"), "{error}");
    assert_eq!(
        std::fs::read(child.join(&victim)).expect("preserved owned file"),
        b"owned"
    );
    assert_eq!(
        std::fs::read(child.join(&collision)).expect("preserved collision"),
        b"unproven"
    );
    assert!(!root
        .path()
        .join(removal_quarantine_name(Path::new("child")))
        .exists());
}

#[cfg(windows)]
#[test]
fn windows_verbatim_prefixes_compare_as_the_same_canonical_path() {
    assert!(canonical_paths_match(
        Path::new(r"\\?\C:\nib\state"),
        Path::new(r"C:\nib\state")
    ));
    assert!(canonical_paths_match(
        Path::new(r"\\?\UNC\server\share\nib"),
        Path::new(r"\\server\share\nib")
    ));
    assert!(!canonical_paths_match(
        Path::new(r"\\?\C:\nib\other"),
        Path::new(r"C:\nib\state")
    ));
}

#[cfg(windows)]
#[test]
fn windows_verbatim_canonical_prefix_accepts_a_real_directory() {
    let root = tempdir().expect("tempdir");
    let nested = root.path().join("one/two");

    assert_eq!(
        ensure_directory_without_symlinks(&nested).expect("safe directory"),
        nested.canonicalize().expect("canonical directory")
    );
    verify_directory_without_symlinks(&nested).expect("verify safe directory");
}

#[cfg(windows)]
#[test]
fn windows_dos_short_alias_accepts_a_real_directory() {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

    let root = tempdir().expect("tempdir");
    let canonical = root.path().canonicalize().expect("canonical tempdir");
    let long_path = path_without_windows_verbatim_prefix(&canonical);
    let mut input = long_path.as_os_str().encode_wide().collect::<Vec<_>>();
    input.push(0);
    let required = unsafe { GetShortPathNameW(input.as_ptr(), std::ptr::null_mut(), 0) };
    if required == 0 {
        panic!(
            "failed to size the DOS short-path buffer: {}",
            std::io::Error::last_os_error()
        );
    }
    let mut output = vec![0_u16; required as usize];
    let written = unsafe { GetShortPathNameW(input.as_ptr(), output.as_mut_ptr(), required) };
    if written == 0 {
        panic!(
            "failed to resolve the DOS short path: {}",
            std::io::Error::last_os_error()
        );
    }
    assert!(
        (written as usize) < output.len(),
        "DOS short-path output exceeded its sized buffer"
    );
    output.truncate(written as usize);
    let short_path = PathBuf::from(OsString::from_wide(&output));
    if short_path == long_path {
        return;
    }

    assert_eq!(
        ensure_directory_without_symlinks(&short_path).expect("safe DOS short alias"),
        canonical
    );
    verify_directory_without_symlinks(&short_path).expect("verify DOS short alias");
    let nested = canonical.join("nested");
    std::fs::create_dir(&nested).expect("nested directory");
    let canonical_nested = nested.canonicalize().expect("canonical nested directory");
    assert!(canonical_path_starts_with(&canonical_nested, &short_path)
        .expect("compare canonical child with DOS short root"));
}

#[cfg(windows)]
#[test]
fn windows_rooted_rename_is_no_replace_and_preserves_the_open_source() {
    let root = tempdir().expect("tempdir");
    std::fs::write(root.path().join("source"), b"source").expect("source");
    let parent = cap_std::fs::Dir::open_ambient_dir(root.path(), cap_std::ambient_authority())
        .expect("parent capability");
    let source =
        open_capability_entry_no_follow(&parent, Path::new("source")).expect("open source");

    rename_open_entry_no_replace_windows(&parent, &source, Path::new("moved"))
        .expect("rooted rename");
    assert!(!root.path().join("source").exists());
    assert_eq!(
        std::fs::read(root.path().join("moved")).expect("moved source"),
        b"source"
    );

    std::fs::write(root.path().join("collision-source"), b"new").expect("collision source");
    std::fs::write(root.path().join("collision-target"), b"old").expect("collision target");
    let collision_source = open_capability_entry_no_follow(&parent, Path::new("collision-source"))
        .expect("open collision source");
    rename_open_entry_no_replace_windows(&parent, &collision_source, Path::new("collision-target"))
        .expect_err("existing destination must not be replaced");
    assert_eq!(
        std::fs::read(root.path().join("collision-source")).expect("preserved source"),
        b"new"
    );
    assert_eq!(
        std::fs::read(root.path().join("collision-target")).expect("preserved target"),
        b"old"
    );
}

#[cfg(windows)]
#[test]
fn windows_directory_junction_is_rejected_as_a_reparse_point() {
    let root = tempdir().expect("tempdir");
    let target = root.path().join("target");
    let junction = root.path().join("junction");
    std::fs::create_dir(&target).expect("junction target");
    let output = std::process::Command::new("cmd")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&target)
        .output()
        .expect("create junction");
    assert!(
        output.status.success(),
        "mklink failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(verify_directory_without_symlinks(&junction).is_err());
    assert!(ensure_directory_without_symlinks(&junction.join("child")).is_err());
    assert!(
        remove_directory_tree_capability_bound(root.path(), &junction, removal_deadline()).is_err()
    );
    assert!(!target.join("child").exists());
}

#[cfg(windows)]
#[test]
fn windows_nested_junction_is_rejected_before_recursive_removal_mutates() {
    let root = tempdir().expect("tempdir");
    let outside = tempdir().expect("outside");
    let child = root.path().join("child");
    let junction = child.join("junction");
    std::fs::create_dir(&child).expect("child directory");
    std::fs::write(child.join("owned"), b"owned").expect("owned file");
    std::fs::write(outside.path().join("outside"), b"outside").expect("outside file");
    let output = std::process::Command::new("cmd")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(&junction)
        .arg(outside.path())
        .output()
        .expect("create nested junction");
    assert!(
        output.status.success(),
        "mklink failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let error = remove_directory_tree_capability_bound(root.path(), &child, removal_deadline())
        .expect_err("nested junction must be rejected");

    assert!(
        error.to_string().contains("reparse point"),
        "unexpected error: {error}"
    );
    assert_eq!(
        std::fs::read(child.join("owned")).expect("preserved child file"),
        b"owned"
    );
    assert_eq!(
        std::fs::read(outside.path().join("outside")).expect("preserved outside file"),
        b"outside"
    );
    assert!(junction.exists());
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_ancestor_before_creating_child() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("tempdir");
    let outside = tempdir().expect("outside");
    symlink(outside.path(), root.path().join("linked")).expect("symlink");

    assert!(ensure_directory_without_symlinks(&root.path().join("linked/state")).is_err());
    assert!(remove_directory_tree_capability_bound(
        root.path(),
        &root.path().join("linked"),
        removal_deadline()
    )
    .is_err());
    assert!(!outside.path().join("state").exists());
}
