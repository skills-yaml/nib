use super::*;

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn live_revision_and_intent_quarantine_block_reconcile_and_preserve_record() {
    let root = tempfile::tempdir().expect("git project");
    initialize_spawn_test_repository(root.path());
    let project_root = canonical_project_root(root.path()).expect("canonical root");
    let id = format!("sub-{}", uuid::Uuid::new_v4());
    let args = json!({"prompt": "serialized revision fixture"});
    let audit_plan = preflight_subagent_audit_target(&args, &project_root).expect("preflight");
    let records =
        ensure_records_directory_capability_until(&project_root, None).expect("authorized records");
    let namespace_plan = audit_plan
        .fallback_namespace_plan_after_records(&id, &records)
        .expect("namespace plan")
        .expect("fallback plan");
    let intent = SpawnPreparationIntent::create(
        &records,
        &id,
        SubagentOwnerLease::plan(),
        crate::sandbox::worktree::Worktree::plan_preparation_authority(&project_root, &id)
            .expect("worktree plan"),
        &id,
        audit_plan.fallback_sessions_dir().expect("sessions"),
        Some(namespace_plan),
        None,
    )
    .expect("planned intent");
    let mut record = record_fixture(&project_root, &id, "completed");
    record.parent_session_id = None;
    let publication = write_spawn_subagent_record_locked(&project_root, &record, &intent.authority)
        .expect("authoritative record publication");
    let record_path = records.path().join(format!("{id}.json"));
    let record_before = std::fs::read(&record_path).expect("record bytes before race");

    let previous = intent
        .directory
        .deterministic_previous_artifact_path(&intent.path, ".nib-subagent-preparation-")
        .expect("revision previous path");
    std::fs::rename(&intent.path, &previous).expect("pause revision after evacuation");
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let list_root = project_root.clone();
    let reader = std::thread::spawn(move || {
        started_tx.send(()).expect("announce reader");
        result_tx
            .send(list_subagents(&list_root))
            .expect("send list result");
    });
    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("reader started");
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        matches!(
            result_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ),
        "reconciler entered while a revision held the lifetime authority"
    );
    assert!(
        previous.exists(),
        "live evacuated revision was not rolled back"
    );
    let encoded = encode_spawn_preparation_intent(&intent.data).expect("intent bytes");
    intent
        .directory
        .restore_exact_previous_artifact_with_guard(&intent.path, &previous, &encoded, || {
            intent.authority.verify()
        })
        .expect("writer restores evacuated revision");

    let quarantine = intent
        .directory
        .deterministic_artifact_path(
            &intent.path,
            ".nib-subagent-preparation-delete-",
            ".quarantine",
        )
        .expect("intent quarantine");
    std::fs::rename(&intent.path, &quarantine).expect("pause cleanup after quarantine");
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        matches!(
            result_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ),
        "reconciler entered while exact cleanup held the lifetime authority"
    );
    intent
        .directory
        .remove_visible_file_if_matches_direct_with_guard(&quarantine, &intent.file, || {
            intent.authority.verify()
        })
        .expect("writer completes exact intent deletion");
    drop(publication);
    drop(intent);

    let listed = result_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("reader completes after cleanup")
        .expect("list after cleanup");
    assert_eq!(listed.len(), 1, "authoritative record remains visible");
    assert_eq!(listed[0]["id"], id);
    assert_eq!(
        std::fs::read(&record_path).expect("record bytes after race"),
        record_before,
        "reconciliation changed the authoritative record"
    );
    assert!(!previous.exists(), "revision artifact was finalized");
    assert!(!quarantine.exists(), "cleanup artifact was finalized");
    reader.join().expect("join reader");
}

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn terminal_retry_finishes_owner_cleanup_and_audits_exactly_once() {
    let root = tempfile::tempdir().expect("root");
    let store = crate::session::SessionStore::for_project(root.path()).expect("session store");
    store.create_session_with_id("parent");
    let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
    let lease_id = owner_lease.lease_id.clone();
    let execution_generation = owner_lease.execution_generation;
    let id = "sub-terminal-cleanup-retry";
    let mut record = record_fixture(root.path(), id, "running");
    attach_execution_ownership(&mut record, &owner_lease);
    install_completed_process_scope(root.path(), id, execution_generation);
    write_subagent_record(root.path(), &record).expect("running record");
    drop(owner_lease);

    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let project_root = root.path().to_path_buf();
    let holder = std::thread::spawn(move || {
        with_bounded_delegation_lock_in(
            &owner_lease_namespace_lock_path(&project_root),
            &project_root.join(".nib"),
            Duration::from_secs(30),
            |_, _| {
                ready_tx.send(()).expect("publish held owner namespace");
                release_rx.recv().expect("release owner namespace");
                Ok(())
            },
        )
        .expect("hold owner namespace")
    });
    ready_rx.recv().expect("owner namespace is held");

    let error = reconcile_subagent_ownership_until(
        root.path(),
        id,
        // Windows legacy-lock migration can consume two seconds before
        // owner cleanup is reached. Keep this deadline on the held
        // cleanup lock, which is the boundary this test exercises.
        Instant::now() + Duration::from_secs(10),
    )
    .expect_err("owner cleanup deadline must fail closed");
    assert!(error.contains("owner lease cleanup"), "{error}");
    let terminal =
        get_subagent_record_unreconciled(root.path(), id).expect("terminal record first");
    assert_eq!(terminal.status, "failed");
    assert!(terminal.result.as_ref().is_some_and(|result| {
        result["ownership_reconciliation"]["reconciliation_id"]
            .as_str()
            .is_some()
    }));
    assert!(owner_lease_path(root.path(), &lease_id)
        .expect("visible owner lease")
        .exists());
    assert!(owner_lease_anchor_path(root.path(), &lease_id)
        .expect("owner anchor")
        .exists());
    assert!(store
        .load_result("parent")
        .expect("audit session")
        .expect("parent session")
        .events
        .is_empty());

    release_tx.send(()).expect("release owner namespace");
    holder.join().expect("owner namespace holder");
    let visible = owner_lease_path(root.path(), &lease_id).expect("visible owner lease");
    let anchor = owner_lease_anchor_path(root.path(), &lease_id).expect("owner anchor");
    let visible_directory = crate::daemons::state::StableDirectory::open(
        visible.parent().expect("visible owner parent"),
    )
    .expect("visible owner directory");
    let visible_quarantine = visible_directory
        .deterministic_artifact_path(
            &visible,
            ".nib-subagent-owner-visible-delete-",
            ".quarantine",
        )
        .expect("visible owner quarantine");
    let (quarantined_tx, quarantined_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let cleanup_root = root.path().to_path_buf();
    let cleanup_lease_id = lease_id.clone();
    let cleanup_visible = visible.clone();
    let cleanup_quarantine = visible_quarantine.clone();
    // Windows owner-lease preparation can take several seconds on a loaded
    // runner. Start a longer deadline before the worker and expire it only
    // after the quarantine checkpoint has been observed.
    let cleanup_deadline = Instant::now()
        + if cfg!(windows) {
            Duration::from_secs(10)
        } else {
            Duration::from_secs(1)
        };
    let cleanup = std::thread::spawn(move || {
        let mut paused = false;
        remove_persisted_owner_lease_until_with_guard(
            &cleanup_root,
            execution_generation,
            &cleanup_lease_id,
            Some(cleanup_deadline),
            || {
                if !paused && cleanup_quarantine.exists() && !cleanup_visible.exists() {
                    paused = true;
                    quarantined_tx
                        .send(())
                        .expect("publish terminal owner quarantine pause");
                    resume_rx.recv().expect("resume terminal owner cleanup");
                }
                Ok(())
            },
        )
    });
    quarantined_rx
        .recv_timeout(if cfg!(windows) {
            Duration::from_secs(10)
        } else {
            Duration::from_secs(3)
        })
        .expect("terminal cleanup reached retained quarantine");
    std::thread::sleep(
        cleanup_deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(200),
    );
    resume_tx.send(()).expect("resume expired terminal cleanup");
    cleanup
        .join()
        .expect("terminal cleanup worker")
        .expect_err("terminal cleanup expiry retains quarantine");
    assert!(!visible.exists());
    assert!(visible_quarantine.exists());
    assert!(anchor.exists());
    assert!(store
        .load_result("parent")
        .expect("audit session")
        .expect("parent session")
        .events
        .is_empty());

    let (audit_lock_tx, audit_lock_rx) = std::sync::mpsc::sync_channel(1);
    let (release_audit_tx, release_audit_rx) = std::sync::mpsc::sync_channel(1);
    let audit_root = root.path().to_path_buf();
    let audit_holder = std::thread::spawn(move || {
        let audit_store =
            crate::session::SessionStore::for_project(&audit_root).expect("audit store");
        audit_store
            .with_session_lock_for_testing("parent", || {
                audit_lock_tx.send(()).expect("publish held audit lock");
                release_audit_rx.recv().expect("release audit lock");
                Ok(())
            })
            .expect("hold audit session lock")
    });
    audit_lock_rx.recv().expect("audit session lock is held");
    let retry_root = root.path().to_path_buf();
    let retry = std::thread::spawn(move || {
        reconcile_subagent_ownership_until(&retry_root, id, Instant::now() + Duration::from_secs(2))
    });
    let cleanup_observation_deadline = Instant::now() + Duration::from_secs(1);
    while (visible.exists() || visible_quarantine.exists() || anchor.exists())
        && Instant::now() < cleanup_observation_deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!visible.exists(), "terminal retry removed visible owner");
    assert!(
        !visible_quarantine.exists(),
        "terminal retry removed retained quarantine before audit publication"
    );
    assert!(!anchor.exists(), "terminal retry removed owner anchor");
    release_audit_tx
        .send(())
        .expect("release audit session lock");
    audit_holder.join().expect("audit session holder");
    let reconciled = retry
        .join()
        .expect("terminal reconciliation worker")
        .expect("terminal retry finalizes ownership");
    assert_eq!(reconciled.status, "failed");
    assert!(!visible.exists());
    assert!(!anchor.exists());

    reconcile_subagent_ownership_until(root.path(), id, Instant::now() + Duration::from_secs(2))
        .expect("idempotent terminal retry");
    let session = store
        .load_result("parent")
        .expect("audit session")
        .expect("parent session");
    let events = session
        .events
        .iter()
        .filter(|event| event.kind == "subagent_execution_reconciled")
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].details["reconciliation_id"],
        terminal.result.expect("terminal result")["ownership_reconciliation"]["reconciliation_id"]
    );
}
