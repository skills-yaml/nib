use super::*;

#[test]
fn atomic_transaction_artifact_names_are_recognized_exactly() {
    let prefix = ".nib-session-";
    let target = std::ffi::OsStr::new("session-id.json");
    let previous = deterministic_previous_artifact_name(prefix, target)
        .expect("previous transaction artifact name");
    let temporary = deterministic_artifact_name(prefix, target.as_encoded_bytes(), ".tmp");

    assert!(StableDirectory::is_atomic_transaction_artifact_name(
        &previous, prefix
    ));
    assert!(StableDirectory::is_atomic_transaction_artifact_name(
        &temporary, prefix
    ));
    assert!(!StableDirectory::is_atomic_transaction_artifact_name(
        std::ffi::OsStr::new("ordinary-session.json"),
        prefix,
    ));
    assert!(!StableDirectory::is_atomic_transaction_artifact_name(
        std::ffi::OsStr::new(
            ".nib-session-00000000000000000000000000000000.previous-session-id.json"
        ),
        prefix,
    ));
}

#[cfg(unix)]
#[test]
fn optional_read_write_treats_exact_disappearance_during_identity_check_as_absent() {
    let root = tempdir().expect("tempdir");
    let target = root.path().join("cleanup-quarantine.json");
    fs::write(&target, b"owned cleanup state").expect("fixture state");
    let directory = StableDirectory::open(root.path()).expect("stable directory");

    let opened = directory
        .open_read_write_if_exists_with_hook(&target, || {
            fs::remove_file(&target).map_err(|error| format!("remove fixture after open: {error}"))
        })
        .expect("disappearance is an observed absence");

    assert!(opened.is_none());
    assert!(!target.exists());
}

#[cfg(unix)]
#[test]
fn optional_read_write_rejects_replacement_during_identity_check() {
    let root = tempdir().expect("tempdir");
    let target = root.path().join("cleanup-quarantine.json");
    let retired = root.path().join("retired-cleanup-quarantine.json");
    fs::write(&target, b"owned cleanup state").expect("fixture state");
    let directory = StableDirectory::open(root.path()).expect("stable directory");

    let error = directory
        .open_read_write_if_exists_with_hook(&target, || {
            fs::rename(&target, &retired)
                .map_err(|error| format!("retire fixture after open: {error}"))?;
            fs::write(&target, b"replacement cleanup state")
                .map_err(|error| format!("publish replacement fixture: {error}"))
        })
        .expect_err("replacement must fail the identity check");

    assert!(error.contains("state file identity changed while it was in use"));
    assert_eq!(
        fs::read(&target).expect("replacement remains visible"),
        b"replacement cleanup state"
    );
    assert_eq!(
        fs::read(&retired).expect("opened identity remains preserved"),
        b"owned cleanup state"
    );
}

#[test]
fn child_directory_creation_expiry_before_mutation_preserves_namespace() {
    let root = tempdir().expect("tempdir");
    let child = root.path().join("native.staging");
    let directory = StableDirectory::open(root.path()).expect("stable directory");
    let deadline = Instant::now() + Duration::from_millis(40);
    let mut paused = false;
    let error = directory
        .create_owned_child_directory_until_with_hook(&child, deadline, || {
            paused = true;
            while Instant::now() < deadline {
                thread::yield_now();
            }
            Ok(())
        })
        .expect_err("expiry at the final create boundary must win");
    assert!(paused, "test reached the final create boundary");
    assert!(error.contains("namespace deadline elapsed"), "{error}");
    assert!(
        !child.exists(),
        "expired creation must not mutate namespace"
    );

    let error = directory
        .create_owned_child_directory_until(&child, Instant::now())
        .expect_err("pre-expired free namespace must fail");
    assert!(error.contains("namespace deadline elapsed"), "{error}");
    assert!(!child.exists());
}

#[test]
fn existing_exact_child_is_resynced_after_expiry_before_parent_fsync() {
    for relative in [
        ".nib",
        ".nib/subagent-owners",
        ".git/nib/locks",
        ".nib/process-scopes",
    ] {
        let root = tempdir().expect("tempdir");
        let child = root.path().join(relative);
        fs::create_dir_all(child.parent().expect("child parent")).expect("fixture parent");
        let directory = StableDirectory::open(root.path()).expect("stable directory");
        // Leave enough setup time for loaded CI workers to reach the
        // post-create hook; the hook itself deterministically expires the
        // same deadline before the parent-sync guard is rechecked.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut paused_after_create = false;
        let error = directory
            .open_or_create_descendant_directory_with_guard_and_hooks(
                &child,
                || ensure_directory_namespace_deadline(deadline, &child),
                |_| Ok(()),
                |sync_child| {
                    if sync_child == child && !paused_after_create {
                        paused_after_create = true;
                        thread::sleep(deadline.saturating_duration_since(Instant::now()));
                    }
                    Ok(())
                },
            )
            .expect_err("expiry after create and before parent fsync must win");
        assert!(paused_after_create, "paused after creating {relative}");
        assert!(error.contains("namespace deadline elapsed"), "{error}");
        assert!(child.is_dir(), "created child must remain recoverable");

        let retry_deadline = Instant::now() + Duration::from_secs(2);
        let mut observed_retry_parent_sync = false;
        let reopened = directory
            .open_or_create_descendant_directory_with_guard_and_hooks(
                &child,
                || ensure_directory_namespace_deadline(retry_deadline, &child),
                |_| Ok(()),
                |sync_child| {
                    if sync_child == child {
                        observed_retry_parent_sync = true;
                    }
                    Ok(())
                },
            )
            .expect("fresh deadline finalizes the exact existing child");
        assert!(
            observed_retry_parent_sync,
            "retry did not fsync the parent of {relative}"
        );
        reopened.verify_visible().expect("reopened exact child");
    }
}

#[test]
fn child_directory_publication_expiry_preserves_exact_staging() {
    let root = tempdir().expect("tempdir");
    let staging = root.path().join("native.staging");
    let canonical = root.path().join("subagents");
    fs::create_dir(&staging).expect("staging directory");
    fs::write(staging.join("receipt.json"), b"exact receipt").expect("staging receipt");
    let directory = StableDirectory::open(root.path()).expect("stable directory");
    let staged = directory
        .open_owned_child(&staging)
        .expect("owned staging capability");
    let deadline = Instant::now() + Duration::from_millis(40);
    let mut paused = false;
    let error = directory
        .rename_child_directory_until_with_hook(&staging, &staged, &canonical, deadline, || {
            paused = true;
            while Instant::now() < deadline {
                thread::yield_now();
            }
            Ok(())
        })
        .expect_err("expiry at the publication/sync boundary must win");
    assert!(paused, "test reached the publication/sync boundary");
    assert!(error.contains("namespace deadline elapsed"), "{error}");
    assert!(!canonical.exists(), "expired publication must not redirect");
    assert_eq!(
        fs::read(staging.join("receipt.json")).expect("recoverable staging receipt"),
        b"exact receipt"
    );

    let error = directory
        .rename_child_directory_until(&staging, &staged, &canonical, Instant::now())
        .expect_err("pre-expired publication must fail");
    assert!(error.contains("namespace deadline elapsed"), "{error}");
    assert!(staging.exists());
    assert!(!canonical.exists());
}

#[cfg(windows)]
#[test]
fn windows_child_directory_publication_rechecks_expiry_after_mutation_capability() {
    let root = tempdir().expect("tempdir");
    let staging = root.path().join("native.staging");
    let canonical = root.path().join("subagents");
    fs::create_dir(&staging).expect("staging directory");
    fs::write(staging.join("receipt.json"), b"exact receipt").expect("staging receipt");
    let directory = StableDirectory::open(root.path()).expect("stable directory");
    let staged = directory
        .open_owned_child(&staging)
        .expect("owned staging capability");
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut acquired = false;
    let error = directory
        .rename_child_directory_until_with_hooks(
            &staging,
            &staged,
            &canonical,
            deadline,
            || Ok(()),
            || {
                acquired = true;
                while Instant::now() < deadline {
                    thread::yield_now();
                }
                Ok(())
            },
        )
        .expect_err("expiry after mutation capability acquisition must prevent publication");
    assert!(acquired, "test acquired the exact mutation capability");
    assert!(error.contains("namespace deadline elapsed"), "{error}");
    assert!(!canonical.exists(), "expired publication must not redirect");
    assert_eq!(
        fs::read(staging.join("receipt.json")).expect("recoverable staging receipt"),
        b"exact receipt"
    );
}

#[cfg(windows)]
#[test]
fn windows_directory_removal_rechecks_authority_after_mutation_capability() {
    let root = tempdir().expect("tempdir");
    let child_path = root.path().join("child");
    fs::create_dir(&child_path).expect("child directory");
    let parent = StableDirectory::open(root.path()).expect("stable parent");
    let child = parent
        .open_owned_child(&child_path)
        .expect("owned child capability");
    let acquired = std::cell::Cell::new(false);
    let error = parent
        .remove_empty_child_directory_if_matches_with_guard_and_hook(
            &child_path,
            child,
            || {
                if acquired.get() {
                    Err("directory mutation authority expired".to_string())
                } else {
                    Ok(())
                }
            },
            || {
                acquired.set(true);
                Ok(())
            },
        )
        .expect_err("expired post-acquisition authority must prevent removal");
    assert!(
        acquired.get(),
        "test acquired the exact mutation capability"
    );
    assert!(error.contains("authority expired"), "{error}");
    assert!(
        child_path.is_dir(),
        "expired removal must preserve the child"
    );
}

#[cfg(any(unix, windows))]
#[test]
fn daemon_anchor_publication_expiry_preserves_exact_pair_for_fresh_retry() {
    let root = tempdir().expect("tempdir");
    let visible_root = root.path().join("visible");
    let anchor_root = root.path().join("anchors");
    fs::create_dir(&visible_root).expect("visible root");
    fs::create_dir(&anchor_root).expect("anchor root");
    let visible_path = visible_root.join("setup.lock");
    let anchor_path = anchor_root.join("setup.lock.anchor");
    fs::write(&visible_path, b"owned setup lock").expect("visible lock");
    let visible_directory = StableDirectory::open(&visible_root).expect("visible capability");
    let anchor_directory = StableDirectory::open(&anchor_root).expect("anchor capability");
    let visible = visible_directory
        .open_read_write(&visible_path)
        .expect("visible lock handle");
    let identity = daemon_lock_identity(&visible, &visible_path).expect("lock identity");
    // The hard-link publication is the phase under test. Give loaded CI
    // workers enough time to reach it, then expire the same deadline in
    // the guard before final synchronization.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut paused = false;
    let error = repair_daemon_lock_anchor_with_guard(
        &visible_directory,
        &visible_path,
        &anchor_directory,
        &anchor_path,
        &identity,
        || {
            if !paused && anchor_path.exists() {
                paused = true;
                thread::sleep(deadline.saturating_duration_since(Instant::now()));
            }
            ensure_directory_namespace_deadline(deadline, &anchor_path)
        },
    )
    .expect_err("expiry after hard-link publication must win before final sync");
    assert!(paused, "test crossed the hard-link publication boundary");
    assert!(error.contains("namespace deadline elapsed"), "{error}");
    let anchor = anchor_directory
        .open_read_write(&anchor_path)
        .expect("recoverable exact anchor");
    assert!(same_open_file_identity(&visible, &anchor).expect("exact linked identity"));
    assert_eq!(
        fs::read(&visible_path).expect("visible bytes"),
        b"owned setup lock"
    );
    assert_eq!(
        fs::read(&anchor_path).expect("anchor bytes"),
        b"owned setup lock"
    );

    let retry_deadline = Instant::now() + Duration::from_secs(2);
    repair_daemon_lock_anchor_with_guard(
        &visible_directory,
        &visible_path,
        &anchor_directory,
        &anchor_path,
        &identity,
        || ensure_directory_namespace_deadline(retry_deadline, &anchor_path),
    )
    .expect("fresh deadline syncs and verifies retained pair");
    verify_daemon_lock_paths_bound(
        &visible_directory,
        &visible_path,
        &anchor_directory,
        &anchor_path,
        &identity,
    )
    .expect("retried pair remains exact");
}

#[test]
fn guarded_atomic_recovery_preserves_the_complete_namespace_after_expiry() {
    let root = tempdir().expect("tempdir");
    let target = root.path().join("record.json");
    fs::write(&target, b"authoritative").expect("target fixture");
    let directory = StableDirectory::open(root.path()).expect("stable directory");
    let temporary = directory
        .deterministic_artifact_path(&target, ".guarded-save-", ".tmp")
        .expect("temporary path");
    fs::write(&temporary, b"stale transaction fixture").expect("temporary fixture");
    let deadline = Instant::now() + Duration::from_millis(150);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let worker_root = root.path().to_path_buf();
    let worker_target = target.clone();
    let worker = thread::spawn(move || {
        let directory = StableDirectory::open(&worker_root).expect("worker directory");
        let expected = directory
            .open_read(&worker_target)
            .expect("expected target");
        let mut first_guard = true;
        let error = directory
            .save_bytes_atomically_expected_with_receipt_and_guard(
                &worker_target,
                b"must not publish",
                ".guarded-save-",
                FileExpectation::Present(&expected),
                || {
                    if first_guard {
                        first_guard = false;
                        ready_tx.send(()).expect("signal recovery barrier");
                        resume_rx
                            .recv_timeout(Duration::from_secs(5))
                            .expect("release recovery barrier");
                    }
                    if Instant::now() >= deadline {
                        Err("guarded atomic deadline elapsed".to_string())
                    } else {
                        Ok(())
                    }
                },
            )
            .expect_err("expired recovery guard must fail");
        assert!(error.message.contains("guarded atomic deadline elapsed"));
    });
    ready_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("atomic recovery reached its guard");
    let paused = namespace_snapshot(root.path());
    while Instant::now() < deadline {
        thread::yield_now();
    }
    resume_tx.send(()).expect("resume atomic recovery");
    worker.join().expect("guarded atomic worker");

    assert_eq!(namespace_snapshot(root.path()), paused);
    thread::sleep(Duration::from_millis(50));
    assert_eq!(
        namespace_snapshot(root.path()),
        paused,
        "expired atomic recovery mutated its transaction namespace later"
    );
}

#[cfg(windows)]
#[test]
fn windows_namespace_children_reopen_before_exclusive_deletion() {
    let root = tempdir().expect("tempdir");
    let child_path = root.path().join("child");
    fs::create_dir(&child_path).expect("child directory");
    let parent = StableDirectory::open(root.path()).expect("stable parent");
    let child = parent
        .open_child(&child_path)
        .expect("ordinary namespace child");
    let reopened = parent
        .open_child(&child_path)
        .expect("second ordinary namespace child");
    assert!(child.same_identity(&reopened));

    child.verify_visible().expect("first visible identity");
    child
        .verify_visible_at(&child_path)
        .expect("second visible identity");
    let receipt = child
        .directory_removal_receipt()
        .expect("share-compatible ownership receipt");

    drop(reopened);
    drop(child);
    let owned = parent
        .open_owned_child(&child_path)
        .expect("delete-capable child beside observation receipt");
    parent
        .remove_empty_child_directory_if_matches(&child_path, owned)
        .expect("handle-bound empty-directory deletion");
    assert!(!child_path.exists());
    let _retained_identity = receipt.identity();
}

#[cfg(windows)]
#[test]
fn windows_observation_receipt_supports_recursive_handle_bound_cleanup() {
    let root = tempdir().expect("tempdir");
    let tree_path = root.path().join("tree");
    let parent = StableDirectory::open(root.path()).expect("stable parent");
    let tree = parent
        .create_owned_child_directory(&tree_path)
        .expect("owned tree");
    fs::write(tree_path.join("payload"), b"owned").expect("tree payload");
    let receipt = tree
        .directory_removal_receipt()
        .expect("observation receipt");
    let retained_receipt = receipt.clone();
    drop(tree);

    crate::fs_security::remove_directory_tree_capability_bound_if_matches(
        root.path(),
        &tree_path,
        receipt,
        Instant::now() + Duration::from_secs(5),
    )
    .expect("receipt-bound recursive cleanup");

    assert!(!tree_path.exists());
    let _retained_identity = retained_receipt.identity();
}

#[test]
fn daemon_file_lock_replacement_child_process() {
    let Some(lock_path) = std::env::var_os(LOCK_CHILD_PATH) else {
        return;
    };
    let ready_path = PathBuf::from(
        std::env::var_os(LOCK_CHILD_READY).expect("child ready path must be configured"),
    );
    let entered_path = PathBuf::from(
        std::env::var_os(LOCK_CHILD_ENTERED).expect("child entered path must be configured"),
    );
    let expectation =
        std::env::var(LOCK_CHILD_EXPECTATION).expect("child expectation must be configured");
    fs::write(&ready_path, b"ready").expect("publish child readiness");

    let result = with_file_lock(Path::new(&lock_path), |_| {
        fs::write(&entered_path, b"entered").map_err(|error| error.to_string())
    });
    match expectation.as_str() {
        "blocked" => panic!("parent must terminate a blocked lock child, got {result:?}"),
        "identity" => {
            let error = result.expect_err("replacement lock identity must fail closed");
            assert!(
                error.contains("persistent anchor have different identities"),
                "{error}"
            );
        }
        "entered" => {
            result.expect("waiter must enter after the prior owner releases the lock");
            assert!(
                entered_path.exists(),
                "successful waiter did not enter operation"
            );
        }
        value => panic!("unsupported child expectation: {value}"),
    }
    if expectation != "entered" {
        assert!(
            !entered_path.exists(),
            "failed lock child entered operation"
        );
    }
}

#[test]
fn expired_deadline_rejects_a_free_daemon_lock_before_operation() {
    let root = tempdir().expect("tempdir");
    let state_dir = root.path().join("state");
    fs::create_dir(&state_dir).expect("state directory");
    let lock_path = state_dir.join("expired-free.lock");
    let mut operation_ran = false;

    let error = with_file_lock_in_until(
        &lock_path,
        &state_dir,
        Instant::now() - Duration::from_millis(1),
        |_| {
            operation_ran = true;
            Ok(())
        },
    )
    .expect_err("an expired deadline must reject an uncontended lock");

    assert!(error.contains("timed out acquiring daemon state lock"));
    assert!(!operation_ran, "expired lock entered its operation");
    assert!(
        !lock_path.exists(),
        "expired lock setup mutated the namespace"
    );
}

#[cfg(any(unix, windows))]
#[test]
fn bounded_session_lock_setup_retries_parent_and_anchor_finalization() {
    let root = tempdir().expect("tempdir");
    let sessions = root.path().join(".nib/sessions");
    fs::create_dir_all(&sessions).expect("sessions directory");

    let nested_lock = sessions.join(".locks/session.lock");
    let nested_parent = nested_lock.parent().expect("nested lock parent");
    // Leave enough setup headroom for a loaded hosted filesystem; the hook
    // deterministically expires the same absolute deadline once this phase exists.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut paused_after_parent_create = false;
    let mut operation_ran = false;
    let error = with_file_lock_in_with_deadline_and_setup_hook(
        &nested_lock,
        &sessions,
        Some(deadline),
        |_| {
            operation_ran = true;
            Ok(())
        },
        || {
            if nested_parent.is_dir() && !nested_lock.exists() && !paused_after_parent_create {
                paused_after_parent_create = true;
                thread::sleep(deadline.saturating_duration_since(Instant::now()));
            }
            Ok(())
        },
    )
    .expect_err("expiry after lock-parent create must win");
    assert!(paused_after_parent_create);
    assert!(error.contains("timed out acquiring daemon state lock"));
    assert!(!operation_ran);
    assert!(nested_parent.is_dir(), "exact parent remains recoverable");

    with_file_lock_in_until(
        &nested_lock,
        &sessions,
        Instant::now() + Duration::from_secs(2),
        |_| Ok(()),
    )
    .expect("fresh deadline finalizes the retained parent");

    let lock_path = sessions.join("session.lock");
    let anchor_path = daemon_lock_anchor_path(&lock_path).expect("session anchor");
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut paused_after_anchor_link = false;
    let error = with_file_lock_in_with_deadline_and_setup_hook(
        &lock_path,
        &sessions,
        Some(deadline),
        |_| Err::<(), _>("operation must not run after setup expiry".to_string()),
        || {
            if lock_path.exists() && anchor_path.exists() && !paused_after_anchor_link {
                paused_after_anchor_link = true;
                thread::sleep(deadline.saturating_duration_since(Instant::now()));
            }
            Ok(())
        },
    )
    .expect_err("expiry after anchor publication must win");
    assert!(paused_after_anchor_link);
    assert!(error.contains("timed out acquiring daemon state lock"));
    let visible = File::open(&lock_path).expect("recoverable visible lock");
    let anchor = File::open(&anchor_path).expect("recoverable exact anchor");
    assert!(same_open_file_identity(&visible, &anchor).expect("same lock inode"));

    let mut retry_operation_ran = false;
    with_file_lock_in_until(
        &lock_path,
        &sessions,
        Instant::now() + Duration::from_secs(2),
        |_| {
            retry_operation_ran = true;
            Ok(())
        },
    )
    .expect("fresh deadline repairs, syncs, and enters the operation");
    assert!(retry_operation_ran);
    assert!(!anchor_path.exists(), "successful retry cleans its anchor");
}

#[cfg(unix)]
#[test]
fn persistent_anchor_prevents_replaced_daemon_lock_domains() {
    let root = tempdir().expect("tempdir");
    let state_dir = root.path().join("state");
    fs::create_dir(&state_dir).expect("state directory");
    let lock_path = state_dir.join("shared.json.lock");
    let ready_path = root.path().join("child.ready");
    let entered_path = root.path().join("child.entered");

    with_file_lock(&lock_path, |_| {
        let displaced_lock = state_dir.join("shared.json.lock.displaced");
        fs::rename(&lock_path, &displaced_lock)
            .map_err(|error| format!("failed to displace visible lock: {error}"))?;
        fs::write(&lock_path, b"replacement")
            .map_err(|error| format!("failed to replace visible lock: {error}"))?;
        run_identity_failure_child(&lock_path, &ready_path, &entered_path);

        fs::remove_file(&lock_path)
            .map_err(|error| format!("failed to remove replacement lock: {error}"))?;
        fs::rename(&displaced_lock, &lock_path)
            .map_err(|error| format!("failed to restore visible lock: {error}"))?;

        let displaced_state = root.path().join("state.displaced");
        fs::rename(&state_dir, &displaced_state)
            .map_err(|error| format!("failed to displace lock directory: {error}"))?;
        fs::create_dir(&state_dir)
            .map_err(|error| format!("failed to replace lock directory: {error}"))?;
        let mut child = spawn_lock_child(&lock_path, &ready_path, &entered_path, "blocked");
        assert_child_remains_blocked(&mut child, &ready_path, &entered_path);
        fs::remove_dir_all(&state_dir)
            .map_err(|error| format!("failed to remove replacement directory: {error}"))?;
        fs::rename(&displaced_state, &state_dir)
            .map_err(|error| format!("failed to restore lock directory: {error}"))?;
        Ok(())
    })
    .expect("held persistent daemon lock");

    with_file_lock(&lock_path, |_| {
        fs::write(&entered_path, b"restored").map_err(|error| error.to_string())
    })
    .expect("restored daemon lock remains usable");
    assert_eq!(
        fs::read(&entered_path).expect("restored operation marker"),
        b"restored"
    );
    let anchor_path = daemon_lock_anchor_path(&lock_path).expect("anchor path");
    assert_eq!(anchor_path.parent(), state_dir.parent());
    assert!(!anchor_path.starts_with(&state_dir));
}

#[cfg(any(unix, windows))]
#[test]
fn git_lock_anchor_is_removed_and_repository_status_stays_clean() {
    let root = tempdir().expect("tempdir");
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root.path())
        .status()
        .expect("initialize Git repository");
    assert!(status.success());
    fs::write(root.path().join(".git/info/exclude"), ".nib/\n").expect("ignore local state");
    let state_dir = root.path().join(".nib");
    fs::create_dir(&state_dir).expect("state directory");
    let lock_path = state_dir.join("shared.json.lock");
    let anchor_path = daemon_lock_anchor_path(&lock_path).expect("anchor path");
    assert!(anchor_path.starts_with(root.path().join(".git/nib/locks")));

    with_file_lock(&lock_path, |_| Ok(())).expect("lock operation");

    assert!(
        !anchor_path.exists(),
        "successful lock left its anchor file"
    );
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root.path())
        .output()
        .expect("inspect Git status");
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "lock lifecycle dirtied Git status: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[cfg(any(unix, windows))]
#[test]
fn nested_state_lock_does_not_use_a_sibling_git_directory() {
    let root = tempdir().expect("tempdir");
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root.path())
        .status()
        .expect("initialize Git repository");
    assert!(status.success());
    let state_dir = root.path().join("state");
    fs::create_dir(&state_dir).expect("state directory");
    let lock_path = state_dir.join("shared.json.lock");

    let anchor_path = daemon_lock_anchor_path(&lock_path).expect("anchor path");

    assert_eq!(anchor_path.parent(), Some(root.path()));
    assert!(!anchor_path.starts_with(root.path().join(".git")));
}

#[cfg(any(unix, windows))]
#[test]
fn gitdir_file_cannot_redirect_lock_anchor_outside_project() {
    let root = tempdir().expect("tempdir");
    let outside = tempdir().expect("outside");
    fs::write(
        root.path().join(".git"),
        format!("gitdir: {}\n", outside.path().display()),
    )
    .expect("malicious gitdir file");
    let state_dir = root.path().join("state");
    fs::create_dir(&state_dir).expect("state directory");
    let lock_path = state_dir.join("shared.json.lock");
    let anchor_path = daemon_lock_anchor_path(&lock_path).expect("anchor path");
    assert_eq!(anchor_path.parent(), Some(root.path()));

    with_file_lock(&lock_path, |_| Ok(())).expect("fallback lock operation");

    assert!(!anchor_path.exists(), "fallback anchor was not cleaned");
    assert!(
        fs::read_dir(outside.path())
            .expect("outside directory")
            .next()
            .is_none(),
        "gitdir contents redirected anchor creation outside the project"
    );
}

#[cfg(any(unix, windows))]
#[test]
fn blocked_waiter_repairs_then_cleans_removed_anchor() {
    let root = tempdir().expect("tempdir");
    let state_dir = root.path().join("state");
    fs::create_dir(&state_dir).expect("state directory");
    let lock_path = state_dir.join("shared.json.lock");
    let ready_path = root.path().join("waiter.ready");
    let entered_path = root.path().join("waiter.entered");
    let anchor_path = daemon_lock_anchor_path(&lock_path).expect("anchor path");
    let mut waiter = None;

    with_file_lock(&lock_path, |_| {
        let child = spawn_lock_child(&lock_path, &ready_path, &entered_path, "entered");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready_path.exists() {
            assert!(
                Instant::now() < deadline,
                "waiter did not reach lock acquisition"
            );
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(150));
        assert!(!entered_path.exists(), "waiter entered while lock was held");
        waiter = Some(child);
        Ok(())
    })
    .expect("first lock owner");

    let status = waiter
        .expect("spawned waiter")
        .wait()
        .expect("wait for repaired-anchor waiter");
    assert!(status.success(), "waiter failed: {status}");
    assert!(entered_path.exists(), "waiter did not enter after release");
    assert!(!anchor_path.exists(), "waiter left repaired anchor behind");
}

#[cfg(any(unix, windows))]
#[test]
fn anchor_removed_before_open_is_repaired_after_visible_inode_lock() {
    let root = tempdir().expect("tempdir");
    let state_dir = root.path().join("state");
    fs::create_dir(&state_dir).expect("state directory");
    let lock_path = state_dir.join("shared.json.lock");
    let anchor_path = daemon_lock_anchor_path(&lock_path).expect("anchor path");
    let lock_directory = StableDirectory::open(&state_dir).expect("stable state directory");
    let anchor_directory = StableDirectory::open(root.path()).expect("stable anchor directory");
    drop(
        lock_directory
            .open_read_write_create(&lock_path)
            .expect("visible lock"),
    );
    lock_directory
        .hard_link_to(&lock_path, &anchor_directory, &anchor_path)
        .expect("initial anchor");
    let mut removed = false;

    let lock_file = open_daemon_lock_anchor_bound_with_hook(
        &lock_directory,
        &lock_path,
        &anchor_directory,
        &anchor_path,
        || {
            if removed {
                return Ok(());
            }
            let anchor = anchor_directory.open_read_write(&anchor_path)?;
            anchor_directory.remove_file_if_matches(
                &anchor_path,
                &anchor,
                ".nib-test-anchor-delete-",
            )?;
            removed = true;
            Ok(())
        },
    )
    .expect("retain visible inode after disappearing anchor");

    assert!(removed, "test did not remove the first anchor");
    let identity = daemon_lock_identity(&lock_file, &lock_path).expect("lock identity");
    lock_file.lock().expect("lock retained visible inode");
    repair_daemon_lock_anchor(
        &lock_directory,
        &lock_path,
        &anchor_directory,
        &anchor_path,
        &identity,
    )
    .expect("repair disappearing anchor after locking");
    verify_daemon_lock_paths_bound(
        &lock_directory,
        &lock_path,
        &anchor_directory,
        &anchor_path,
        &identity,
    )
    .expect("re-established lock pair");
}

#[cfg(any(unix, windows))]
#[test]
fn lock_pair_removed_before_open_retries_first_use_acquisition() {
    let root = tempdir().expect("tempdir");
    let state_dir = root.path().join("state");
    fs::create_dir(&state_dir).expect("state directory");
    let lock_path = state_dir.join("shared.json.lock");
    let anchor_path = daemon_lock_anchor_path(&lock_path).expect("anchor path");
    let lock_directory = StableDirectory::open(&state_dir).expect("stable state directory");
    let anchor_directory = StableDirectory::open(root.path()).expect("stable anchor directory");
    drop(
        lock_directory
            .open_read_write_create(&lock_path)
            .expect("visible lock"),
    );
    lock_directory
        .hard_link_to(&lock_path, &anchor_directory, &anchor_path)
        .expect("initial anchor");
    let mut removed = false;

    let lock_file = open_daemon_lock_anchor_bound_with_hook(
        &lock_directory,
        &lock_path,
        &anchor_directory,
        &anchor_path,
        || {
            if removed {
                return Ok(());
            }
            let anchor = anchor_directory.open_read_write(&anchor_path)?;
            anchor_directory.remove_file_if_matches(
                &anchor_path,
                &anchor,
                ".nib-test-anchor-delete-",
            )?;
            let visible = lock_directory.open_read_write(&lock_path)?;
            lock_directory.remove_file_if_matches(
                &lock_path,
                &visible,
                ".nib-test-lock-delete-",
            )?;
            removed = true;
            Ok(())
        },
    )
    .expect("retry after disappearing lock pair");

    assert!(removed, "test did not remove the first lock pair");
    let identity = daemon_lock_identity(&lock_file, &lock_path).expect("lock identity");
    lock_file.lock().expect("lock recreated visible inode");
    repair_daemon_lock_anchor(
        &lock_directory,
        &lock_path,
        &anchor_directory,
        &anchor_path,
        &identity,
    )
    .expect("repair recreated lock pair");
    verify_daemon_lock_paths_bound(
        &lock_directory,
        &lock_path,
        &anchor_directory,
        &anchor_path,
        &identity,
    )
    .expect("verified recreated lock pair");
}

#[cfg(any(unix, windows))]
#[test]
fn legacy_lock_cleanup_preserves_a_replacement_at_the_delete_boundary() {
    let root = tempdir().expect("tempdir");
    let visible_root = root.path().join("visible");
    let anchor_root = root.path().join("anchor");
    fs::create_dir(&visible_root).expect("visible directory");
    fs::create_dir(&anchor_root).expect("anchor directory");
    let visible_path = visible_root.join("legacy.lock");
    let anchor_path = anchor_root.join("legacy.anchor");
    let displaced = visible_root.join("legacy.displaced");
    fs::write(&visible_path, b"legacy").expect("legacy lock");
    fs::hard_link(&visible_path, &anchor_path).expect("legacy hard-link pair");
    let visible = StableDirectory::open(&visible_root).expect("visible capability");
    let anchor = StableDirectory::open(&anchor_root).expect("anchor capability");

    let error =
        cleanup_legacy_lock_pair_with_hook(&visible, &visible_path, &anchor, &anchor_path, || {
            fs::rename(&visible_path, &displaced).map_err(|error| error.to_string())?;
            fs::write(&visible_path, b"replacement").map_err(|error| error.to_string())
        })
        .expect_err("replacement must stop legacy deletion");

    assert!(error.contains("identity changed"), "{error}");
    assert_eq!(
        fs::read(&visible_path).expect("replacement lock"),
        b"replacement"
    );
    assert_eq!(
        fs::read(&displaced).expect("displaced legacy lock"),
        b"legacy"
    );
    assert_eq!(fs::read(&anchor_path).expect("legacy anchor"), b"legacy");
}

#[cfg(any(unix, windows))]
#[test]
fn absent_optional_legacy_lock_pair_still_executes_final_namespace_guard() {
    let root = tempdir().expect("tempdir");
    let anchor_root = root.path().join("anchor");
    fs::create_dir(&anchor_root).expect("anchor directory");
    let anchor = StableDirectory::open(&anchor_root).expect("anchor capability");
    let visible_path = root.path().join("missing").join("legacy.lock");
    let anchor_path = anchor_root.join("missing.anchor");
    let mut guard_calls = 0;

    let error = cleanup_legacy_lock_pair_optional_with_guard(
        None,
        &visible_path,
        &anchor,
        &anchor_path,
        || {
            guard_calls += 1;
            if guard_calls == 2 {
                Err("final namespace guard observed".to_string())
            } else {
                Ok(())
            }
        },
    )
    .expect_err("an absent pair must not bypass its final namespace guard");

    assert_eq!(error, "final namespace guard observed");
    assert_eq!(guard_calls, 2);
    assert!(!anchor_path.exists());
}

#[cfg(windows)]
#[test]
fn open_directory_capability_blocks_daemon_lock_parent_replacement() {
    let root = tempdir().expect("tempdir");
    let state_dir = root.path().join("state");
    fs::create_dir(&state_dir).expect("state directory");
    let lock_path = state_dir.join("shared.json.lock");
    let displaced_state = root.path().join("state.displaced");

    let error = with_file_lock(&lock_path, |_| {
        fs::rename(&state_dir, &displaced_state).map_err(|error| error.to_string())
    })
    .expect_err("a live Windows lock domain must pin its parent namespace");

    assert!(!error.is_empty());
    assert!(state_dir.is_dir(), "original lock domain remains visible");
    assert!(!displaced_state.exists(), "lock domain was not displaced");
    assert!(
        lock_path.is_file(),
        "original lock remains in the pinned domain"
    );
}

#[cfg(windows)]
#[test]
fn lock_common_root_replacement_is_rejected_before_namespace_mutation() {
    let root = tempdir().expect("tempdir");
    let project = root.path().join("project");
    let state_dir = project.join("state");
    let displaced_project = root.path().join("project.displaced");
    let lock_path = state_dir.join("shared.json.lock");
    let anchor_path = daemon_lock_anchor_path(&lock_path).expect("lock anchor");
    let displaced_anchor = displaced_project.join(
        anchor_path
            .strip_prefix(&project)
            .expect("anchor below common root"),
    );
    fs::create_dir_all(&state_dir).expect("state directory");
    fs::write(state_dir.join("original"), b"original").expect("original sentinel");
    let mut replaced = false;
    let mut operation_ran = false;

    let error = with_file_lock_in_until_with_setup_hook(
        &lock_path,
        &state_dir,
        Instant::now() + Duration::from_secs(2),
        |_| {
            operation_ran = true;
            Ok(())
        },
        || {
            if replaced {
                return Ok(());
            }
            fs::rename(&project, &displaced_project).map_err(|error| error.to_string())?;
            fs::create_dir_all(&state_dir).map_err(|error| error.to_string())?;
            fs::write(state_dir.join("replacement"), b"replacement")
                .map_err(|error| error.to_string())?;
            replaced = true;
            Ok(())
        },
    )
    .expect_err("a replaced common lock root must fail closed");

    assert!(
        replaced,
        "test did not replace the common lock root: {error}"
    );
    assert!(
        !operation_ran,
        "replacement entered the protected operation"
    );
    assert!(error.contains("identity changed"), "{error}");
    assert_eq!(
        fs::read(displaced_project.join("state/original")).expect("original sentinel"),
        b"original"
    );
    assert_eq!(
        fs::read(state_dir.join("replacement")).expect("replacement sentinel"),
        b"replacement"
    );
    assert!(!displaced_project.join("state/shared.json.lock").exists());
    assert!(!displaced_anchor.exists());
    assert!(!lock_path.exists());
    assert!(!anchor_path.exists());
}

#[cfg(unix)]
#[test]
fn daemon_lock_open_rejects_a_symlink_inserted_after_inspection() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("tempdir");
    let lock_path = root.path().join("race.lock");
    let displaced_path = root.path().join("race.lock.displaced");
    let outside_path = root.path().join("outside");
    fs::write(&lock_path, b"original").expect("original lock");
    fs::write(&outside_path, b"sentinel").expect("outside target");

    let error = open_daemon_lock_file_with_hook(&lock_path, || {
        fs::rename(&lock_path, &displaced_path)
            .map_err(|error| format!("failed to displace inspected lock: {error}"))?;
        symlink(&outside_path, &lock_path)
            .map_err(|error| format!("failed to insert lock symlink: {error}"))
    })
    .expect_err("no-follow open must reject the inserted symlink");

    assert!(error.contains("failed to open daemon lock"), "{error}");
    assert_eq!(
        fs::read(&outside_path).expect("outside target remains readable"),
        b"sentinel"
    );
}

#[cfg(unix)]
#[test]
fn pure_atomic_save_aborts_when_directory_is_detached_before_commit() {
    let root = tempdir().expect("tempdir");
    let state_dir = root.path().join("state");
    let displaced = root.path().join("state.displaced");
    let target = state_dir.join("record.json");
    fs::create_dir(&state_dir).expect("state directory");
    fs::write(&target, b"original").expect("original state");
    let directory = StableDirectory::open(&state_dir).expect("stable directory");

    fs::rename(&state_dir, &displaced).expect("detach state directory");
    fs::create_dir(&state_dir).expect("replacement state directory");
    fs::write(&target, b"replacement").expect("replacement sentinel");

    let error = directory
        .save_bytes_atomically(&target, b"new", ".pure-")
        .expect_err("known detachment must abort a pure commit");
    assert!(error.contains("identity changed"), "{error}");
    assert_eq!(
        fs::read(displaced.join("record.json")).expect("original tree record"),
        b"original"
    );
    assert_eq!(
        fs::read(&target).expect("replacement tree record"),
        b"replacement"
    );
    assert!(
        fs::read_dir(&displaced)
            .expect("original tree")
            .all(|entry| !entry
                .expect("state entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".pure-")),
        "aborted temporary file must be removed from the original capability"
    );
}

#[cfg(unix)]
#[test]
fn atomic_save_race_never_redirects_commit_to_replacement_directory() {
    let root = tempdir().expect("tempdir");
    let state_dir = root.path().join("state");
    let displaced = root.path().join("state.displaced");
    let target = state_dir.join("record.json");
    fs::create_dir(&state_dir).expect("state directory");
    fs::write(&target, b"original").expect("original state");
    let directory = StableDirectory::open(&state_dir).expect("stable directory");

    let error = directory
        .save_bytes_atomically_with_hook(&target, b"committed", ".race-", true, || {
            fs::rename(&state_dir, &displaced)
                .map_err(|error| format!("failed to detach state directory: {error}"))?;
            fs::create_dir(&state_dir)
                .map_err(|error| format!("failed to create replacement state: {error}"))?;
            fs::write(&target, b"replacement")
                .map_err(|error| format!("failed to seed replacement state: {error}"))
        })
        .expect_err("post-check must report the attachment race");
    assert!(error.contains("identity changed"), "{error}");
    assert_eq!(
        fs::read(displaced.join("record.json")).expect("original capability commit"),
        b"committed"
    );
    assert_eq!(
        fs::read(&target).expect("replacement sentinel"),
        b"replacement"
    );
}

#[cfg(unix)]
#[test]
fn post_publication_failure_retains_the_exact_publication_receipt() {
    let root = tempdir().expect("tempdir");
    let state_dir = root.path().join("state");
    let displaced = root.path().join("state.displaced");
    let target = state_dir.join("record.json");
    fs::create_dir(&state_dir).expect("state directory");
    fs::write(&target, b"original").expect("original state");
    let directory = StableDirectory::open(&state_dir).expect("stable directory");
    let expected = directory.open_read(&target).expect("expected state");

    let failure = directory
        .save_bytes_atomically_expected_with_hooks(
            &target,
            b"committed",
            ".receipt-race-",
            AtomicSaveExpectation {
                require_attached_before_commit: true,
                file: FileExpectation::Present(&expected),
                retain_publication_lock: false,
            },
            || {
                fs::rename(&state_dir, &displaced)
                    .map_err(|error| format!("failed to detach state directory: {error}"))?;
                fs::create_dir(&state_dir)
                    .map_err(|error| format!("failed to create replacement state: {error}"))?;
                fs::write(&target, b"replacement")
                    .map_err(|error| format!("failed to seed replacement state: {error}"))
            },
            || {},
        )
        .expect_err("detached publication must report its receipt with the error");

    assert!(failure.message.contains("identity changed"), "{failure:?}");
    let receipt = failure
        .receipt
        .expect("post-publication failure must retain a receipt");
    assert!(receipt.exact_identity);
    let committed_path = displaced.join("record.json");
    let committed_directory = StableDirectory::open(&displaced).expect("committed directory");
    let committed = committed_directory
        .open_read(&committed_path)
        .expect("committed state");
    assert!(same_open_file_identity(&receipt.file, &committed).expect("same identity"));
    assert_eq!(fs::read(&target).expect("replacement"), b"replacement");
}

#[cfg(unix)]
#[test]
fn paired_atomic_save_commits_original_tree_after_effects_despite_detachment() {
    let root = tempdir().expect("tempdir");
    let state_dir = root.path().join("state");
    let displaced = root.path().join("state.displaced");
    let target = state_dir.join("record.json");
    fs::create_dir(&state_dir).expect("state directory");
    fs::write(&target, b"before-effect").expect("original state");
    let directory = StableDirectory::open(&state_dir).expect("stable directory");

    fs::rename(&state_dir, &displaced).expect("detach state directory");
    fs::create_dir(&state_dir).expect("replacement state directory");
    fs::write(&target, b"replacement").expect("replacement sentinel");

    let error = directory
        .save_bytes_atomically_after_effects(&target, b"paired-transition", ".paired-")
        .expect_err("paired commit must still report detachment");
    assert!(error.contains("identity changed"), "{error}");
    assert_eq!(
        fs::read(displaced.join("record.json")).expect("paired original commit"),
        b"paired-transition"
    );
    assert_eq!(
        fs::read(&target).expect("replacement sentinel"),
        b"replacement"
    );
}

#[test]
fn handle_bound_publication_supports_present_and_missing_expectations() {
    let root = tempdir().expect("tempdir");
    let existing = root.path().join("existing.json");
    let missing = root.path().join("missing.json");
    fs::write(&existing, b"old").expect("existing state");
    let directory = StableDirectory::open(root.path()).expect("stable directory");
    let expected = directory.open_read(&existing).expect("expected state");

    directory
        .save_bytes_atomically_expected(
            &existing,
            b"new",
            ".runtime-publication-",
            FileExpectation::Present(&expected),
        )
        .expect("publish over retained state");
    directory
        .save_bytes_atomically_expected(
            &missing,
            b"created",
            ".runtime-publication-",
            FileExpectation::Missing,
        )
        .expect("publish into proven absence");

    assert_eq!(fs::read(existing).expect("updated state"), b"new");
    assert_eq!(fs::read(missing).expect("created state"), b"created");
}

#[cfg(windows)]
#[test]
fn delete_capable_child_accepts_an_equivalent_dos_short_parent() {
    let root = tempdir().expect("tempdir");
    let canonical = root.path().canonicalize().expect("canonical tempdir");
    let state = canonical.join(".nib");
    fs::create_dir(&state).expect("state directory");
    let short_state = crate::fs_security::windows_dos_short_path_for_test(&state)
        .expect("DOS short state directory");
    let root_directory = StableDirectory::open(&canonical).expect("stable canonical directory");
    let directory = root_directory
        .open_owned_child(&state)
        .expect("delete-capable state directory");
    let short_target = short_state.join("short-parent.json");

    directory
        .save_bytes_atomically_expected(
            &short_target,
            b"short-parent",
            ".short-parent-publication-",
            FileExpectation::Missing,
        )
        .expect("publish through equivalent DOS short parent");

    assert_eq!(
        fs::read(state.join("short-parent.json")).expect("canonical publication"),
        b"short-parent"
    );
}

#[cfg(windows)]
#[test]
fn stable_descendant_walk_does_not_retain_delete_access_on_namespaces() {
    let root = tempdir().expect("tempdir");
    let canonical = root.path().canonicalize().expect("canonical tempdir");
    let state = canonical.join(".nib");
    let records = state.join("process-scopes");
    fs::create_dir(&state).expect("state directory");
    let root_directory = StableDirectory::open(&canonical).expect("stable canonical directory");
    let deadline = Instant::now() + Duration::from_secs(2);
    let retained_records = root_directory
        .open_or_create_descendant_directory_with_guard(
            &records,
            || ensure_directory_namespace_deadline(deadline, &records),
            |_| Ok(()),
        )
        .expect("retained stable namespace");

    let reopened_state = StableDirectory::open(&state)
        .expect("stable descendant capability must not block its ancestor");
    let reopened_records = reopened_state
        .open_child(&records)
        .expect("reopen retained namespace from its ancestor");
    assert!(retained_records.same_identity(&reopened_records));
}

#[cfg(windows)]
#[test]
fn publication_receipt_does_not_retain_a_mandatory_byte_lock() {
    let root = tempdir().expect("tempdir");
    let target = root.path().join("receipt.json");
    let directory = StableDirectory::open(root.path()).expect("stable directory");

    let receipt = directory
        .save_bytes_atomically_expected_with_receipt(
            &target,
            b"published",
            ".receipt-publication-",
            FileExpectation::Missing,
        )
        .expect("publish with retained receipt");

    assert!(receipt.exact_identity);
    assert_eq!(
        read_open_file_prefix(&receipt.file, b"published".len() + 1)
            .expect("read retained receipt"),
        b"published"
    );
    assert_eq!(
        fs::read(&target).expect("read while receipt remains alive"),
        b"published"
    );
    let contender = directory
        .open_read_write(&target)
        .expect("open publication contender");
    contender
        .try_lock()
        .expect("generic receipt must not retain the publication lock");
    contender.unlock().expect("release publication contender");
    assert!(same_open_file_identity(
        &receipt.file,
        &directory.open_read(&target).expect("reopen publication")
    )
    .expect("receipt identity"));
}

#[cfg(any(unix, windows))]
#[test]
fn locked_publication_receipt_continuously_excludes_recovery() {
    let root = tempdir().expect("tempdir");
    let target = root.path().join("owned-receipt.json");
    let directory = StableDirectory::open(root.path()).expect("stable directory");
    let mut checked_before_return = false;

    let receipt = directory
        .save_bytes_atomically_expected_with_locked_receipt_before_return(
            &target,
            b"owned",
            ".owned-receipt-publication-",
            FileExpectation::Missing,
            || {
                assert!(target.is_file(), "publication is visible before return");
                let contender = directory
                    .open_read_write(&target)
                    .expect("open pre-return recovery contender");
                assert!(matches!(
                    contender.try_lock(),
                    Err(std::fs::TryLockError::WouldBlock)
                ));
                checked_before_return = true;
            },
        )
        .expect("publish retained locked receipt");
    assert!(checked_before_return);
    assert_eq!(
        read_open_file_prefix(&receipt.file, b"owned".len() + 1)
            .expect("read through lock-owning receipt"),
        b"owned"
    );

    let contender = directory
        .open_read_write(&target)
        .expect("open recovery contender");
    assert!(matches!(
        contender.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));

    drop(receipt);
    contender
        .try_lock()
        .expect("dropping the receipt releases ownership");
    contender.unlock().expect("release contender lock");
}

#[cfg(windows)]
#[test]
fn windows_delete_access_supports_handle_bound_directory_quarantine() {
    let root = tempdir().expect("tempdir");
    let source = root.path().join("managed-skill");
    let quarantine = root.path().join("managed-skill.quarantine");
    fs::create_dir(&source).expect("source directory");
    fs::write(source.join("marker.json"), b"managed").expect("source marker");
    let directory = StableDirectory::open(root.path()).expect("stable directory");
    let ordinary = directory.open_child(&source).expect("ordinary source");
    let error = directory
        .rename_child_directory(&source, &ordinary, &quarantine)
        .expect_err("ordinary namespace capability must not mutate");
    assert!(
        error.contains("lacks the retained DELETE capability"),
        "{error}"
    );
    drop(ordinary);
    let source_directory = directory
        .open_owned_child(&source)
        .expect("source capability");

    directory
        .rename_child_directory(&source, &source_directory, &quarantine)
        .expect("handle-bound directory quarantine");
    source_directory
        .verify_visible_at(&quarantine)
        .expect("quarantined source identity");

    assert!(!source.exists());
    assert_eq!(
        fs::read(quarantine.join("marker.json")).expect("quarantined marker"),
        b"managed"
    );
}

#[cfg(windows)]
#[test]
fn windows_handle_delete_preserves_a_replacement_after_the_final_identity_check() {
    let root = tempdir().expect("tempdir");
    let target = root.path().join("record.json");
    let displaced = root.path().join("record.displaced.json");
    fs::write(&target, b"expected").expect("expected state");
    let directory = StableDirectory::open(root.path()).expect("stable directory");
    let expected = directory
        .open_read_write(&target)
        .expect("expected state handle");

    directory
        .remove_bound_file_if_matches_with_hook_after_identity(
            &target,
            &expected,
            || Ok(()),
            || {
                fs::rename(&target, &displaced).map_err(|error| error.to_string())?;
                fs::write(&target, b"replacement").map_err(|error| error.to_string())
            },
        )
        .expect("handle-bound deletion");
    drop(expected);

    assert_eq!(fs::read(&target).expect("replacement"), b"replacement");
    assert!(
        !displaced.exists(),
        "opened original must be deleted by handle"
    );
}

#[cfg(windows)]
#[test]
fn windows_conditional_removal_deletes_the_requested_hard_link_alias() {
    let root = tempdir().expect("tempdir");
    let anchor = root.path().join("anchor.json");
    let visible = root.path().join("visible.json");
    fs::write(&anchor, b"owned").expect("anchor state");
    fs::hard_link(&anchor, &visible).expect("visible hard link");
    let directory = StableDirectory::open(root.path()).expect("stable directory");
    let expected = directory.open_read(&anchor).expect("anchor handle");

    directory
        .remove_file_if_matches(&visible, &expected, ".hard-link-delete-")
        .expect("remove requested alias");

    assert!(!visible.exists(), "requested alias must be deleted");
    assert_eq!(fs::read(&anchor).expect("anchor remains"), b"owned");
}

#[test]
fn temporary_path_substitution_after_fsync_preserves_destination_and_replacement() {
    let root = tempdir().expect("tempdir");
    let target = root.path().join("record.json");
    fs::write(&target, b"old").expect("old state");
    let directory = StableDirectory::open(root.path()).expect("stable directory");
    let expected = directory.open_read(&target).expect("expected state");
    let temporary = root.path().join(deterministic_artifact_name(
        ".temp-substitution-",
        b"record.json",
        ".tmp",
    ));
    let displaced = root.path().join("displaced-temp");

    let error = directory
        .save_bytes_atomically_expected_with_hook(
            &target,
            b"new",
            ".temp-substitution-",
            true,
            FileExpectation::Present(&expected),
            || {
                fs::rename(&temporary, &displaced).map_err(|error| error.to_string())?;
                fs::write(&temporary, b"replacement-temp").map_err(|error| error.to_string())
            },
        )
        .expect_err("substituted temporary pathname must fail closed");

    assert!(error.contains("identity changed"), "{error}");
    assert_eq!(fs::read(&target).expect("destination"), b"old");
    assert_eq!(
        fs::read(&temporary).expect("replacement temporary"),
        b"replacement-temp"
    );
    assert_eq!(fs::read(displaced).expect("original temporary"), b"new");
}

#[test]
fn publication_conflict_after_evacuation_preserves_prior_and_new_target() {
    let root = tempdir().expect("tempdir");
    let target = root.path().join("record.json");
    fs::write(&target, b"prior").expect("prior state");
    let directory = StableDirectory::open(root.path()).expect("stable directory");
    let expected = directory.open_read(&target).expect("prior handle");
    let previous = root.path().join(
        deterministic_previous_artifact_name(
            ".rollback-ambiguity-",
            std::ffi::OsStr::new("record.json"),
        )
        .expect("previous artifact name"),
    );

    let failure = directory
        .save_bytes_atomically_expected_with_hooks(
            &target,
            b"attempted",
            ".rollback-ambiguity-",
            AtomicSaveExpectation {
                require_attached_before_commit: true,
                file: FileExpectation::Present(&expected),
                retain_publication_lock: false,
            },
            || Ok(()),
            || fs::write(&target, b"new-target").expect("conflicting target"),
        )
        .expect_err("identity-distinct publication conflict must fail closed");

    assert!(failure.message.contains("identity-distinct"), "{failure:?}");
    assert!(failure.receipt.is_none());
    assert_eq!(fs::read(&target).expect("new target"), b"new-target");
    assert_eq!(fs::read(&previous).expect("evacuated prior"), b"prior");
}

#[test]
fn failed_publication_finalization_validates_bytes_before_prior_cleanup() {
    let root = tempdir().expect("tempdir");
    let target = root.path().join("record.json");
    let previous = root.path().join(
        deterministic_previous_artifact_name(
            ".finalize-bytes-",
            std::ffi::OsStr::new("record.json"),
        )
        .expect("previous artifact name"),
    );
    fs::write(&target, b"corrupt").expect("corrupt target");
    fs::write(&previous, b"prior").expect("prior state");
    let directory = StableDirectory::open(root.path()).expect("stable directory");
    let target_file = directory.open_read(&target).expect("target handle");
    let previous_file = directory.open_read(&previous).expect("previous handle");
    let receipt = FilePublicationReceipt {
        file: target_file,
        exact_identity: true,
    };

    let error = directory
        .finalize_failed_exact_publication(
            &target,
            Some(&previous_file),
            &receipt,
            ".finalize-bytes-",
            b"expected",
        )
        .expect_err("corrupt publication must not be finalized");

    assert!(error.contains("bytes changed"), "{error}");
    assert_eq!(fs::read(&target).expect("corrupt target"), b"corrupt");
    assert_eq!(fs::read(&previous).expect("prior retained"), b"prior");
}

#[cfg(unix)]
#[test]
fn temporary_cleanup_preserves_exact_link_if_target_is_substituted() {
    let root = tempdir().expect("tempdir");
    let temporary = root.path().join("record.tmp");
    let target = root.path().join("record.json");
    let displaced = root.path().join("record.displaced");
    fs::write(&temporary, b"managed").expect("temporary state");
    fs::hard_link(&temporary, &target).expect("published hard link");
    let directory = StableDirectory::open(root.path()).expect("stable directory");
    let expected = directory.open_read(&temporary).expect("temporary handle");

    let error = directory
        .cleanup_open_temporary_file(&temporary, &expected, || {
            fs::rename(&target, &displaced).map_err(|error| error.to_string())?;
            fs::write(&target, b"replacement").map_err(|error| error.to_string())?;
            directory.verify_publication_bytes(&target, &expected, b"managed")
        })
        .expect_err("target substitution must preserve the exact temporary link");

    assert!(error.contains("identity changed"), "{error}");
    assert_eq!(fs::read(&temporary).expect("managed link"), b"managed");
    assert_eq!(fs::read(&target).expect("replacement"), b"replacement");
    assert_eq!(fs::read(&displaced).expect("displaced target"), b"managed");
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
#[test]
fn pathname_publication_source_swap_is_rescued_and_prior_is_restored() {
    let root = tempdir().expect("tempdir");
    let target = root.path().join("record.json");
    fs::write(&target, b"prior").expect("prior state");
    let directory = StableDirectory::open(root.path()).expect("stable directory");
    let expected = directory.open_read(&target).expect("prior handle");
    let temporary = root.path().join(deterministic_artifact_name(
        ".mac-publication-swap-",
        b"record.json",
        ".tmp",
    ));
    let displaced = root.path().join("attempted.displaced");

    let failure = directory
        .save_bytes_atomically_expected_with_hooks(
            &target,
            b"attempted",
            ".mac-publication-swap-",
            AtomicSaveExpectation {
                require_attached_before_commit: true,
                file: FileExpectation::Present(&expected),
                retain_publication_lock: false,
            },
            || Ok(()),
            || {
                fs::rename(&temporary, &displaced).expect("displace attempted state");
                fs::write(&temporary, b"replacement-temp").expect("replacement temporary");
            },
        )
        .expect_err("pathname source swap must fail closed");

    assert!(failure.message.contains("source changed"), "{failure:?}");
    assert!(failure.receipt.is_none());
    assert_eq!(fs::read(&target).expect("restored prior"), b"prior");
    assert_eq!(
        fs::read(&temporary).expect("rescued replacement"),
        b"replacement-temp"
    );
    assert_eq!(fs::read(&displaced).expect("attempted state"), b"attempted");
}

#[cfg(unix)]
#[test]
fn source_substitution_before_quarantine_is_rescued_without_deleting_either_file() {
    let root = tempdir().expect("tempdir");
    let target = root.path().join("record.json");
    fs::write(&target, b"expected").expect("expected state");
    let directory = StableDirectory::open(root.path()).expect("stable directory");
    let expected = directory.open_read(&target).expect("expected state handle");
    let displaced = root.path().join("displaced-expected");
    let quarantine = directory
        .deterministic_artifact_path(&target, ".source-swap-", ".quarantine")
        .expect("quarantine path");

    let error = directory
        .move_open_file_no_replace_bound_with_hook(&target, &expected, &quarantine, || {
            fs::rename(&target, &displaced).map_err(|error| error.to_string())?;
            fs::write(&target, b"replacement").map_err(|error| error.to_string())
        })
        .expect_err("source substitution must fail closed");

    assert!(error.contains("state source changed"), "{error}");
    assert_eq!(fs::read(&target).expect("replacement"), b"replacement");
    assert_eq!(fs::read(&displaced).expect("expected"), b"expected");
    assert!(!quarantine.exists());
}

#[test]
fn late_quarantine_substitution_is_preserved() {
    let root = tempdir().expect("tempdir");
    let target = root.path().join("record.json");
    fs::write(&target, b"old").expect("old state");
    let directory = StableDirectory::open(root.path()).expect("stable directory");
    let expected = directory.open_read(&target).expect("expected state");
    let quarantine = directory
        .deterministic_artifact_path(&target, ".late-delete-", ".quarantine")
        .expect("quarantine path");
    let displaced = root.path().join("expected-quarantine");

    let error = directory
        .remove_file_if_matches_with_hooks(
            &target,
            &expected,
            ".late-delete-",
            || Ok(()),
            || {
                fs::rename(&quarantine, &displaced).map_err(|error| error.to_string())?;
                fs::write(&quarantine, b"replacement-quarantine").map_err(|error| error.to_string())
            },
        )
        .expect_err("late quarantine replacement must fail closed");

    assert!(error.contains("identity changed"), "{error}");
    assert_eq!(
        fs::read(&quarantine).expect("replacement quarantine"),
        b"replacement-quarantine"
    );
    assert_eq!(fs::read(displaced).expect("expected quarantine"), b"old");
}
