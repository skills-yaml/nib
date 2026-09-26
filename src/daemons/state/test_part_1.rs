use super::*;

#[test]
fn restart_recovery_preserves_ambiguous_prior_and_quarantine_artifacts() {
    let root = tempdir().expect("tempdir");
    let target = root.path().join("record.json");
    fs::write(&target, b"newer").expect("newer target");
    let directory = StableDirectory::open(root.path()).expect("stable directory");
    let previous = root.path().join(
        deterministic_previous_artifact_name(".restart-", target.file_name().unwrap())
            .expect("previous name"),
    );
    fs::write(&previous, b"substituted-prior").expect("prior artifact");

    let error = directory
        .recover_stale_temporary_files(".restart-", 16, 4096)
        .expect_err("ambiguous prior must fail closed");
    assert!(error.contains("both were preserved"), "{error}");
    assert_eq!(fs::read(&target).expect("target"), b"newer");
    assert_eq!(fs::read(&previous).expect("prior"), b"substituted-prior");

    fs::remove_file(&target).expect("remove target for missing-target recovery case");
    let error = directory
        .recover_stale_temporary_files(".restart-", 16, 4096)
        .expect_err("missing target with an unjournaled prior must fail closed");
    assert!(error.contains("target is missing"), "{error}");
    assert!(!target.exists());
    assert_eq!(
        fs::read(&previous).expect("preserved unproven prior"),
        b"substituted-prior"
    );
    fs::write(&target, b"newer").expect("restore target for quarantine case");

    let quarantine = directory
        .deterministic_artifact_path(&target, ".restart-delete-", ".quarantine")
        .expect("quarantine path");
    fs::write(&quarantine, b"substituted-quarantine").expect("quarantine artifact");
    let error = directory
        .recover_quarantined_file(&target, ".restart-delete-")
        .expect_err("ambiguous quarantine must fail closed");
    assert!(error.contains("both were preserved"), "{error}");
    assert!(target.exists());
    assert!(quarantine.exists());
}

#[test]
fn live_atomic_writer_cannot_hide_an_evacuated_target_from_recovery() {
    let root = tempdir().expect("tempdir");
    let target = root.path().join("session.json");
    fs::write(&target, b"authoritative session").expect("session target");
    let prefix = ".nib-session-";
    let temporary = root.path().join(deterministic_artifact_name(
        prefix,
        target.file_name().unwrap().as_encoded_bytes(),
        ".tmp",
    ));
    let previous = root.path().join(
        deterministic_previous_artifact_name(prefix, target.file_name().unwrap())
            .expect("previous artifact"),
    );
    let temporary_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&temporary)
        .expect("live temporary");
    temporary_file.lock().expect("own temporary transaction");
    fs::rename(&target, &previous).expect("evacuate target");
    let directory = StableDirectory::open(root.path()).expect("stable directory");

    let error = directory
        .recover_stale_temporary_files_strict(prefix, 16, 4096)
        .expect_err("live transaction must not be silently filtered");

    assert!(error.contains("live writer"), "{error}");
    assert!(error.contains("session.json"), "{error}");
    assert!(!target.exists());
    assert_eq!(
        fs::read(previous).expect("preserved prior session"),
        b"authoritative session"
    );
}

#[test]
fn strict_recovery_waits_for_a_transient_atomic_writer() {
    let root = tempdir().expect("tempdir");
    let target = root.path().join("session.json");
    fs::write(&target, b"authoritative session").expect("session target");
    let prefix = ".nib-session-";
    let temporary = root.path().join(deterministic_artifact_name(
        prefix,
        target.file_name().unwrap().as_encoded_bytes(),
        ".tmp",
    ));
    let previous = root.path().join(
        deterministic_previous_artifact_name(prefix, target.file_name().unwrap())
            .expect("previous artifact"),
    );
    let temporary_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&temporary)
        .expect("live temporary");
    temporary_file.lock().expect("own temporary transaction");
    fs::rename(&target, &previous).expect("evacuate target");

    let writer_target = target.clone();
    let writer_previous = previous.clone();
    let writer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        fs::rename(writer_previous, writer_target).expect("restore target");
        drop(temporary_file);
    });
    let directory = StableDirectory::open(root.path()).expect("stable directory");
    let started = Instant::now();

    directory
        .recover_stale_temporary_files_strict(prefix, 16, 4096)
        .expect("transient writer must finish before strict recovery returns");

    writer.join().expect("transient writer");
    assert!(started.elapsed() >= Duration::from_millis(25));
    assert_eq!(
        fs::read(&target).expect("restored session"),
        b"authoritative session"
    );
    assert!(!previous.exists());
    assert!(!temporary.exists());
}

#[test]
fn atomic_recovery_retries_when_prior_disappears_before_open() {
    let root = tempdir().expect("tempdir");
    let target = root.path().join("session.json");
    fs::write(&target, b"published session").expect("session target");
    let prefix = ".nib-session-";
    let temporary = root.path().join(deterministic_artifact_name(
        prefix,
        target.file_name().unwrap().as_encoded_bytes(),
        ".tmp",
    ));
    let previous = root.path().join(
        deterministic_previous_artifact_name(prefix, target.file_name().unwrap())
            .expect("previous artifact"),
    );
    fs::write(&previous, b"prior session").expect("prior artifact");
    let directory = StableDirectory::open(root.path()).expect("stable directory");
    let writer_directory = StableDirectory::open(root.path()).expect("writer directory");
    let previous_file = writer_directory
        .open_read(&previous)
        .expect("retained prior state");
    let previous_for_hook = previous.clone();
    let mut pending_cleanup = Some((writer_directory, previous_file));
    let recovered = {
        let mut remove_before_open = || {
            let (writer_directory, previous_file) =
                pending_cleanup.take().expect("prior-open hook runs once");
            writer_directory.remove_visible_file_if_matches(
                &previous_for_hook,
                &previous_file,
                || Ok(()),
            )
        };
        let mut live_target_hook = || {};
        directory
            .recover_atomic_transaction_with_hooks(
                &target,
                &temporary,
                &previous,
                true,
                false,
                AtomicRecoveryHooks {
                    previous_open: &mut remove_before_open,
                    live_target: &mut live_target_hook,
                },
            )
            .expect("changed namespace must be re-evaluated")
    };

    assert!(!recovered);
    assert!(pending_cleanup.is_none());
    assert_eq!(
        fs::read(&target).expect("published target"),
        b"published session"
    );
    assert!(!previous.exists());
}

#[test]
fn recovery_recognizes_a_live_writer_after_temporary_publication() {
    let root = tempdir().expect("tempdir");
    let target = root.path().join("session.json");
    fs::write(&target, b"prior session").expect("session target");
    let prefix = ".nib-session-";
    let temporary = root.path().join(deterministic_artifact_name(
        prefix,
        target.file_name().unwrap().as_encoded_bytes(),
        ".tmp",
    ));
    let previous = root.path().join(
        deterministic_previous_artifact_name(prefix, target.file_name().unwrap())
            .expect("previous artifact"),
    );
    let directory = StableDirectory::open(root.path()).expect("stable directory");
    let mut published = directory
        .open_read_write_create(&temporary)
        .expect("temporary publication");
    published.write_all(b"new session").expect("new state");
    published.sync_all().expect("sync new state");
    published.lock().expect("own atomic publication");
    fs::rename(&target, &previous).expect("evacuate prior state");
    fs::rename(&temporary, &target).expect("publish locked temporary");

    let recovered = directory
        .recover_stale_temporary_files(prefix, 16, 4096)
        .expect("live published writer must be skipped");
    assert_eq!(recovered, 0);
    assert!(target.exists());
    assert!(previous.exists());
    directory
        .verify_publication_bytes(&target, &published, b"new session")
        .expect("published target bytes through lock-owning handle");
    assert_eq!(
        fs::read(&previous).expect("preserved prior"),
        b"prior session"
    );

    let writer_directory = StableDirectory::open(root.path()).expect("writer directory");
    let writer_previous = previous.clone();
    let writer_previous_file = writer_directory
        .open_read(&previous)
        .expect("retained prior state");
    let (blocked_tx, blocked_rx) = std::sync::mpsc::sync_channel(0);
    let writer = thread::spawn(move || {
        blocked_rx.recv().expect("recovery reached target lock");
        writer_directory
            .remove_visible_file_if_matches(&writer_previous, &writer_previous_file, || Ok(()))
            .expect("finish prior cleanup");
        published.unlock().expect("release publication");
    });
    let mut report_live_target = || {
        blocked_tx
            .send(())
            .expect("report live published target to writer");
    };
    directory
        .recover_atomic_transaction_with_live_target_hook(
            &target,
            &temporary,
            &previous,
            true,
            true,
            &mut report_live_target,
        )
        .expect("strict recovery must observe completed publication");

    writer.join().expect("atomic writer");
    assert_eq!(fs::read(&target).expect("published target"), b"new session");
    assert!(!previous.exists());
    assert!(!temporary.exists());
}

#[test]
fn strict_recovery_times_out_then_preserves_unlocked_publication_ambiguity() {
    let root = tempdir().expect("tempdir");
    let target = root.path().join("session.json");
    let previous = root.path().join(
        deterministic_previous_artifact_name(".nib-session-", target.file_name().unwrap())
            .expect("previous artifact"),
    );
    fs::write(&target, b"published").expect("published target");
    fs::write(&previous, b"prior").expect("prior artifact");
    let directory = StableDirectory::open(root.path()).expect("stable directory");
    let target_file = directory.open_read_write(&target).expect("target handle");
    target_file.lock().expect("own published target");

    let started = Instant::now();
    let error = directory
        .recover_stale_temporary_files_strict(".nib-session-", 16, 4096)
        .expect_err("live published target must reach the strict deadline");
    assert!(
        started.elapsed() >= STRICT_RECOVERY_LIVE_WRITER_WAIT / 2,
        "strict recovery returned before its live-writer wait"
    );
    assert!(
        error.contains("target is still owned by a live writer"),
        "{error}"
    );
    assert!(target.exists());
    assert!(previous.exists());
    directory
        .verify_publication_bytes(&target, &target_file, b"published")
        .expect("preserved target bytes through lock-owning handle");
    assert_eq!(fs::read(&previous).expect("preserved prior"), b"prior");

    target_file.unlock().expect("release published target");
    let error = directory
        .recover_stale_temporary_files_strict(".nib-session-", 16, 4096)
        .expect_err("unlocked identity-distinct state must stay ambiguous");
    assert!(error.contains("both were preserved"), "{error}");
    assert_eq!(fs::read(&target).expect("preserved target"), b"published");
    assert_eq!(fs::read(&previous).expect("preserved prior"), b"prior");
}

#[test]
fn stale_temporary_recovery_ignores_near_match_names() {
    let root = tempdir().expect("tempdir");
    let near_matches = [
        ".bounded-abc.tmp",
        ".bounded-0000000000000000000000000000000g.tmp",
        ".bounded-00000000000000000000000000000000.tmp.extra",
    ];
    for name in near_matches {
        fs::write(root.path().join(name), b"preserve").expect("near-match artifact");
    }
    let directory = StableDirectory::open(root.path()).expect("stable directory");

    assert_eq!(
        directory
            .recover_stale_temporary_files(".bounded-", 16, 4096)
            .expect("bounded recovery"),
        0
    );
    for name in near_matches {
        assert!(root.path().join(name).exists(), "near match {name} removed");
    }
}

#[test]
fn pre_evacuation_stale_temporary_is_recoverable() {
    let root = tempdir().expect("tempdir");
    let target_name = std::ffi::OsStr::new("record.json");
    let temporary = root.path().join(deterministic_artifact_name(
        ".recoverable-",
        target_name.as_encoded_bytes(),
        ".tmp",
    ));
    fs::write(&temporary, b"unpublished temporary").expect("stale temporary");
    let directory = StableDirectory::open(root.path()).expect("stable directory");

    assert_eq!(
        directory
            .recover_stale_temporary_files(".recoverable-", 16, 4096)
            .expect("recover stale pre-evacuation temporary"),
        1
    );
    assert!(!temporary.exists());
    assert!(!root.path().join(target_name).exists());
}

#[cfg(unix)]
#[test]
fn real_child_atomic_fsync_crash_recovery_matrix() {
    if let Some(root) = std::env::var_os(ATOMIC_CRASH_CHILD_ROOT) {
        run_atomic_crash_child(Path::new(&root));
        return;
    }

    let root = tempdir().expect("tempdir");
    let pre_root = root.path().join("pre-evacuation");
    fs::create_dir(&pre_root).expect("pre-evacuation directory");
    let pre_target = pre_root.join("record.json");
    fs::write(&pre_target, b"old-pre").expect("pre-evacuation target");
    let pre_ready = root.path().join("pre.ready");
    let mut pre_child = spawn_atomic_crash_child(&pre_root, "before", &pre_ready);
    wait_for_atomic_child(&mut pre_child, &pre_ready);
    let pre_temporary = pre_root.join(deterministic_artifact_name(
        ".child-crash-",
        b"record.json",
        ".tmp",
    ));
    assert!(pre_temporary.exists(), "fsynced temporary was not visible");
    pre_child.kill().expect("kill pre-evacuation writer");
    pre_child.wait().expect("reap pre-evacuation writer");

    let pre_directory = StableDirectory::open(&pre_root).expect("pre recovery capability");
    assert_eq!(
        pre_directory
            .recover_stale_temporary_files(".child-crash-", 16, 4096)
            .expect("recover killed pre-evacuation writer"),
        1
    );
    assert_eq!(fs::read(&pre_target).expect("pre target"), b"old-pre");
    assert!(!pre_temporary.exists(), "recovery left the stale temporary");

    let post_root = root.path().join("post-evacuation");
    fs::create_dir(&post_root).expect("post-evacuation directory");
    let post_target = post_root.join("record.json");
    fs::write(&post_target, b"old-post").expect("post-evacuation target");
    let post_ready = root.path().join("post.ready");
    let mut post_child = spawn_atomic_crash_child(&post_root, "after", &post_ready);
    wait_for_atomic_child(&mut post_child, &post_ready);
    let post_temporary = post_root.join(deterministic_artifact_name(
        ".child-crash-",
        b"record.json",
        ".tmp",
    ));
    let post_previous = post_root.join(
        deterministic_previous_artifact_name(
            ".child-crash-",
            post_target.file_name().expect("post target name"),
        )
        .expect("post previous name"),
    );
    assert!(!post_target.exists(), "target was not evacuated");
    assert!(post_temporary.exists(), "post-evacuation temp missing");
    assert!(post_previous.exists(), "post-evacuation prior missing");
    post_child.kill().expect("kill post-evacuation writer");
    post_child.wait().expect("reap post-evacuation writer");

    let post_directory = StableDirectory::open(&post_root).expect("post recovery capability");
    let error = post_directory
        .recover_stale_temporary_files(".child-crash-", 16, 4096)
        .expect_err("post-evacuation crash must fail closed");
    assert!(error.contains("target is missing"), "{error}");
    assert!(!post_target.exists(), "recovery invented a target");
    assert_eq!(
        fs::read(&post_previous).expect("preserved prior"),
        b"old-post"
    );
    assert_eq!(
        fs::read(&post_temporary).expect("preserved temporary"),
        b"new-state"
    );
}

#[test]
fn stable_directory_scan_enforces_entry_and_filename_byte_budgets() {
    let root = tempdir().expect("tempdir");
    for name in ["one", "two", "three"] {
        fs::write(root.path().join(name), b"x").expect("scan fixture");
    }
    let directory = StableDirectory::open(root.path()).expect("stable directory");

    let entry_error = directory
        .for_each_entry_bounded(2, 1024, |_| Ok(()))
        .expect_err("entry cap must stop the streaming scan");
    assert!(entry_error.contains("bounded scan limit"), "{entry_error}");

    let name_error = directory
        .for_each_entry_bounded(3, 8, |_| Ok(()))
        .expect_err("filename byte cap must stop the streaming scan");
    assert!(name_error.contains("bounded scan limit"), "{name_error}");
}

#[cfg(windows)]
#[test]
fn delete_capable_directory_scan_uses_its_retained_handle() {
    let root = tempdir().expect("tempdir");
    let child = root.path().join("owned");
    fs::create_dir(&child).expect("owned child");
    fs::write(child.join("payload"), b"data").expect("owned child payload");
    let root_directory = StableDirectory::open(root.path()).expect("stable root");
    let owned = root_directory
        .open_owned_child(&child)
        .expect("delete-capable child");
    let mut names = Vec::new();

    owned
        .for_each_entry_bounded(4, 128, |name| {
            names.push(name);
            Ok(())
        })
        .expect("scan through retained delete-capable handle");

    assert_eq!(names, [OsString::from("payload")]);
}
