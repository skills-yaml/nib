use super::*;

#[test]
fn post_scan_live_legacy_pair_and_anchor_are_detected_before_success() {
    assert_post_scan_live_legacy_publication_is_rejected(false);
    assert_post_scan_live_legacy_publication_is_rejected(true);
}

#[cfg(any(unix, windows))]
#[test]
fn post_scan_legacy_directory_replacement_is_rejected_before_mutation() {
    let root = tempfile::tempdir().expect("root");
    let records = ensure_records_directory(root.path()).expect("records");
    let legacy_locks = records.join(".locks");
    std::fs::create_dir(&legacy_locks).expect("initial legacy directory");
    let displaced = records.join(".locks.displaced");
    let visible = legacy_locks.join("post-scan-replacement.lock");
    let anchor = crate::daemons::state::daemon_lock_anchor_path(&visible).expect("legacy anchor");
    let mut owner = None;

    let error = confirm_no_legacy_subagent_processes_with_scan_hook(root.path(), |pass| {
        if pass != 0 {
            return Ok(());
        }
        std::fs::rename(&legacy_locks, &displaced)
            .map_err(|error| format!("displace legacy directory: {error}"))?;
        std::fs::create_dir(&legacy_locks)
            .map_err(|error| format!("create replacement legacy directory: {error}"))?;
        std::fs::write(&visible, b"replacement-live")
            .map_err(|error| format!("write replacement legacy lock: {error}"))?;
        std::fs::hard_link(&visible, &anchor)
            .map_err(|error| format!("link replacement legacy anchor: {error}"))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&anchor)
            .map_err(|error| format!("open replacement legacy owner: {error}"))?;
        file.try_lock()
            .map_err(|error| format!("hold replacement legacy owner: {error}"))?;
        owner = Some(file);
        Ok(())
    })
    .expect_err("a replaced legacy namespace must fail before mutation");

    assert!(
        error.contains("state changed") || error.contains("identity changed"),
        "{error}"
    );
    assert!(visible.is_file(), "replacement legacy lock was preserved");
    assert!(anchor.is_file(), "replacement legacy anchor was preserved");
    assert!(
        displaced.is_dir(),
        "the retained original directory was preserved"
    );

    drop(owner.take());
    assert_eq!(
        std::fs::read(&visible).expect("replacement legacy lock"),
        b"replacement-live"
    );
    assert_eq!(
        std::fs::read(&anchor).expect("replacement legacy anchor"),
        b"replacement-live"
    );
    confirm_no_legacy_subagent_processes(root.path())
        .expect("a fresh attestation cleans the released replacement artifacts");
    assert!(!visible.exists(), "released replacement lock was removed");
    assert!(!anchor.exists(), "released replacement anchor was removed");
    assert!(
        displaced.is_dir(),
        "unrelated displaced state was untouched"
    );
}

#[cfg(any(unix, windows))]
#[test]
fn record_bridge_blocks_a_live_legacy_contender_published_after_migration() {
    let initial_bridge_timeout = if cfg!(windows) {
        Duration::from_secs(5)
    } else {
        Duration::from_millis(200)
    };
    let root = tempfile::tempdir().expect("root");
    let records = ensure_records_directory(root.path()).expect("records");
    let id = "post-migration-legacy-contender";
    let legacy = legacy_record_lock_path(&records, id).expect("legacy lock path");
    let anchor = crate::daemons::state::daemon_lock_anchor_path(&legacy).expect("legacy anchor");
    let modern = record_lock_path(root.path(), id).expect("modern lock path");
    let mut owner = None;
    let mut entered = false;

    let error = with_subagent_record_lock_bridge_in_deadline(
        root.path(),
        id,
        &records,
        Instant::now() + initial_bridge_timeout,
        None,
        || {
            std::fs::create_dir(legacy.parent().expect("legacy lock parent"))
                .map_err(|error| format!("create legacy lock directory: {error}"))?;
            std::fs::write(&legacy, b"live-old-writer")
                .map_err(|error| format!("write legacy lock: {error}"))?;
            std::fs::hard_link(&legacy, &anchor)
                .map_err(|error| format!("link legacy anchor: {error}"))?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&anchor)
                .map_err(|error| format!("open legacy owner: {error}"))?;
            file.try_lock()
                .map_err(|error| format!("hold legacy owner: {error}"))?;
            owner = Some(file);
            Ok(())
        },
        |_, _| {
            entered = true;
            Ok(())
        },
    )
    .expect_err("post-migration legacy contender must fence the modern operation");

    assert!(error.contains("confirm-no-legacy-processes"), "{error}");
    assert!(
        !entered,
        "modern critical section did not overlap the old writer"
    );
    assert!(
        !modern.exists(),
        "modern stripe was not acquired after legacy contention"
    );
    assert!(legacy.is_file(), "live legacy lock was preserved");
    assert!(anchor.is_file(), "live legacy anchor was preserved");

    drop(owner.take());
    assert_eq!(
        std::fs::read(&legacy).expect("preserved legacy lock"),
        b"live-old-writer"
    );
    assert_eq!(
        std::fs::read(&anchor).expect("preserved legacy anchor"),
        b"live-old-writer"
    );
    let error = with_subagent_record_lock_bridge_in_deadline(
        root.path(),
        id,
        &records,
        Instant::now() + Duration::from_secs(2),
        None,
        || Ok(()),
        |_, _| Ok(()),
    )
    .expect_err("a completed epoch never consumes newly introduced legacy state");
    assert!(error.contains("confirm-no-legacy-processes"), "{error}");
    confirm_no_legacy_subagent_processes(root.path())
        .expect("fresh offline attestation cleans the released contender");
    let mut observed_cleanup = false;
    with_subagent_record_lock_bridge_in_deadline(
        root.path(),
        id,
        &records,
        Instant::now() + Duration::from_secs(2),
        None,
        || Ok(()),
        |_, _| {
            observed_cleanup = !legacy.exists() && !anchor.exists();
            Ok(())
        },
    )
    .expect("fresh bridge cleans the dead contender and enters safely");
    assert!(
        observed_cleanup,
        "dead legacy identity was removed before ordinary fixed-stripe use resumed"
    );
}

#[cfg(any(unix, windows))]
#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn offline_epoch_preserves_an_open_before_lock_contender_until_operator_quiescence() {
    let root = tempfile::tempdir().expect("root");
    let records = ensure_records_directory(root.path()).expect("native records namespace");
    let legacy = legacy_record_lock_path(&records, LEGACY_OPEN_CHILD_ID).expect("legacy lock path");
    let anchor = crate::daemons::state::daemon_lock_anchor_path(&legacy).expect("legacy anchor");
    std::fs::create_dir(legacy.parent().expect("legacy parent")).expect("legacy lock directory");
    std::fs::write(&legacy, b"prior-version-contender").expect("legacy lock");
    std::fs::hard_link(&legacy, &anchor).expect("legacy anchor");
    let opened_before = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&anchor)
        .expect("observe exact legacy anchor");
    let identity_before = crate::fs_security::file_identity_snapshot(&opened_before)
        .expect("legacy identity before child open");
    let ready = root.path().join("legacy-open.ready");
    let resume = root.path().join("legacy-open.resume");
    let locked = root.path().join("legacy-open.locked");
    let release = root.path().join("legacy-open.release");
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "tools::delegation::tests::test_part_1::legacy_open_before_lock_child_process",
            "--nocapture",
        ])
        .env(LEGACY_OPEN_CHILD_PROJECT_ROOT, root.path())
        .env(LEGACY_OPEN_CHILD_READY, &ready)
        .env(LEGACY_OPEN_CHILD_RESUME, &resume)
        .env(LEGACY_OPEN_CHILD_LOCKED, &locked)
        .env(LEGACY_OPEN_CHILD_RELEASE, &release)
        .spawn()
        .expect("spawn paused prior-version contender");
    let wait_for = |path: &Path, child: &mut std::process::Child| {
        let started = Instant::now();
        while !path.exists() {
            if let Some(status) = child.try_wait().expect("inspect legacy child") {
                panic!("legacy child exited before {}: {status}", path.display());
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "legacy child did not publish {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    wait_for(&ready, &mut child);

    let mut entered = false;
    let error = with_subagent_record_lock_bridge_in_deadline(
        root.path(),
        LEGACY_OPEN_CHILD_ID,
        &records,
        Instant::now() + Duration::from_secs(1),
        None,
        || Ok(()),
        |_, _| {
            entered = true;
            Ok(())
        },
    )
    .expect_err("ordinary operation must preserve an open-but-unlocked legacy inode");
    assert!(error.contains("confirm-no-legacy-processes"), "{error}");
    assert!(!entered);
    let reopened = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&anchor)
        .expect("preserved legacy anchor");
    assert_eq!(
        crate::fs_security::file_identity_snapshot(&reopened)
            .expect("legacy identity after refusal"),
        identity_before,
        "ordinary refusal neither deletes nor recreates the inode already opened by the child"
    );

    std::fs::write(&resume, b"lock exact inode").expect("resume legacy child");
    wait_for(&locked, &mut child);
    std::fs::write(&release, b"operator stopped old binary").expect("release legacy child");
    let status = child.wait().expect("reap legacy child");
    assert!(status.success(), "legacy child failed: {status}");
    drop(reopened);
    drop(opened_before);

    assert_eq!(
        confirm_no_legacy_subagent_processes(root.path())
            .expect("attestation is accepted only after the prior binary exits"),
        2
    );
    with_subagent_record_lock_bridge_in_deadline(
        root.path(),
        LEGACY_OPEN_CHILD_ID,
        &records,
        Instant::now() + Duration::from_secs(1),
        None,
        || Ok(()),
        |_, _| Ok(()),
    )
    .expect("completed epoch enables the fixed modern stripe");
    assert!(!legacy.exists());
    assert!(!anchor.exists());

    std::fs::write(&legacy, b"late-prior-version-state").expect("late legacy lock");
    std::fs::hard_link(&legacy, &anchor).expect("late legacy anchor");
    let error = with_subagent_record_lock_bridge_in_deadline(
        root.path(),
        LEGACY_OPEN_CHILD_ID,
        &records,
        Instant::now() + Duration::from_secs(1),
        None,
        || Ok(()),
        |_, _| Ok(()),
    )
    .expect_err("a stale completed epoch cannot authorize newly introduced legacy state");
    assert!(error.contains("confirm-no-legacy-processes"), "{error}");
    assert!(legacy.exists());
    assert!(anchor.exists());
}

#[cfg(any(unix, windows))]
#[test]
fn record_bridge_uses_only_the_modern_stripe_and_obeys_one_deadline() {
    let root = tempfile::tempdir().expect("root");
    let records = ensure_records_directory(root.path()).expect("records");
    let id = "record-bridge-lock-order";
    let modern = record_lock_path(root.path(), id).expect("modern lock path");
    let modern_anchor =
        crate::daemons::state::daemon_lock_anchor_path(&modern).expect("modern anchor");
    let held_modern =
        open_repository_merge_lock_anchor(&modern, &modern_anchor).expect("modern lock pair");
    held_modern.try_lock().expect("hold modern stripe");
    let legacy = legacy_record_lock_path(&records, id).expect("legacy lock path");
    let legacy_anchor =
        crate::daemons::state::daemon_lock_anchor_path(&legacy).expect("legacy anchor");
    let marker = records.join("record-bridge-operation.marker");
    let started = Instant::now();

    let error = with_subagent_record_lock_bridge_in_deadline(
        root.path(),
        id,
        &records,
        Instant::now() + Duration::from_millis(200),
        None,
        || Ok(()),
        |_, _| std::fs::write(&marker, b"late mutation").map_err(|error| error.to_string()),
    )
    .expect_err("held modern stripe must expire after the legacy fence is acquired");

    assert!(
        error.contains("delegation state lock deadline elapsed"),
        "{error}"
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(!legacy.exists(), "modern bridges never create legacy locks");
    assert!(
        !legacy_anchor.exists(),
        "modern bridges never create legacy anchors"
    );
    assert!(
        !marker.exists(),
        "expired bridge did not enter record mutation"
    );
    std::thread::sleep(Duration::from_millis(50));
    assert!(!marker.exists(), "no record mutation occurred after return");

    drop(held_modern);
    with_subagent_record_lock_bridge_in_deadline(
        root.path(),
        id,
        &records,
        Instant::now() + Duration::from_secs(2),
        None,
        || Ok(()),
        |_, _| std::fs::write(&marker, b"fresh").map_err(|error| error.to_string()),
    )
    .expect("fresh deadline acquires the fixed modern stripe");
    assert_eq!(std::fs::read(&marker).expect("fresh mutation"), b"fresh");
}

#[cfg(unix)]
#[test]
fn record_bridge_rejects_whole_records_replacement_before_sensitive_mutation() {
    let root = tempfile::tempdir().expect("root");
    let records = ensure_records_directory(root.path()).expect("records");
    let original_sentinel = records.join("original.sentinel");
    std::fs::write(&original_sentinel, b"original records").expect("original records sentinel");
    let process_state = root.path().join(".nib").join("process-scopes");
    let audit_state = root.path().join(".nib").join("sessions");
    std::fs::create_dir(&process_state).expect("process state directory");
    std::fs::create_dir(&audit_state).expect("audit state directory");
    std::fs::write(process_state.join("scope.sentinel"), b"process state")
        .expect("process sentinel");
    std::fs::write(audit_state.join("audit.sentinel"), b"audit state").expect("audit sentinel");
    let original_before = directory_tree_snapshot(&records);
    let process_before = directory_tree_snapshot(&process_state);
    let audit_before = directory_tree_snapshot(&audit_state);
    let displaced = root.path().join(".nib").join("subagents.displaced");
    let record = record_fixture(root.path(), "records-capability-replacement", "running");
    let path = record_path(root.path(), &record.id).expect("record path");
    let mut replacement_before = None;
    let mut entered = false;

    let error = with_subagent_record_lock_bridge_in_deadline(
        root.path(),
        &record.id,
        &records,
        Instant::now() + Duration::from_secs(2),
        None,
        || {
            std::fs::rename(&records, &displaced)
                .map_err(|error| format!("displace records directory: {error}"))?;
            std::fs::create_dir(&records)
                .map_err(|error| format!("create replacement records directory: {error}"))?;
            std::fs::write(records.join("replacement.sentinel"), b"replacement records")
                .map_err(|error| format!("write replacement sentinel: {error}"))?;
            replacement_before = Some(directory_tree_snapshot(&records));
            Ok(())
        },
        |directory, deadline| {
            entered = true;
            write_subagent_record_unlocked_until(
                root.path(),
                directory,
                &path,
                &record,
                crate::daemons::state::FileExpectation::Missing,
                deadline,
            )?;
            std::fs::write(process_state.join("scope.sentinel"), b"redirected")
                .map_err(|error| error.to_string())?;
            std::fs::write(audit_state.join("audit.sentinel"), b"redirected")
                .map_err(|error| error.to_string())
        },
    )
    .expect_err("whole records replacement must detach the retained capability");

    assert!(error.contains("identity changed"), "{error}");
    assert!(!entered, "the sensitive record callback was not entered");
    assert_eq!(
        directory_tree_snapshot(&displaced),
        original_before,
        "the retained original namespace was not mutated after displacement"
    );
    assert_eq!(
        directory_tree_snapshot(&records),
        replacement_before.expect("replacement snapshot"),
        "the replacement namespace did not receive redirected lock or record state"
    );
    assert_eq!(directory_tree_snapshot(&process_state), process_before);
    assert_eq!(directory_tree_snapshot(&audit_state), audit_before);
    assert!(!displaced.join(format!("{}.json", record.id)).exists());
    assert!(!path.exists());
}

#[test]
fn pending_merge_intent_preserves_subagent_result_and_integration_identity() {
    let root = tempfile::tempdir().expect("root");
    let mut record = SubagentRecord {
        id: "sub-pending".to_string(),
        parent_session_id: Some("parent".to_string()),
        child_session_id: "child-pending".to_string(),
        prompt: "fixture".to_string(),
        status: "completed".to_string(),
        execution_generation: None,
        owner_lease: None,
        worktree_path: root.path().join("worktree"),
        branch: "nib/subagent/sub-pending".to_string(),
        branch_oid: Some("a".repeat(40)),
        result: Some(json!({"summary": "verified"})),
        error: None,
        verification: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    begin_pending_merge(&mut record, "task test", &"a".repeat(40), &"b".repeat(40));

    let intent = pending_merge_intent(&record).expect("pending intent");
    assert_eq!(record.status, MERGE_PENDING_STATUS);
    assert_eq!(intent.branch_commit, "a".repeat(40));
    assert_eq!(intent.parent_head, "b".repeat(40));
    assert_eq!(intent.verification_command, "task test");
    assert_eq!(
        record.result.as_ref().unwrap()["subagent_result"]["summary"],
        "verified"
    );
    assert!(record.result.as_ref().unwrap()["merge_stdout"].is_null());
}

#[cfg(unix)]
#[tokio::test]
async fn repository_merge_lock_times_out_when_another_holder_does_not_release() {
    let root = tempfile::tempdir().expect("root");
    let _held = RepositoryMergeLock::acquire_with_timeout(root.path(), Duration::from_secs(1))
        .await
        .expect("first lock");
    let started = Instant::now();

    let error = RepositoryMergeLock::acquire_with_timeout(root.path(), Duration::from_millis(75))
        .await
        .expect_err("second lock must time out");

    assert!(error.contains("timed out acquiring repository merge lock"));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn repository_merge_lock_default_timeout_is_thirty_seconds() {
    assert_eq!(REPOSITORY_MERGE_LOCK_TIMEOUT, Duration::from_secs(30));
}

#[cfg(any(unix, windows))]
#[test]
fn expired_absolute_delegation_lock_rejects_free_lock_before_operation() {
    let root = tempfile::tempdir().expect("root");
    let records = ensure_records_directory(root.path()).expect("records");
    let lock_path = record_lock_path(root.path(), "sub-expired-free").expect("record lock");
    let mut operation_ran = false;

    let error = with_bounded_delegation_lock_in_until(
        &lock_path,
        &records,
        Instant::now() - Duration::from_millis(1),
        |_, _| {
            operation_ran = true;
            Ok(())
        },
    )
    .expect_err("an expired absolute deadline must reject an uncontended lock");

    assert!(error.contains("delegation state lock deadline elapsed"));
    assert!(!operation_ran, "expired delegation lock entered mutation");
    assert!(!lock_path.exists(), "expired delegation lock created state");
}

#[cfg(any(unix, windows))]
#[test]
fn records_initialization_deadline_guards_nib_creation_and_retries_safely() {
    let root = tempfile::tempdir().expect("root");
    let nib = root.path().join(".nib");
    let deadline = Instant::now() + expiry_checkpoint_timeout();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let worker_root = root.path().to_path_buf();
    let worker_nib = nib.clone();
    let worker = std::thread::spawn(move || {
        let mut paused = false;
        open_or_create_records_directory_with_setup_hook(&worker_root, Some(deadline), |path| {
            if !paused && path == worker_nib {
                paused = true;
                ready_tx.send(()).expect("signal nib create boundary");
                resume_rx.recv().expect("resume nib create boundary");
            }
            Ok(())
        })
        .expect_err("expired nib creation must fail closed")
    });
    ready_rx
        .recv_timeout(expiry_checkpoint_wait())
        .expect("records initialization reached nib creation");
    let paused = subagent_namespace_snapshot(root.path());
    while Instant::now() < deadline {
        std::thread::yield_now();
    }
    resume_tx.send(()).expect("release nib create boundary");
    let error = worker.join().expect("join records initializer");
    assert!(error.contains("deadline elapsed"), "{error}");
    assert_eq!(subagent_namespace_snapshot(root.path()), paused);
    assert!(!nib.exists());

    let records = open_or_create_records_directory(
        root.path(),
        Some(Instant::now() + Duration::from_secs(2)),
    )
    .expect("fresh deadline retries exact records initialization");
    assert!(records.is_dir());
}

#[cfg(any(unix, windows))]
#[test]
fn records_setup_and_migration_share_one_default_absolute_deadline() {
    let root = tempfile::tempdir().expect("root");
    let mut paused_namespace = None;
    let default_timeout = Duration::from_secs(2);
    let error = ensure_records_directory_until_with_phase_hook(
        root.path(),
        None,
        default_timeout,
        |effective_deadline| {
            paused_namespace = Some(subagent_namespace_snapshot(root.path()));
            while Instant::now() < effective_deadline {
                std::thread::yield_now();
            }
            Ok(())
        },
    )
    .expect_err("migration must not receive a renewed default budget");
    assert!(error.contains("deadline elapsed"), "{error}");
    let paused_namespace = paused_namespace
        .unwrap_or_else(|| panic!("records setup did not reach its phase hook: {error}"));
    assert_eq!(
        subagent_namespace_snapshot(root.path()),
        paused_namespace,
        "expired second phase mutated the records namespace"
    );
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        subagent_namespace_snapshot(root.path()),
        paused_namespace,
        "expired migration mutated the records namespace later"
    );

    ensure_records_directory_until(root.path(), Some(Instant::now() + Duration::from_secs(2)))
        .expect("fresh explicit deadline completes authorization and migration");
}

#[cfg(any(unix, windows))]
#[test]
fn delegation_lock_setup_deadline_guards_every_namespace_mutation_and_retries() {
    #[derive(Clone, Copy)]
    enum Boundary {
        LockParent,
        AnchorParent,
        VisibleLock,
        AnchorLink,
    }

    for boundary in [
        Boundary::LockParent,
        Boundary::AnchorParent,
        Boundary::VisibleLock,
        Boundary::AnchorLink,
    ] {
        let root = tempfile::tempdir().expect("root");
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root.path())
            .status()
            .expect("initialize git repository");
        assert!(status.success());
        let nib = root.path().join(".nib");
        std::fs::create_dir(&nib).expect("nib fixture");
        let lock_parent = nib.join("deadline-locks");
        let lock_path = match boundary {
            Boundary::LockParent => lock_parent.join("setup.lock"),
            Boundary::AnchorParent | Boundary::VisibleLock | Boundary::AnchorLink => {
                nib.join("setup.lock")
            }
        };
        let anchor_path =
            crate::daemons::state::daemon_lock_anchor_path(&lock_path).expect("anchor path");
        let target = match boundary {
            Boundary::LockParent => lock_parent.clone(),
            Boundary::AnchorParent => anchor_path.parent().expect("anchor parent").to_path_buf(),
            Boundary::VisibleLock => lock_path.clone(),
            Boundary::AnchorLink => anchor_path.clone(),
        };
        let deadline = Instant::now() + expiry_checkpoint_timeout();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
        let worker_nib = nib.clone();
        let worker_lock = lock_path.clone();
        let worker_target = target.clone();
        let worker = std::thread::spawn(move || {
            let mut paused = false;
            with_delegation_lock_in_deadline_with_setup_hook(
                &worker_lock,
                &worker_nib,
                deadline,
                None,
                |_, _| -> Result<(), String> {
                    panic!("expired setup must not enter protected operation")
                },
                |path| {
                    if !paused && path == worker_target {
                        paused = true;
                        ready_tx.send(()).expect("signal setup boundary");
                        resume_rx.recv().expect("resume setup boundary");
                    }
                    Ok(())
                },
            )
            .expect_err("expired setup boundary must fail closed")
        });
        ready_rx
            .recv_timeout(expiry_checkpoint_wait())
            .expect("delegation setup reached mutation boundary");
        let paused = subagent_namespace_snapshot(root.path());
        while Instant::now() < deadline {
            std::thread::yield_now();
        }
        resume_tx.send(()).expect("release setup boundary");
        let error = worker.join().expect("join setup worker");
        assert!(error.contains("deadline elapsed"), "{error}");
        assert_eq!(
            subagent_namespace_snapshot(root.path()),
            paused,
            "expired setup mutated its namespace after the final boundary"
        );

        let mut entered = false;
        with_bounded_delegation_lock_in_until(
            &lock_path,
            &nib,
            Instant::now() + Duration::from_secs(2),
            |_, _| {
                entered = true;
                Ok(())
            },
        )
        .expect("fresh deadline repairs and acquires exact setup artifacts");
        assert!(entered);
        let visible = open_repository_merge_lock(&lock_path).expect("visible lock");
        let anchor = open_repository_merge_lock(&anchor_path).expect("anchor lock");
        assert_eq!(
            repository_lock_identity(&visible, &lock_path).expect("visible identity"),
            repository_lock_identity(&anchor, &anchor_path).expect("anchor identity")
        );
    }
}

#[cfg(any(unix, windows))]
#[test]
fn expired_owner_lease_removal_preserves_exact_artifacts() {
    let root = tempfile::tempdir().expect("root");
    let lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
    let visible_path = lease.visible_path.clone();
    let anchor_path = lease.anchor_path.clone();

    let error = lease
        .remove_until(Some(Instant::now() - Duration::from_millis(1)))
        .expect_err("expired cleanup must preserve the owner lease");

    assert!(error.contains("delegation state lock deadline elapsed"));
    assert!(
        visible_path.is_file(),
        "visible owner lease was removed late"
    );
    assert!(anchor_path.is_file(), "owner lease anchor was removed late");
}

#[cfg(any(unix, windows))]
#[test]
fn delegation_lock_rejects_success_when_its_operation_crosses_the_deadline() {
    let root = tempfile::tempdir().expect("root");
    let records = ensure_records_directory(root.path()).expect("records");
    let lock_path =
        record_lock_path(root.path(), "sub-post-operation-deadline").expect("record lock");

    let error = with_bounded_delegation_lock_in_until(
        &lock_path,
        &records,
        Instant::now() + Duration::from_millis(40),
        |_, _| {
            std::thread::sleep(Duration::from_millis(75));
            Ok(())
        },
    )
    .expect_err("post-operation deadline expiry must reject success");

    assert!(
        error.contains("delegation state lock deadline elapsed"),
        "{error}"
    );
}

#[cfg(any(unix, windows))]
#[test]
fn owner_creation_stops_before_anchor_publication_when_its_deadline_expires() {
    let operation_timeout = Duration::from_secs(2);
    let expiry_delay = operation_timeout + Duration::from_millis(50);
    let boundary_wait = Duration::from_secs(10);
    let root = tempfile::tempdir().expect("root");
    let owner_directory = owner_lease_directory(root.path());
    let anchor_directory = root.path().join(".nib");
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let project_root = root.path().to_path_buf();
    let owner_plan = SubagentOwnerLease::plan();
    let worker_owner_directory = owner_directory.clone();
    let worker_anchor_directory = anchor_directory.clone();
    let worker = std::thread::spawn(move || {
        let mut paused = false;
        SubagentOwnerLease::create_with_timeout_and_guard(
            &project_root,
            operation_timeout,
            &owner_plan,
            || {
                let visible_count = std::fs::read_dir(&worker_owner_directory)
                    .map(|entries| entries.filter_map(Result::ok).count())
                    .unwrap_or(0);
                let anchor_count = std::fs::read_dir(&worker_anchor_directory)
                    .map(|entries| {
                        entries
                            .filter_map(Result::ok)
                            .filter(|entry| {
                                entry
                                    .file_name()
                                    .as_encoded_bytes()
                                    .starts_with(OWNER_LEASE_ANCHOR_PREFIX.as_bytes())
                            })
                            .count()
                    })
                    .unwrap_or(0);
                if !paused && visible_count == 1 && anchor_count == 0 {
                    paused = true;
                    ready_tx.send(()).expect("publish owner creation pause");
                    resume_rx.recv().expect("resume owner creation");
                }
                Ok(())
            },
        )
    });

    ready_rx
        .recv_timeout(boundary_wait)
        .expect("owner creation reached its anchor publication boundary");
    let before_expiry = subagent_namespace_snapshot(&owner_directory);
    assert_eq!(
        before_expiry.len(),
        1,
        "one recoverable visible lease exists"
    );
    assert!(
        std::fs::read_dir(&anchor_directory)
            .expect("anchor namespace")
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .as_encoded_bytes()
                .starts_with(OWNER_LEASE_ANCHOR_PREFIX.as_bytes())),
        "owner anchor was not published before the pause"
    );
    std::thread::sleep(expiry_delay);
    resume_tx.send(()).expect("resume expired owner creation");
    let error = worker
        .join()
        .expect("owner creation worker")
        .expect_err("expired owner creation must fail closed");
    assert!(
        error.contains("timed out acquiring delegation state lock"),
        "{error}"
    );
    assert_eq!(subagent_namespace_snapshot(&owner_directory), before_expiry);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(subagent_namespace_snapshot(&owner_directory), before_expiry);
}

#[cfg(any(unix, windows))]
#[test]
fn sweep_pair_and_both_half_deletions_stop_at_expired_quarantines() {
    sweep_owner_quarantine_expiry_fixture(None);
    sweep_owner_quarantine_expiry_fixture(Some(true));
    sweep_owner_quarantine_expiry_fixture(Some(false));
}

#[cfg(any(unix, windows))]
#[test]
fn precommit_record_cleanup_stops_at_its_quarantine_after_expiry() {
    let root = tempfile::tempdir().expect("root");
    let record = record_fixture(root.path(), "sub-precommit-expiry", "running");
    write_subagent_record(root.path(), &record).expect("initial record");
    let path = record_path(root.path(), &record.id).expect("record path");
    let expected = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("expected record");
    let directory =
        crate::daemons::state::StableDirectory::open(path.parent().expect("record parent"))
            .expect("record directory");
    let quarantine = directory
        .deterministic_artifact_path(&path, ".nib-subagent-precommit-delete-", ".quarantine")
        .expect("precommit quarantine");
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let project_root = root.path().to_path_buf();
    let worker_path = path.clone();
    let worker_quarantine = quarantine.clone();
    let worker_record = record.clone();
    let worker_expected = expected.try_clone().expect("clone expected record");
    let operation_timeout = expiry_checkpoint_timeout();
    let worker = std::thread::spawn(move || {
        cleanup_precommit_record_with_timeout_and_hooks(
            &project_root,
            &worker_record,
            Some(&worker_expected),
            operation_timeout,
            || Ok(()),
            || {
                if worker_quarantine.exists() && !worker_path.exists() {
                    ready_tx
                        .send(())
                        .expect("publish precommit quarantine pause");
                    resume_rx.recv().expect("resume precommit cleanup");
                }
                Ok(())
            },
        )
    });

    ready_rx
        .recv_timeout(expiry_checkpoint_wait())
        .expect("precommit cleanup reached its final deletion boundary");
    let quarantined = std::fs::read(&quarantine).expect("precommit quarantine bytes");
    std::thread::sleep(operation_timeout + Duration::from_millis(200));
    resume_tx
        .send(())
        .expect("resume expired precommit cleanup");
    let error = worker
        .join()
        .expect("precommit cleanup worker")
        .expect_err("expired precommit cleanup must fail closed");
    assert!(
        error.contains("timed out acquiring delegation state lock"),
        "{error}"
    );
    assert_eq!(
        std::fs::read(&quarantine).expect("retained precommit quarantine"),
        quarantined
    );
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        std::fs::read(&quarantine).expect("later precommit quarantine"),
        quarantined
    );
    cleanup_precommit_record_with_timeout_and_hooks(
        root.path(),
        &record,
        Some(&expected),
        Duration::from_secs(2),
        || Ok(()),
        || Ok(()),
    )
    .expect("fresh precommit cleanup finishes retained quarantine deletion");
    assert!(!path.exists(), "fresh cleanup leaves no canonical record");
    assert!(
        !quarantine.exists(),
        "fresh cleanup removes the retained precommit quarantine"
    );
}

#[cfg(any(unix, windows))]
#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn initial_and_revision_publications_stop_before_namespace_mutation_after_expiry() {
    let operation_timeout = expiry_checkpoint_timeout();
    let expiry_delay = operation_timeout + Duration::from_millis(50);
    let boundary_wait = expiry_checkpoint_wait();
    let root = tempfile::tempdir().expect("root");
    let initial = record_fixture(root.path(), "sub-initial-publication-expiry", "running");
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let project_root = root.path().to_path_buf();
    let worker_initial = initial.clone();
    let worker = std::thread::spawn(move || {
        let mut paused = false;
        write_subagent_record_with_refresh_hooks_and_timeout(
            &project_root,
            &worker_initial,
            operation_timeout,
            || {
                if !paused {
                    paused = true;
                    ready_tx.send(()).expect("publish initial record pause");
                    resume_rx.recv().expect("resume initial publication");
                }
                Ok(())
            },
            || Ok(()),
        )
    });
    ready_rx
        .recv_timeout(boundary_wait)
        .expect("initial publication reached atomic namespace boundary");
    let records = records_dir(root.path());
    let before_initial = subagent_namespace_snapshot(&records);
    std::thread::sleep(expiry_delay);
    resume_tx
        .send(())
        .expect("resume expired initial publication");
    let error = match worker.join().expect("initial publication worker") {
        Ok(_) => panic!("expired initial publication must fail closed"),
        Err(error) => error,
    };
    assert!(
        error
            .message
            .contains("timed out acquiring delegation state lock"),
        "{}",
        error.message
    );
    assert_eq!(subagent_namespace_snapshot(&records), before_initial);
    assert!(!record_path(root.path(), &initial.id)
        .expect("initial path")
        .exists());

    let mut revised = record_fixture(root.path(), "sub-revision-publication-expiry", "running");
    write_subagent_record(root.path(), &revised).expect("revision base record");
    let revised_path = record_path(root.path(), &revised.id).expect("revision path");
    let expected = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&revised_path)
        .expect("revision identity");
    revised.status = "failed".to_string();
    revised.error = Some("must not publish".to_string());
    revised.updated_at = Utc::now();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let project_root = root.path().to_path_buf();
    let worker = std::thread::spawn(move || {
        let mut expected = expected;
        let mut paused = false;
        persist_subagent_record_revision_with_refresh_hooks_and_timeout(
            &project_root,
            &revised,
            &mut expected,
            operation_timeout,
            || {
                if !paused {
                    paused = true;
                    ready_tx.send(()).expect("publish revision record pause");
                    resume_rx.recv().expect("resume revision publication");
                }
                Ok(())
            },
            || Ok(()),
        )
    });
    ready_rx
        .recv_timeout(boundary_wait)
        .expect("revision publication reached atomic namespace boundary");
    let before_revision = subagent_namespace_snapshot(&records);
    std::thread::sleep(expiry_delay);
    resume_tx
        .send(())
        .expect("resume expired revision publication");
    let error = worker
        .join()
        .expect("revision publication worker")
        .expect_err("expired revision publication must fail closed");
    assert!(
        error.contains("timed out acquiring delegation state lock"),
        "{error}"
    );
    assert_eq!(subagent_namespace_snapshot(&records), before_revision);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(subagent_namespace_snapshot(&records), before_revision);
}

#[cfg(any(unix, windows))]
#[test]
fn paired_owner_cleanup_stops_at_its_quarantine_when_deadline_expires() {
    owner_cleanup_quarantine_expiry_fixture(None);
}

#[cfg(any(unix, windows))]
#[test]
fn half_owner_cleanup_stops_at_its_quarantine_when_deadline_expires() {
    owner_cleanup_quarantine_expiry_fixture(Some(true));
    owner_cleanup_quarantine_expiry_fixture(Some(false));
}

#[cfg(any(unix, windows))]
#[test]
fn owner_cleanup_rejects_ambiguous_and_live_retained_quarantines() {
    let root = tempfile::tempdir().expect("root");
    let lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
    let execution_generation = lease.execution_generation;
    let lease_id = lease.lease_id.clone();
    let visible = lease.visible_path.clone();
    let anchor = lease.anchor_path.clone();
    let visible_directory =
        crate::daemons::state::StableDirectory::open(visible.parent().expect("visible parent"))
            .expect("visible directory");
    let quarantine = visible_directory
        .deterministic_artifact_path(
            &visible,
            ".nib-subagent-owner-visible-delete-",
            ".quarantine",
        )
        .expect("visible quarantine");
    std::fs::rename(&visible, &quarantine).expect("retain live visible quarantine");

    let error = remove_persisted_owner_lease_until(
        root.path(),
        execution_generation,
        &lease_id,
        Some(Instant::now() + Duration::from_secs(1)),
    )
    .expect_err("a live quarantined owner must remain unresolved");
    assert!(error.contains("still live"), "{error}");
    assert!(quarantine.exists());
    assert!(anchor.exists());

    drop(lease);
    std::fs::write(&visible, b"replacement").expect("ambiguous visible replacement");
    let error = remove_persisted_owner_lease_until(
        root.path(),
        execution_generation,
        &lease_id,
        Some(Instant::now() + Duration::from_secs(1)),
    )
    .expect_err("a mismatched canonical/quarantine pair must fail closed");
    assert!(error.contains("different identities"), "{error}");
    assert_eq!(
        std::fs::read(&visible).expect("replacement visible"),
        b"replacement"
    );
    assert!(quarantine.exists());
    assert!(anchor.exists());
}

#[cfg(any(unix, windows))]
#[test]
fn legacy_lock_migration_retries_a_retained_deletion_quarantine() {
    let root = tempfile::tempdir().expect("root");
    let records = ensure_records_directory(root.path()).expect("records");
    let legacy_locks = records.join(".locks");
    std::fs::create_dir(&legacy_locks).expect("legacy locks");
    let id = "sub-legacy-quarantine-retry";
    let visible = legacy_locks.join(format!("{id}.lock"));
    let anchor =
        crate::daemons::state::daemon_lock_anchor_path(&visible).expect("legacy anchor path");
    std::fs::write(&visible, b"legacy-lock").expect("legacy visible");
    std::fs::hard_link(&visible, &anchor).expect("legacy anchor");
    let visible_directory =
        crate::daemons::state::StableDirectory::open(&legacy_locks).expect("legacy directory");
    let anchor_directory =
        crate::daemons::state::StableDirectory::open(&records).expect("record directory");
    let quarantine = visible_directory
        .deterministic_artifact_path(&visible, ".nib-legacy-lock-delete-", ".quarantine")
        .expect("legacy quarantine");
    let deadline = Instant::now() + expiry_checkpoint_timeout();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let worker_visible = visible.clone();
    let worker_quarantine = quarantine.clone();
    let worker = std::thread::spawn(move || {
        let mut paused = false;
        crate::daemons::state::cleanup_legacy_lock_pair_with_guard(
            &visible_directory,
            &worker_visible,
            &anchor_directory,
            &anchor,
            || {
                if !paused && worker_quarantine.exists() && !worker_visible.exists() {
                    paused = true;
                    ready_tx.send(()).expect("publish legacy quarantine pause");
                    resume_rx.recv().expect("resume legacy cleanup");
                }
                ensure_subagent_reconciliation_deadline(Some(deadline))
            },
        )
    });
    ready_rx
        .recv_timeout(expiry_checkpoint_wait())
        .expect("legacy cleanup reached its quarantine boundary");
    std::thread::sleep(
        deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(200),
    );
    resume_tx.send(()).expect("resume expired legacy cleanup");
    let error = worker
        .join()
        .expect("legacy cleanup worker")
        .expect_err("expired legacy cleanup must retain quarantine");
    assert!(error.contains("deadline elapsed"), "{error}");
    assert!(quarantine.exists());

    confirm_no_legacy_subagent_processes(root.path())
        .expect("fresh attestation finishes retained legacy cleanup");
    assert!(!visible.exists());
    assert!(!quarantine.exists());
    assert!(!crate::daemons::state::daemon_lock_anchor_path(&visible)
        .expect("legacy anchor")
        .exists());
}

#[cfg(any(unix, windows))]
#[test]
fn record_and_owner_namespace_locks_have_absolute_deadlines() {
    let root = tempfile::tempdir().expect("root");
    let records = ensure_records_directory(root.path()).expect("records");
    let record_lock = record_lock_path(root.path(), "sub-held-lock").expect("record lock");
    let record_anchor =
        crate::daemons::state::daemon_lock_anchor_path(&record_lock).expect("record lock anchor");
    crate::fs_security::ensure_directory_without_symlinks(
        record_anchor.parent().expect("record anchor parent"),
    )
    .expect("record anchor directory");
    let held_record =
        open_repository_merge_lock_anchor(&record_lock, &record_anchor).expect("record lock pair");
    held_record.try_lock().expect("hold record stripe");
    let started = Instant::now();
    let error = with_bounded_delegation_lock_in(
        &record_lock,
        &records,
        Duration::from_millis(75),
        |_, _| -> Result<(), String> { panic!("held record stripe must not run its operation") },
    )
    .expect_err("held record stripe must time out");
    assert!(error.contains("timed out acquiring delegation state lock"));
    assert!(started.elapsed() < Duration::from_secs(1));
    drop(held_record);

    let owner_lock = owner_lease_namespace_lock_path(root.path());
    let owner_anchor =
        crate::daemons::state::daemon_lock_anchor_path(&owner_lock).expect("owner lock anchor");
    crate::fs_security::ensure_directory_without_symlinks(
        owner_anchor.parent().expect("owner anchor parent"),
    )
    .expect("owner anchor directory");
    let held_owner =
        open_repository_merge_lock_anchor(&owner_lock, &owner_anchor).expect("owner lock pair");
    held_owner.try_lock().expect("hold owner namespace");
    let started = Instant::now();
    let error = with_bounded_delegation_lock_in(
        &owner_lock,
        &root.path().join(".nib"),
        Duration::from_millis(75),
        |_, _| -> Result<(), String> { panic!("held owner namespace must not run its operation") },
    )
    .expect_err("held owner namespace must time out");
    assert!(error.contains("timed out acquiring delegation state lock"));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn repository_merge_lock_replacement_child_process() {
    let Some(project_root) = std::env::var_os(MERGE_LOCK_CHILD_PROJECT_ROOT) else {
        return;
    };
    let expectation = std::env::var(MERGE_LOCK_CHILD_EXPECTATION).expect("child lock expectation");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("child runtime");
    let error = runtime
        .block_on(RepositoryMergeLock::acquire_with_timeout(
            Path::new(&project_root),
            Duration::from_secs(2),
        ))
        .expect_err("replacement must not create a second repository lock domain");
    match expectation.as_str() {
        "timeout" => assert!(
            error.contains("timed out acquiring repository merge lock"),
            "{error}"
        ),
        "identity" => assert!(
            error.contains("persistent anchor have different identities"),
            "{error}"
        ),
        "offline" => assert!(error.contains("confirm-no-legacy-processes"), "{error}"),
        value => panic!("unsupported child expectation: {value}"),
    }
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn persistent_anchor_prevents_replaced_repository_lock_domains() {
    let root = tempfile::tempdir().expect("root");
    let held = RepositoryMergeLock::acquire_with_timeout(root.path(), Duration::from_secs(10))
        .await
        .expect("held persistent repository merge lock");

    let run_child = |expectation: &str| {
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tools::delegation::tests::test_part_3::repository_merge_lock_replacement_child_process",
                "--nocapture",
            ])
            .env(MERGE_LOCK_CHILD_PROJECT_ROOT, root.path())
            .env(MERGE_LOCK_CHILD_EXPECTATION, expectation)
            .output()
            .expect("run repository merge lock child process");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    };

    run_child("timeout");

    let records_path = records_dir(root.path());
    let lock_path = records_path.join(".merge.lock");
    let displaced_lock = records_path.join(".merge.lock.displaced");
    std::fs::rename(&lock_path, &displaced_lock).expect("displace visible lock path");
    std::fs::write(&lock_path, b"replacement").expect("replace visible lock path");
    run_child("identity");

    std::fs::remove_file(&lock_path).expect("remove replacement lock path");
    std::fs::rename(&displaced_lock, &lock_path).expect("restore anchored lock path");

    let displaced_records = root.path().join(".nib/subagents.displaced");
    #[cfg(unix)]
    {
        std::fs::rename(&records_path, &displaced_records)
            .expect("displace subagent records directory");
        std::fs::create_dir(&records_path).expect("replace subagent records directory");
        run_child("offline");
        std::fs::remove_dir_all(&records_path).expect("remove replacement records directory");
        std::fs::rename(&displaced_records, &records_path)
            .expect("restore subagent records directory");
    }
    #[cfg(windows)]
    {
        std::fs::rename(&records_path, &displaced_records)
            .expect_err("the locked Windows record inode must pin its parent directory");
        run_child("timeout");
    }

    drop(held);
    // The timeout covers records-directory and lock-anchor validation as well as
    // acquisition. Leave enough room for those bounded filesystem checks on a
    // loaded CI host after the child-process probes exit.
    RepositoryMergeLock::acquire_with_timeout(root.path(), Duration::from_secs(10))
        .await
        .expect("restored persistent lock identity remains usable");
}

#[cfg(unix)]
#[test]
fn repository_merge_lock_rejects_path_replacement_after_open() {
    let root = tempfile::tempdir().expect("root");
    let directory = ensure_records_directory(root.path()).expect("record directory");
    let path = directory.join(".merge.lock");
    let opened = open_repository_merge_lock(&path).expect("opened lock");
    let displaced = directory.join(".merge.lock.displaced");
    std::fs::rename(&path, &displaced).expect("displace opened lock");
    std::fs::write(&path, b"replacement").expect("replacement lock");

    let error = validate_repository_lock_path(&opened, &path)
        .expect_err("replacement must change identity");

    assert!(error.contains("changed while it was acquired"));
}

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
async fn merge_rejects_child_edit_after_immutable_verification_snapshot() {
    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .expect("git starts");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let root = tempfile::tempdir().expect("root");
    git(root.path(), &["init", "-q"]);
    git(
        root.path(),
        &["config", "user.email", "nib@example.invalid"],
    );
    git(root.path(), &["config", "user.name", "nib"]);
    std::fs::write(root.path().join("README.md"), "fixture\n").expect("fixture");
    git(root.path(), &["add", "README.md"]);
    git(root.path(), &["commit", "-qm", "initial"]);
    let worktree = crate::sandbox::worktree::Worktree::create(root.path(), "sub-post-verify")
        .expect("worktree");
    std::fs::write(worktree.path.join("result.txt"), "verified\n").expect("result");
    let record = SubagentRecord {
        id: "sub-post-verify".to_string(),
        parent_session_id: Some("parent".to_string()),
        child_session_id: "child".to_string(),
        prompt: "fixture".to_string(),
        status: "completed".to_string(),
        execution_generation: None,
        owner_lease: None,
        worktree_path: worktree.path.clone(),
        branch: worktree.branch.clone(),
        branch_oid: Some(worktree.branch_oid.clone()),
        result: Some(json!({"summary": "done"})),
        error: None,
        verification: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    write_subagent_record(root.path(), &record).expect("record");
    let target = prepare_subagent_verification_target(root.path(), &record.id, None)
        .await
        .expect("immutable verification target");
    let verified_commit = target.snapshot_commit.clone();
    assert_eq!(
        get_subagent_record(root.path(), &record.id)
            .expect("snapshot ownership record")
            .branch_oid
            .as_deref(),
        Some(verified_commit.as_str())
    );
    std::fs::write(
        worktree.path.join("result.txt"),
        "edited after verification\n",
    )
    .expect("post-verification edit");
    let evidence = VerificationEvidence {
        tool_name: "run_terminal".to_string(),
        command: "true".to_string(),
        worktree_path: target.worktree_path.clone(),
        success: true,
        output: Some(json!({
            "command": "true",
            "exit_code": 0,
            "cwd": target.worktree_path,
            "provider": "internal",
        })),
        error: None,
        approval_granted: true,
        approval_source: Some("user".to_string()),
        duration_seconds: 0.01,
        configured_provider: "internal".to_string(),
        sandbox_profile: "internal".to_string(),
        boundaries: BoundaryConfig::default(),
        session_id: Some("parent".to_string()),
        snapshot_commit: Some(verified_commit.clone()),
        executed_at: Utc::now(),
    };

    let error = merge_verified_subagent_worktree(
        &json!({
            "subagent_id": record.id,
            "verification_command": "true",
        }),
        root.path(),
        evidence,
        None,
    )
    .await
    .expect_err("post-verification edit must fail closed");

    assert!(error.contains("mergeable changes after verification"));
    assert!(error.contains("fresh verification"));
    assert!(!root.path().join("result.txt").exists());
    let snapshot = std::process::Command::new("git")
        .current_dir(&worktree.path)
        .args(["show", &format!("{verified_commit}:result.txt")])
        .output()
        .expect("inspect snapshot");
    assert!(snapshot.status.success());
    assert_eq!(String::from_utf8_lossy(&snapshot.stdout), "verified\n");
    assert_eq!(
        get_subagent_record(root.path(), "sub-post-verify")
            .expect("failed record")
            .status,
        "verification_failed"
    );
}

#[test]
fn sync_spawn_compensation_does_not_short_circuit_after_record_failure() {
    let calls = std::cell::RefCell::new(Vec::new());
    let errors = collect_spawn_compensation_sync(
        || {
            calls.borrow_mut().push("record");
            Err("record sentinel".to_string())
        },
        || {
            calls.borrow_mut().push("worktree");
            Err("worktree sentinel".to_string())
        },
        |action| {
            assert_eq!(action, OwnerLeaseCompensation::ReleaseForReconciliation);
            calls.borrow_mut().push("lease");
            Err("lease sentinel".to_string())
        },
    );

    assert_eq!(*calls.borrow(), ["record", "worktree", "lease"]);
    assert_eq!(errors.len(), 3);
    assert!(errors[0].contains("record sentinel"));
    assert!(errors[1].contains("worktree sentinel"));
    assert!(errors[2].contains("lease sentinel"));
}

#[tokio::test]
async fn async_spawn_compensation_does_not_short_circuit_after_record_failure() {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let record_calls = calls.clone();
    let worktree_calls = calls.clone();
    let lease_calls = calls.clone();
    let errors = collect_spawn_compensation_async(
        move || {
            record_calls.lock().expect("calls").push("record");
            Err("record sentinel".to_string())
        },
        async move {
            worktree_calls.lock().expect("calls").push("worktree");
            Err("worktree sentinel".to_string())
        },
        move |action| {
            assert_eq!(action, OwnerLeaseCompensation::ReleaseForReconciliation);
            lease_calls.lock().expect("calls").push("lease");
            Err("lease sentinel".to_string())
        },
    )
    .await;

    assert_eq!(
        *calls.lock().expect("calls"),
        ["record", "lease", "worktree"]
    );
    assert_eq!(errors.len(), 3);
    assert!(errors[0].contains("record sentinel"));
    assert!(errors[1].contains("worktree sentinel"));
    assert!(errors[2].contains("lease sentinel"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_async_spawn_compensation_still_runs_every_cleanup() {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let record_calls = calls.clone();
    let lease_calls = calls.clone();
    let worktree_calls = calls.clone();
    let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let (finished_sender, finished_receiver) = tokio::sync::oneshot::channel();
    let worktree_cleanup = async move {
        tokio::task::spawn_blocking(move || {
            worktree_calls.lock().expect("calls").push("worktree");
            let _ = started_sender.send(());
            let _ = release_receiver.recv();
            let _ = finished_sender.send(());
            Ok(())
        })
        .await
        .map_err(|error| error.to_string())?
    };
    let compensation = tokio::spawn(collect_spawn_compensation_async(
        move || {
            record_calls.lock().expect("calls").push("record");
            Ok(())
        },
        worktree_cleanup,
        move |action| {
            assert_eq!(action, OwnerLeaseCompensation::Remove);
            lease_calls.lock().expect("calls").push("lease");
            Ok(())
        },
    ));
    started_receiver.await.expect("worktree cleanup starts");

    compensation.abort();
    assert!(compensation
        .await
        .expect_err("compensation task cancellation")
        .is_cancelled());
    release_sender.send(()).expect("release worktree cleanup");
    finished_receiver.await.expect("worktree cleanup finishes");

    let calls = calls.lock().expect("calls").clone();
    assert_eq!(calls, ["record", "lease", "worktree"]);
}

#[test]
fn failed_record_compensation_preserves_an_unlockable_owner_lease() {
    let root = tempfile::tempdir().expect("root");
    let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
    let execution_generation = owner_lease.execution_generation;
    let lease_id = owner_lease.lease_id.clone();

    let errors = collect_spawn_compensation_sync(
        || Err("record remains durable".to_string()),
        || Ok(()),
        |action| compensate_owner_lease(owner_lease, action),
    );

    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("record remains durable"));
    let OwnerLeaseProbe::Acquired(reconciler) =
        SubagentOwnerLease::probe(root.path(), execution_generation, &lease_id)
            .expect("preserved lease remains probeable")
    else {
        panic!("released compensation lease must be unlockable");
    };
    reconciler.remove().expect("cleanup preserved lease");
}

#[test]
fn precommit_cleanup_preserves_a_moved_owned_branch() {
    fn git(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .expect("git starts");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    let root = tempfile::tempdir().expect("root");
    git(root.path(), &["init", "-q"]);
    git(
        root.path(),
        &["config", "user.email", "nib@example.invalid"],
    );
    git(root.path(), &["config", "user.name", "nib"]);
    std::fs::write(root.path().join("README.md"), "fixture\n").expect("fixture");
    git(root.path(), &["add", "README.md"]);
    git(root.path(), &["commit", "-qm", "initial"]);
    let worktree = crate::sandbox::worktree::Worktree::create(root.path(), "sub-moved-branch")
        .expect("worktree");
    std::fs::write(root.path().join("parent.txt"), "advanced\n").expect("parent change");
    git(root.path(), &["add", "parent.txt"]);
    git(root.path(), &["commit", "-qm", "advance parent"]);
    let moved_oid = git(root.path(), &["rev-parse", "HEAD"]);
    let reference = format!("refs/heads/{}", worktree.branch);
    git(
        root.path(),
        &["update-ref", reference.as_str(), moved_oid.as_str()],
    );

    let error = cleanup_precommit_worktree_sync(root.path(), &worktree)
        .expect_err("moved branch must be preserved");
    assert!(error.contains("identity changed"), "{error}");
    assert!(error.contains("preserving"), "{error}");
    assert!(!worktree.path.exists(), "owned worktree path was removed");
    assert_eq!(
        git(root.path(), &["show-ref", "--hash", "--verify", &reference]),
        moved_oid
    );
}

#[cfg(windows)]
#[tokio::test]
async fn delegation_accepts_a_dos_short_project_root_and_persists_canonical_ownership() {
    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .expect("git starts");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let root = tempfile::tempdir().expect("delegation DOS-alias repository");
    let _spawn_timeout = SpawnPreparationTimeoutGuard::set(Duration::from_secs(30));
    let _timeout = SubagentCancellationTimeoutGuard::set(Duration::from_secs(30));
    git(root.path(), &["init", "-q"]);
    git(
        root.path(),
        &["config", "user.email", "nib@example.invalid"],
    );
    git(root.path(), &["config", "user.name", "nib"]);
    std::fs::write(root.path().join(".gitignore"), ".nib/\n").expect("gitignore");
    std::fs::write(root.path().join("README.md"), "fixture\n").expect("fixture");
    git(root.path(), &["add", ".gitignore", "README.md"]);
    git(root.path(), &["commit", "-qm", "initial"]);
    let mut config = crate::config::NibConfig::default();
    config.execution.plan_mode = false;
    crate::config::save_nib_config_full(root.path(), &mut config).expect("save config");

    let canonical_root = root.path().canonicalize().expect("canonical repository");
    let short_root = crate::fs_security::windows_dos_short_path_for_test(&canonical_root)
        .expect("DOS short project root");
    if short_root == crate::fs_security::path_without_windows_verbatim_prefix(&canonical_root) {
        return;
    }

    let started = spawn_subagent(
        &json!({"prompt": "Return a bounded fixture response.", "max_steps": 1}),
        &short_root,
    )
    .expect("delegate through DOS short root");
    let id = started["subagent_id"].as_str().expect("subagent id");
    let record = get_subagent_record(&short_root, id).expect("delegation record");
    assert!(record.worktree_path.starts_with(&canonical_root));
    assert!(!record.worktree_path.starts_with(&short_root));

    match resolve_subagent_cancellation_async(&short_root, id).await {
        CancelSubagentResolution::Cancelled { .. } | CancelSubagentResolution::Terminal { .. } => {}
        CancelSubagentResolution::Unresolved { error, .. } => {
            panic!("DOS-alias delegation cancellation was unresolved: {error}")
        }
    }
    crate::sandbox::worktree::Worktree::remove(&short_root, id)
        .expect("remove delegated worktree through DOS short root");
}
