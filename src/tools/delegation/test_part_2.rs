use super::*;

#[test]
fn direct_terminal_cancellation_waits_for_live_owner_cleanup_before_resolution() {
    let root = tempfile::tempdir().expect("root");
    let owner_lease = SubagentOwnerLease::create(root.path()).expect("live owner lease");
    let lease_id = owner_lease.lease_id.clone();
    let execution_generation = owner_lease.execution_generation;
    let id = "sub-direct-terminal-cancel";
    let proof = install_completed_process_scope_with_outcome(
        root.path(),
        id,
        execution_generation,
        "cancelled",
    );
    let mut record =
        terminal_record_fixture(root.path(), id, execution_generation, &lease_id, &proof);
    record.status = "cancelled".to_string();
    record.error = Some("cancelled by manage_subagents".to_string());
    write_subagent_record(root.path(), &record)
        .expect("supervisor persists direct terminal cancellation before monitor cleanup");

    let live_resolution = resolve_subagent_cancellation_until(
        root.path(),
        id,
        Instant::now() + Duration::from_millis(250),
    );
    assert!(
        matches!(
            live_resolution,
            CancelSubagentResolution::Unresolved {
                observed_status: None,
                ..
            }
        ),
        "a live exact owner must keep direct cancellation unresolved: {live_resolution:?}"
    );
    assert!(owner_lease_path(root.path(), &lease_id)
        .expect("visible owner lease")
        .exists());
    assert!(owner_lease_anchor_path(root.path(), &lease_id)
        .expect("owner lease anchor")
        .exists());

    owner_lease
        .release_for_reconciliation()
        .expect("monitor releases exact ownership after supervisor exit");
    let resolved = resolve_subagent_cancellation_until(
        root.path(),
        id,
        Instant::now() + Duration::from_secs(2),
    );
    let CancelSubagentResolution::Cancelled { record } = resolved else {
        panic!("released direct cancellation must reconcile successfully: {resolved:?}");
    };
    assert_eq!(record.status, "cancelled");
    // A clean first cleanup never sets this optional recovery marker;
    // only a retry after a recorded cleanup error writes explicit false.
    assert_ne!(
        record
            .result
            .as_ref()
            .and_then(|result| result.get("cleanup_unverified"))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert!(!owner_lease_path(root.path(), &lease_id)
        .expect("visible owner lease")
        .exists());
    assert!(!owner_lease_anchor_path(root.path(), &lease_id)
        .expect("owner lease anchor")
        .exists());
}

#[test]
fn legacy_precommit_audit_is_adopted_and_upgraded_without_duplication() {
    let root = tempfile::tempdir().expect("root");
    let store = crate::session::SessionStore::for_project(root.path()).expect("session store");
    store.create_session_with_id("parent");
    let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
    let execution_generation = owner_lease.execution_generation;
    let lease_id = owner_lease.lease_id.clone();
    let id = "sub-legacy-precommit-audit";
    let mut record = record_fixture(root.path(), id, "running");
    attach_execution_ownership(&mut record, &owner_lease);
    let proof = install_completed_process_scope(root.path(), id, execution_generation);
    write_subagent_record(root.path(), &record).expect("running record");
    drop(owner_lease);
    let reconciled_at = Utc::now() - chrono::Duration::seconds(1);
    store
        .record_event(
            "parent",
            "subagent_execution_reconciled",
            json!({
                "outcome": "supervisor_result_lost_after_verified_cleanup",
                "subagent_id": id,
                "execution_generation": execution_generation,
                "owner_lease": lease_id,
                "manager_status": Value::Null,
                "terminal_status": "failed",
                "reconciled_at": reconciled_at,
                "cleanup_verified": true,
                "cleanup_scope": "foreground_descendant_process_tree",
                "cleanup_proof": proof,
            }),
        )
        .expect("legacy audit-first event");

    // Audit adoption exercises durable writes and identity checks across several
    // stores. Use the production budget; this is not a latency regression test.
    let terminal = reconcile_subagent_ownership_until(
        root.path(),
        id,
        Instant::now() + SUBAGENT_RECORD_LOCK_TIMEOUT,
    )
    .expect("adopt legacy audit");
    assert_eq!(terminal.status, "failed");
    assert_eq!(
        terminal.result.as_ref().expect("result")["ownership_reconciliation"]["reconciled_at"],
        json!(reconciled_at)
    );
    let retried = reconcile_subagent_ownership_until(
        root.path(),
        id,
        Instant::now() + SUBAGENT_RECORD_LOCK_TIMEOUT,
    )
    .expect("retry adopted legacy audit");
    assert_eq!(retried.status, terminal.status);
    assert_eq!(retried.result, terminal.result);
    let session = store
        .load_result("parent")
        .expect("session")
        .expect("parent");
    let events = session
        .events
        .iter()
        .filter(|event| event.kind == "subagent_execution_reconciled")
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1);
    assert!(events[0].details["reconciliation_id"].is_string());
}

#[test]
fn no_parent_reconciliation_creates_the_pinned_child_audit_session() {
    let root = tempfile::tempdir().expect("root");
    crate::session::SessionStore::for_project(root.path()).expect("session namespace");
    let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
    let execution_generation = owner_lease.execution_generation;
    let id = "sub-no-parent-audit";
    let mut record = record_fixture(root.path(), id, "running");
    record.parent_session_id = None;
    attach_execution_ownership(&mut record, &owner_lease);
    install_completed_process_scope(root.path(), id, execution_generation);
    write_subagent_record(root.path(), &record).expect("running record");
    drop(owner_lease);

    reconcile_subagent_ownership_until(root.path(), id, Instant::now() + Duration::from_secs(2))
        .expect("no-parent reconciliation");
    let store = crate::session::SessionStore::for_project(root.path()).expect("session store");
    let child = store
        .load_result(&record.child_session_id)
        .expect("child audit session")
        .expect("created child audit session");
    assert_eq!(
        child
            .events
            .iter()
            .filter(|event| event.kind == "subagent_execution_reconciled")
            .count(),
        1
    );
}

#[test]
fn merge_wrapped_record_retains_its_pinned_audit_target() {
    let root = tempfile::tempdir().expect("root");
    let store = crate::session::SessionStore::for_project(root.path()).expect("session store");
    let target = SubagentAuditTarget {
        sessions_dir: store.sessions_dir().canonicalize().expect("sessions dir"),
        directory_identity: store
            .persistent_directory_identity()
            .expect("session identity"),
    };
    let mut record = record_fixture(root.path(), "sub-merge-target", MERGE_PENDING_STATUS);
    record.result = Some(json!({
        "subagent_result": {
            "_ownership_audit_target": target.clone(),
        }
    }));

    assert_eq!(
        subagent_audit_target(&record)
            .expect("valid target")
            .expect("nested target"),
        target
    );
}

#[test]
fn stale_generation_guard_cannot_overwrite_reused_running_record() {
    let root = tempfile::tempdir().expect("root");
    let old_lease = SubagentOwnerLease::create(root.path()).expect("old owner lease");
    let old_lease_id = old_lease.lease_id.clone();
    let mut record = record_fixture(root.path(), "sub-generation-fence", "running");
    attach_execution_ownership(&mut record, &old_lease);
    write_subagent_record(root.path(), &record).expect("old generation record");
    let guard = SubagentRunGuard::new(root.path().to_path_buf(), record.id.clone(), old_lease);

    let new_lease = SubagentOwnerLease::create(root.path()).expect("new owner lease");
    attach_execution_ownership(&mut record, &new_lease);
    update_subagent_record(root.path(), &record.id, |current| {
        *current = record.clone();
        Ok(())
    })
    .expect("reused generation record");
    drop(guard);

    let persisted = get_subagent_record_internal(root.path(), &record.id)
        .expect("new live generation remains authoritative");
    assert_eq!(persisted.status, "running");
    assert_eq!(
        persisted.execution_generation,
        Some(new_lease.execution_generation)
    );
    assert_eq!(
        persisted.owner_lease.as_deref(),
        Some(new_lease.lease_id.as_str())
    );
    assert!(!owner_lease_path(root.path(), &old_lease_id)
        .expect("old visible lease")
        .exists());
    update_subagent_record(root.path(), &record.id, |record| {
        record.status = "failed".to_string();
        record.error = Some("test cleanup".to_string());
        record.updated_at = Utc::now();
        Ok(())
    })
    .expect("terminalize new generation fixture");
    new_lease.remove().expect("clean new owner lease");
}

#[cfg(unix)]
#[test]
fn live_owner_is_not_reconciled_through_replaced_lease_paths() {
    let root = tempfile::tempdir().expect("root");
    let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
    let lease_id = owner_lease.lease_id.clone();
    let mut record = record_fixture(root.path(), "sub-live-owner-replacement", "running");
    attach_execution_ownership(&mut record, &owner_lease);
    write_subagent_record(root.path(), &record).expect("running record");

    let visible_directory = owner_lease_directory(root.path());
    let displaced_directory = root.path().join(".nib/subagent-owner-leases.displaced");
    std::fs::rename(&visible_directory, &displaced_directory)
        .expect("displace live lease directory");
    std::fs::create_dir(&visible_directory).expect("replacement lease directory");
    std::fs::copy(
        displaced_directory.join(format!("{lease_id}{OWNER_LEASE_SUFFIX}")),
        visible_directory.join(format!("{lease_id}{OWNER_LEASE_SUFFIX}")),
    )
    .expect("copy unlocked replacement lease");
    let error = get_subagent_record(root.path(), &record.id)
        .expect_err("replacement directory must not form a second lease domain");
    assert!(error.contains("different identities"), "{error}");
    assert_eq!(
        get_subagent_record_unreconciled(root.path(), &record.id)
            .expect("record after directory replacement")
            .status,
        "running"
    );
    std::fs::remove_dir_all(&visible_directory).expect("remove replacement directory");
    std::fs::rename(&displaced_directory, &visible_directory).expect("restore lease directory");

    let visible = owner_lease_path(root.path(), &lease_id).expect("visible lease");
    let displaced_file = visible.with_extension("lease.displaced");
    std::fs::rename(&visible, &displaced_file).expect("displace live lease file");
    std::fs::write(&visible, b"replacement").expect("replacement lease file");
    let error = get_subagent_record(root.path(), &record.id)
        .expect_err("replacement file must not form a second lease domain");
    assert!(error.contains("different identities"), "{error}");
    assert_eq!(
        get_subagent_record_unreconciled(root.path(), &record.id)
            .expect("record after file replacement")
            .status,
        "running"
    );
    std::fs::remove_file(&visible).expect("remove replacement lease file");
    std::fs::rename(&displaced_file, &visible).expect("restore lease file");
    update_subagent_record(root.path(), &record.id, |record| {
        record.status = "failed".to_string();
        record.updated_at = Utc::now();
        Ok(())
    })
    .expect("terminalize replacement fixture");
    owner_lease.remove().expect("clean owner lease");
}

#[test]
fn anchor_only_owner_artifacts_are_reported_while_live_and_removed_when_stale() {
    let root = tempfile::tempdir().expect("root");
    let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
    let lease_id = owner_lease.lease_id.clone();
    let visible_path = owner_lease.visible_path.clone();
    let anchor_path = owner_lease.anchor_path.clone();
    remove_visible_owner_lease(&owner_lease);

    let error = sweep_owner_lease_artifacts(root.path(), &std::collections::HashSet::new())
        .expect_err("a locked anchor-only owner must be reported");
    assert!(error.contains("live subagent owner anchor"), "{error}");
    assert!(anchor_path.exists());
    assert!(!visible_path.exists());

    drop(owner_lease);
    sweep_owner_lease_artifacts(root.path(), &std::collections::HashSet::new())
        .expect("stale anchor-only owner cleanup");
    assert!(!anchor_path.exists());
    assert!(!owner_lease_path(root.path(), &lease_id)
        .expect("visible owner path")
        .exists());
}

#[test]
fn live_anchor_only_running_record_supports_status_list_and_cancel_reconciliation() {
    let root = tempfile::tempdir().expect("root");
    let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
    let lease_id = owner_lease.lease_id.clone();
    let mut record = record_fixture(root.path(), "sub-live-anchor-only", "running");
    attach_execution_ownership(&mut record, &owner_lease);
    write_subagent_record(root.path(), &record).expect("running record");
    remove_visible_owner_lease(&owner_lease);

    let status = get_subagent_record(root.path(), &record.id).expect("status reconciliation");
    assert_eq!(status.status, "running");
    let listed = list_subagents(root.path()).expect("list reconciliation");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["status"], "running");
    let cancel_error = cancel_subagent(root.path(), &record.id)
        .expect_err("an untracked but live owner cannot be reported cancelled");
    assert!(
        cancel_error.contains("observed_status: running"),
        "{cancel_error}"
    );
    assert!(
        !cancel_error.contains("owner lease is unavailable"),
        "{cancel_error}"
    );
    assert!(
        owner_lease_anchor_path(root.path(), &lease_id)
            .expect("anchor path")
            .exists(),
        "the live anchor remains authoritative"
    );

    update_subagent_record(root.path(), &record.id, |current| {
        current.status = "failed".to_string();
        current.updated_at = Utc::now();
        Ok(())
    })
    .expect("terminalize fixture");
    owner_lease
        .remove()
        .expect("clean anchor-only owner through normal owner cleanup");
    assert!(
        !owner_lease_anchor_path(root.path(), &lease_id)
            .expect("anchor path")
            .exists(),
        "normal owner cleanup removes the anchor"
    );
}

#[test]
fn dead_anchor_only_record_without_process_scope_requires_recovery() {
    #[cfg(windows)]
    let _timeout = SubagentCancellationTimeoutGuard::set(Duration::from_secs(10));
    let root = tempfile::tempdir().expect("root");
    crate::session::SessionStore::for_project(root.path())
        .expect("session store")
        .create_session_with_id("parent");
    let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
    let lease_id = owner_lease.lease_id.clone();
    let mut record = record_fixture(root.path(), "sub-dead-anchor-only", "running");
    attach_execution_ownership(&mut record, &owner_lease);
    write_subagent_record(root.path(), &record).expect("running record");
    remove_visible_owner_lease(&owner_lease);
    drop(owner_lease);

    let cancel_error = cancel_subagent(root.path(), &record.id)
        .expect_err("cleanup cannot be inferred from owner loss");
    assert!(
        cancel_error.contains("observed_status: running"),
        "{cancel_error}"
    );
    assert!(cancel_error.contains("untracked"), "{cancel_error}");
    let reconciled =
        get_subagent_record_internal(root.path(), &record.id).expect("reconciled record");
    assert_eq!(reconciled.status, "running");
    let result = reconciled.result.as_ref().expect("recovery evidence");
    assert_eq!(result["outcome"], "recovery_required");
    assert_eq!(result["process_scope"]["status"], "missing");
    assert_eq!(result["cleanup_verified"], false);
    assert!(
        owner_lease_anchor_path(root.path(), &lease_id)
            .expect("anchor path")
            .exists(),
        "the unverified owner anchor remains available for explicit recovery"
    );
    assert!(owner_lease_directory(root.path()).is_dir());
}

#[test]
fn legacy_running_record_and_orphan_cancellation_fail_closed() {
    // Filesystem reconciliation can exceed the test-only 250 ms deadline on CI.
    let _timeout = SubagentCancellationTimeoutGuard::set(Duration::from_secs(10));
    let root = tempfile::tempdir().expect("root");
    let legacy = record_fixture(root.path(), "sub-legacy-owner", "running");
    write_subagent_record(root.path(), &legacy).expect("legacy running record");
    let error = get_subagent_record(root.path(), &legacy.id)
        .expect_err("legacy ownership must not be guessed");
    assert!(error.contains("legacy execution ownership"), "{error}");
    assert_eq!(
        get_subagent_record_unreconciled(root.path(), &legacy.id)
            .expect("legacy record remains durable")
            .status,
        "running"
    );

    let store = crate::session::SessionStore::for_project(root.path()).expect("session store");
    store.create_session_with_id("parent");
    let owner_lease = SubagentOwnerLease::create(root.path()).expect("orphan lease");
    let mut orphan = record_fixture(root.path(), "sub-cancel-orphan", "running");
    attach_execution_ownership(&mut orphan, &owner_lease);
    write_subagent_record(root.path(), &orphan).expect("orphan running record");
    drop(owner_lease);
    match resolve_subagent_cancellation(root.path(), &orphan.id) {
        CancelSubagentResolution::Unresolved {
            manager_stopped,
            observed_status,
            error,
        } => {
            assert!(!manager_stopped);
            assert_eq!(observed_status.as_deref(), Some("running"), "{error}");
            assert!(error.contains("untracked"), "{error}");
            let persisted = get_subagent_record_internal(root.path(), &orphan.id)
                .expect("orphan remains recoverable");
            assert_eq!(persisted.status, "running");
            let result = persisted.result.expect("recovery evidence");
            assert_eq!(result["outcome"], "recovery_required");
            assert_eq!(result["cleanup_verified"], false);
        }
        resolution => panic!("orphan cancellation must fail closed, got {resolution:?}"),
    }
}

#[tokio::test]
async fn contradictory_terminal_record_and_manager_states_fail_closed() {
    #[cfg(windows)]
    let _timeout = SubagentCancellationTimeoutGuard::set(Duration::from_secs(10));
    let root = tempfile::tempdir().expect("root");
    let completed_id = format!("sub-terminal-cancelled-{}", uuid::Uuid::new_v4());
    let completed = record_fixture(root.path(), &completed_id, "completed");
    write_subagent_record(root.path(), &completed).expect("completed record");
    crate::daemons::task::TASK_MANAGER
        .register_task(completed_id.clone(), "subagent")
        .expect("completed manager task");
    let pending = tokio::spawn(std::future::pending::<()>());
    crate::daemons::task::TASK_MANAGER
        .attach_abort_handle(&completed_id, pending.abort_handle())
        .expect("completed manager abort handle");
    crate::daemons::task::TASK_MANAGER
        .cancel(&completed_id)
        .expect("force contradictory cancelled manager state");

    match resolve_subagent_cancellation(root.path(), &completed_id) {
        CancelSubagentResolution::Unresolved { error, .. } => {
            assert!(error.contains("contradicts"), "{error}");
            assert!(error.contains("cancelled"), "{error}");
        }
        resolution => panic!("contradictory completion must fail closed: {resolution:?}"),
    }
    assert!(pending
        .await
        .expect_err("cancelled manager task")
        .is_cancelled());

    let cancelled_id = format!("sub-cancelled-completed-{}", uuid::Uuid::new_v4());
    let cancelled = record_fixture(root.path(), &cancelled_id, "cancelled");
    write_subagent_record(root.path(), &cancelled).expect("cancelled record");
    crate::daemons::task::TASK_MANAGER
        .register_task(cancelled_id.clone(), "subagent")
        .expect("cancelled manager task");
    crate::daemons::task::TASK_MANAGER.complete(&cancelled_id, None);

    match resolve_subagent_cancellation(root.path(), &cancelled_id) {
        CancelSubagentResolution::Unresolved { error, .. } => {
            assert!(error.contains("contradicts"), "{error}");
            assert!(error.contains("completed"), "{error}");
        }
        resolution => panic!("contradictory cancellation must fail closed: {resolution:?}"),
    }
}

#[tokio::test]
async fn async_cancellation_reconciliation_persists_an_aborted_run_on_one_worker() {
    let root = tempfile::tempdir().expect("root");
    let _timeout = SubagentCancellationTimeoutGuard::set(Duration::from_secs(2));
    let id = format!("sub-async-cancel-{}", uuid::Uuid::new_v4());
    let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
    let mut record = record_fixture(root.path(), &id, "running");
    attach_execution_ownership(&mut record, &owner_lease);
    write_subagent_record(root.path(), &record).expect("running record");
    crate::daemons::task::TASK_MANAGER
        .register_task(id.clone(), "subagent")
        .expect("background task");

    let guard = SubagentRunGuard::new(root.path().to_path_buf(), id.clone(), owner_lease);
    let running = tokio::spawn(async move {
        let guard = guard;
        std::future::pending::<()>().await;
        drop(guard);
    });
    crate::daemons::task::TASK_MANAGER
        .attach_abort_handle(&id, running.abort_handle())
        .expect("abort handle");

    match resolve_subagent_cancellation_async(root.path(), &id).await {
        CancelSubagentResolution::Cancelled { record } => {
            assert_eq!(record.status, "cancelled");
            assert_eq!(
                record.error.as_deref(),
                Some("cancelled by manage_subagents")
            );
        }
        resolution => panic!("async cancellation must persist stopped truth: {resolution:?}"),
    }
    assert!(running
        .await
        .expect_err("subagent task is aborted")
        .is_cancelled());
    assert_eq!(
        get_subagent_record_unreconciled(root.path(), &id)
            .expect("durable cancelled record")
            .status,
        "cancelled"
    );
}

#[tokio::test]
async fn async_cancellation_worker_start_delay_cannot_renew_the_api_deadline() {
    let root = tempfile::tempdir().expect("root");
    let id = format!("sub-delayed-cancel-{}", uuid::Uuid::new_v4());
    let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
    let mut record = record_fixture(root.path(), &id, "running");
    attach_execution_ownership(&mut record, &owner_lease);
    write_subagent_record(root.path(), &record).expect("running record");
    drop(owner_lease);
    let namespace_before = subagent_namespace_snapshot(root.path());

    let (worker_ready_tx, worker_ready_rx) = tokio::sync::oneshot::channel();
    let (worker_release_tx, worker_release_rx) = std::sync::mpsc::channel();
    let timeout = Duration::from_millis(50);
    let api_started = Instant::now();
    let cancellation =
        resolve_subagent_cancellation_async_with_start_hook(root.path(), &id, timeout, move || {
            let _ = worker_ready_tx.send(());
            worker_release_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("release delayed reconciliation worker");
        });
    let resolver = tokio::spawn(cancellation);
    tokio::time::timeout(Duration::from_secs(1), worker_ready_rx)
        .await
        .expect("worker start gate timeout")
        .expect("worker reached start gate");
    tokio::time::sleep(timeout * 2).await;

    let released = Instant::now();
    worker_release_tx
        .send(())
        .expect("resume delayed reconciliation worker");
    let resolution = tokio::time::timeout(Duration::from_secs(1), resolver)
        .await
        .expect("expired worker must join within its original budget")
        .expect("resolver task");
    match resolution {
        CancelSubagentResolution::Unresolved { error, .. } => {
            assert!(error.contains("deadline elapsed"), "{error}");
        }
        resolution => panic!("expired API-entry deadline must fail closed: {resolution:?}"),
    }
    assert!(
        api_started.elapsed() >= timeout,
        "fixture did not hold the worker beyond the API-entry deadline"
    );
    assert!(
        released.elapsed() < Duration::from_secs(1),
        "worker gained a new reconciliation budget after its start gate"
    );
    assert_eq!(subagent_namespace_snapshot(root.path()), namespace_before);
    let persisted = get_subagent_record_unreconciled(root.path(), &id).expect("unchanged record");
    assert_eq!(persisted.status, "running");
    assert!(persisted.result.is_none());
    tokio::time::sleep(timeout * 2).await;
    assert_eq!(subagent_namespace_snapshot(root.path()), namespace_before);
}

#[tokio::test]
async fn async_cancellation_worker_abort_joins_a_held_lock_reconciler() {
    let root = tempfile::tempdir().expect("root");
    let id = format!("sub-joined-cancel-{}", uuid::Uuid::new_v4());
    let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
    let mut record = record_fixture(root.path(), &id, "running");
    attach_execution_ownership(&mut record, &owner_lease);
    write_subagent_record(root.path(), &record).expect("running record");
    drop(owner_lease);
    let held = hold_subagent_record_lock_for_test(root.path(), &id).expect("held record lock");

    let project_root = root.path().to_path_buf();
    let cancellation_id = id.clone();
    let resolver = tokio::spawn(async move {
        resolve_subagent_cancellation_async(&project_root, &cancellation_id).await
    });
    tokio::time::sleep(Duration::from_millis(25)).await;
    let started = Instant::now();
    resolver.abort();
    assert!(resolver
        .await
        .expect_err("resolver task is aborted")
        .is_cancelled());
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "worker join exceeded its absolute reconciliation deadline"
    );

    drop(held);
    tokio::time::sleep(SUBAGENT_CANCELLATION_RECONCILIATION_TIMEOUT * 2).await;
    let persisted =
        get_subagent_record_unreconciled(root.path(), &id).expect("unreconciled record");
    assert_eq!(persisted.status, "running");
    assert!(
        persisted.result.is_none(),
        "an aborted reconciliation worker mutated the record after its owner returned"
    );
}

#[tokio::test]
async fn async_cancellation_worker_panic_is_fail_closed() {
    let id = format!("sub-panicked-cancel-{}", uuid::Uuid::new_v4());
    match run_subagent_cancellation_worker(id, || panic!("worker panic fixture")).await {
        CancelSubagentResolution::Unresolved {
            manager_stopped,
            observed_status,
            error,
        } => {
            assert!(!manager_stopped);
            assert!(observed_status.is_none());
            assert!(error.contains("worker panicked before shutdown"), "{error}");
        }
        resolution => panic!("worker panic must fail closed: {resolution:?}"),
    }
}

#[test]
fn subagent_record_directory_replacement_child() {
    let Some(project_root) = std::env::var_os(RECORD_WRITE_CHILD_PROJECT_ROOT) else {
        return;
    };
    let project_root = PathBuf::from(project_root);
    let record = record_fixture(&project_root, "replacement-record", "completed");
    let error = write_subagent_record(&project_root, &record)
        .expect_err("detached records directory must reject a pure record commit");
    assert!(error.contains("identity changed"), "{error}");
}

#[test]
fn subagent_record_destination_replacement_child() {
    let Some(project_root) = std::env::var_os(RECORD_WRITE_CHILD_PROJECT_ROOT) else {
        return;
    };
    let project_root = PathBuf::from(project_root);
    let record = record_fixture(&project_root, "replacement-file", "completed");
    let error = write_subagent_record(&project_root, &record)
        .expect_err("a replacement record must defeat Missing publication");
    assert!(error.contains("appeared"), "{error}");
}

#[test]
fn subagent_record_revision_replacement_child() {
    let Some(project_root) = std::env::var_os(RECORD_WRITE_CHILD_PROJECT_ROOT) else {
        return;
    };
    let project_root = PathBuf::from(project_root);
    let records = ensure_records_directory(&project_root).expect("records");
    let directory = crate::daemons::state::StableDirectory::open(&records).expect("directory");
    let path = record_path(&project_root, "replacement-revision").expect("record path");
    let mut opened = read_opened_subagent_record_in(&directory, &path).expect("revision");
    opened.record.status = "completed".to_string();
    opened.record.updated_at = Utc::now();
    let error = persist_subagent_record_revision(&project_root, &opened.record, &mut opened.file)
        .expect_err("replacement must defeat Present publication");
    assert!(
        error.contains("identity") || error.contains("changed"),
        "{error}"
    );
}

#[test]
fn bounded_subagent_record_deadline_child() {
    let Some(project_root) = std::env::var_os(RECORD_WRITE_CHILD_PROJECT_ROOT) else {
        return;
    };
    let project_root = PathBuf::from(project_root);
    let error = update_subagent_record_until(
        &project_root,
        "expired-reconciliation-write",
        Some(Instant::now() + Duration::from_secs(2)),
        |record| {
            record.status = "failed".to_string();
            record.error = Some("must not publish".to_string());
            record.updated_at = Utc::now();
            Ok(())
        },
    )
    .expect_err("expired bounded record publication must fail");
    assert!(
        error.contains("delegation state lock deadline elapsed"),
        "{error}"
    );
}

#[test]
fn bounded_subagent_finalization_deadline_child() {
    let Some(project_root) = std::env::var_os(RECORD_WRITE_CHILD_PROJECT_ROOT) else {
        return;
    };
    let project_root = PathBuf::from(project_root);
    inject_recoverable_revision_publication_failures("expired-finalization-write", 1);
    let error = update_subagent_record_until(
        &project_root,
        "expired-finalization-write",
        Some(Instant::now() + Duration::from_secs(3)),
        |record| {
            record.status = "failed".to_string();
            record.error = Some("published before finalization pause".to_string());
            record.updated_at = Utc::now();
            Ok(())
        },
    )
    .expect_err("expired exact-receipt finalization must fail closed");
    assert!(
        error.contains("delegation state lock deadline elapsed"),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
fn subagent_record_write_never_redirects_to_replaced_records_directory() {
    let root = tempfile::tempdir().expect("root");
    let record = record_fixture(root.path(), "replacement-record", "running");
    write_subagent_record(root.path(), &record).expect("initial record");
    let path = record_path(root.path(), &record.id).expect("record path");
    let initial = std::fs::read(&path).expect("initial record bytes");
    let ready = root.path().join("record-write.ready");
    let resume = root.path().join("record-write.resume");
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "tools::delegation::tests::test_part_2::subagent_record_directory_replacement_child",
            "--nocapture",
        ])
        .env(RECORD_WRITE_CHILD_PROJECT_ROOT, root.path())
        .env("NIB_TEST_SUBAGENT_WRITE_ID", &record.id)
        .env("NIB_TEST_SUBAGENT_WRITE_READY", &ready)
        .env("NIB_TEST_SUBAGENT_WRITE_RESUME", &resume)
        .spawn()
        .expect("spawn record writer child");
    let started = Instant::now();
    while !ready.exists() {
        if let Some(status) = child.try_wait().expect("inspect record writer child") {
            panic!("record writer exited before pause: {status}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "record writer did not pause before commit"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let records = records_dir(root.path());
    let displaced = root.path().join(".nib/subagents.displaced-write");
    std::fs::rename(&records, &displaced).expect("detach records directory");
    std::fs::create_dir(&records).expect("replacement records directory");
    std::fs::write(records.join("replacement-record.json"), &initial)
        .expect("replacement record sentinel");
    std::fs::write(&resume, b"resume").expect("resume record writer");
    let status = child.wait().expect("wait for record writer child");
    assert!(status.success(), "record writer child failed: {status}");
    assert_eq!(
        std::fs::read(displaced.join("replacement-record.json"))
            .expect("original record after aborted write"),
        initial
    );
    assert_eq!(
        std::fs::read(records.join("replacement-record.json"))
            .expect("replacement record sentinel"),
        initial
    );
}

#[test]
fn missing_record_creation_preserves_a_destination_replacement() {
    let root = tempfile::tempdir().expect("root");
    let ready = root.path().join("record-create.ready");
    let resume = root.path().join("record-create.resume");
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "tools::delegation::tests::test_part_2::subagent_record_destination_replacement_child",
            "--nocapture",
        ])
        .env(RECORD_WRITE_CHILD_PROJECT_ROOT, root.path())
        .env("NIB_TEST_SUBAGENT_WRITE_ID", "replacement-file")
        .env("NIB_TEST_SUBAGENT_WRITE_READY", &ready)
        .env("NIB_TEST_SUBAGENT_WRITE_RESUME", &resume)
        .spawn()
        .expect("spawn record creator child");
    let started = Instant::now();
    while !ready.exists() {
        if let Some(status) = child.try_wait().expect("inspect record creator child") {
            panic!("record creator exited before pause: {status}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "record creator did not pause before publication"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let mut replacement = record_fixture(root.path(), "replacement-file", "failed");
    replacement.error = Some("replacement sentinel".to_string());
    let path = record_path(root.path(), &replacement.id).expect("replacement path");
    let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("replacement JSON");
    std::fs::write(&path, &replacement_bytes).expect("publish replacement record");
    std::fs::write(&resume, b"resume").expect("resume record creator");
    let status = child.wait().expect("wait for record creator child");
    assert!(status.success(), "record creator child failed: {status}");
    assert_eq!(
        std::fs::read(&path).expect("replacement record remains"),
        replacement_bytes
    );
}

#[test]
fn expected_record_revision_rechecks_identity_after_the_precommit_pause() {
    let root = tempfile::tempdir().expect("root");
    let initial = record_fixture(root.path(), "replacement-revision", "running");
    write_subagent_record(root.path(), &initial).expect("initial record");
    let ready = root.path().join("record-revision.ready");
    let resume = root.path().join("record-revision.resume");
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "tools::delegation::tests::test_part_2::subagent_record_revision_replacement_child",
            "--nocapture",
        ])
        .env(RECORD_WRITE_CHILD_PROJECT_ROOT, root.path())
        .env("NIB_TEST_SUBAGENT_WRITE_ID", "replacement-revision")
        .env("NIB_TEST_SUBAGENT_WRITE_READY", &ready)
        .env("NIB_TEST_SUBAGENT_WRITE_RESUME", &resume)
        .spawn()
        .expect("spawn record revision child");
    let started = Instant::now();
    while !ready.exists() {
        if let Some(status) = child.try_wait().expect("inspect record revision child") {
            panic!("record revision child exited before pause: {status}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "record revision child did not pause before publication"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let path = record_path(root.path(), &initial.id).expect("record path");
    let displaced = path.with_extension("old-revision");
    std::fs::rename(&path, &displaced).expect("displace expected revision");
    let mut replacement = initial.clone();
    replacement.status = "failed".to_string();
    replacement.error = Some("replacement sentinel".to_string());
    replacement.updated_at = Utc::now();
    let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("replacement JSON");
    std::fs::write(&path, &replacement_bytes).expect("publish replacement revision");
    std::fs::write(&resume, b"resume").expect("resume record revision child");
    let status = child.wait().expect("wait for record revision child");
    assert!(status.success(), "record revision child failed: {status}");
    assert_eq!(
        std::fs::read(&path).expect("replacement revision remains"),
        replacement_bytes
    );
    assert!(displaced.exists(), "expected revision remains displaced");
}

#[test]
fn bounded_reconciliation_record_does_not_publish_after_precommit_expiry() {
    let root = tempfile::tempdir().expect("root");
    let initial = record_fixture(root.path(), "expired-reconciliation-write", "running");
    write_subagent_record(root.path(), &initial).expect("initial record");
    let path = record_path(root.path(), &initial.id).expect("record path");
    let original = std::fs::read(&path).expect("initial record bytes");
    let ready = root.path().join("record-deadline.ready");
    let resume = root.path().join("record-deadline.resume");
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "tools::delegation::tests::test_part_2::bounded_subagent_record_deadline_child",
            "--nocapture",
        ])
        .env(RECORD_WRITE_CHILD_PROJECT_ROOT, root.path())
        .env("NIB_TEST_SUBAGENT_WRITE_ID", &initial.id)
        .env("NIB_TEST_SUBAGENT_WRITE_READY", &ready)
        .env("NIB_TEST_SUBAGENT_WRITE_RESUME", &resume)
        .spawn()
        .expect("spawn bounded record writer child");
    let started = Instant::now();
    while !ready.exists() {
        if let Some(status) = child.try_wait().expect("inspect record writer child") {
            panic!("bounded record writer exited before pause: {status}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "bounded record writer did not pause before publication"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let paused_namespace = subagent_namespace_snapshot(&records_dir(root.path()));
    std::thread::sleep(Duration::from_millis(2_100));
    std::fs::write(&resume, b"resume").expect("resume bounded record writer");

    let status = child.wait().expect("wait for bounded record writer child");
    assert!(
        status.success(),
        "bounded record writer child failed: {status}"
    );
    assert_eq!(
        std::fs::read(&path).expect("record remains readable"),
        original,
        "bounded reconciliation published after its absolute deadline"
    );
    assert_eq!(
        subagent_namespace_snapshot(&records_dir(root.path())),
        paused_namespace,
        "expired record save mutated its transaction namespace"
    );
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        subagent_namespace_snapshot(&records_dir(root.path())),
        paused_namespace,
        "expired record save mutated its transaction namespace later"
    );
}

#[test]
fn exact_receipt_finalization_preserves_its_namespace_after_expiry() {
    let root = tempfile::tempdir().expect("root");
    let initial = record_fixture(root.path(), "expired-finalization-write", "running");
    write_subagent_record(root.path(), &initial).expect("initial record");
    let records = ensure_records_directory(root.path()).expect("records directory");
    let directory =
        crate::daemons::state::StableDirectory::open(&records).expect("stable records directory");
    let path = record_path(root.path(), &initial.id).expect("record path");
    let temporary = directory
        .deterministic_artifact_path(&path, ".nib-subagent-", ".tmp")
        .expect("temporary artifact path");
    let ready = root.path().join("record-finalization.ready");
    let resume = root.path().join("record-finalization.resume");
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "tools::delegation::tests::test_part_2::bounded_subagent_finalization_deadline_child",
            "--nocapture",
        ])
        .env(RECORD_WRITE_CHILD_PROJECT_ROOT, root.path())
        .env("NIB_TEST_SUBAGENT_FINALIZE_ID", &initial.id)
        .env("NIB_TEST_SUBAGENT_FINALIZE_READY", &ready)
        .env("NIB_TEST_SUBAGENT_FINALIZE_RESUME", &resume)
        .spawn()
        .expect("spawn exact-receipt finalizer child");
    let started = Instant::now();
    while !ready.exists() {
        if let Some(status) = child.try_wait().expect("inspect finalizer child") {
            panic!("exact-receipt finalizer exited before pause: {status}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "exact-receipt finalizer did not pause"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::write(
        &temporary,
        b"preserve post-publication transaction artifact",
    )
    .expect("temporary finalization fixture");
    let paused_namespace = subagent_namespace_snapshot(&records);
    std::thread::sleep(Duration::from_millis(3_200));
    std::fs::write(&resume, b"resume").expect("resume exact-receipt finalizer");

    let status = child.wait().expect("wait for exact-receipt finalizer");
    assert!(
        status.success(),
        "exact-receipt finalizer child failed: {status}"
    );
    assert_eq!(
        subagent_namespace_snapshot(&records),
        paused_namespace,
        "expired exact-receipt finalization mutated its namespace"
    );
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        subagent_namespace_snapshot(&records),
        paused_namespace,
        "expired exact-receipt finalization mutated its namespace later"
    );
    assert!(
        temporary.exists(),
        "expired finalizer removed its transaction artifact"
    );
}

#[test]
fn post_publication_refresh_rejects_a_substituted_record_generation() {
    let root = tempfile::tempdir().expect("root");
    let initial = record_fixture(root.path(), "post-publish-substitution", "running");
    write_subagent_record(root.path(), &initial).expect("initial record");
    let records = ensure_records_directory(root.path()).expect("records");
    let directory = crate::daemons::state::StableDirectory::open(&records).expect("directory");
    let path = record_path(root.path(), &initial.id).expect("record path");
    let mut opened = read_opened_subagent_record_in(&directory, &path).expect("opened record");
    let mut committed = initial.clone();
    committed.status = "completed".to_string();
    committed.updated_at = Utc::now();
    let replacement = committed.clone();
    let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("replacement JSON");
    let displaced = path.with_extension("published-displaced");

    let error = persist_subagent_record_revision_with_refresh_hook(
        root.path(),
        &committed,
        &mut opened.file,
        || {
            std::fs::rename(&path, &displaced).map_err(|error| error.to_string())?;
            std::fs::write(&path, &replacement_bytes).map_err(|error| error.to_string())
        },
    )
    .expect_err("a substituted post-publication generation must not be adopted");
    assert!(error.contains("identity-distinct"), "{error}");
    assert_eq!(
        std::fs::read(&path).expect("replacement remains visible"),
        replacement_bytes
    );
    assert!(displaced.exists(), "committed generation remains displaced");

    committed.status = "verification_failed".to_string();
    let error = persist_subagent_record_revision(root.path(), &committed, &mut opened.file)
        .expect_err("the prior handle must not have adopted the replacement");
    assert!(
        error.contains("identity") || error.contains("changed"),
        "{error}"
    );
    assert_eq!(
        std::fs::read(&path).expect("replacement remains after stale retry"),
        replacement_bytes
    );
}

#[test]
fn recoverable_revision_publication_error_finalizes_and_adopts_exact_receipt() {
    let root = tempfile::tempdir().expect("root");
    let initial = record_fixture(root.path(), "recoverable-revision-receipt", "running");
    write_subagent_record(root.path(), &initial).expect("initial record");
    let records = ensure_records_directory(root.path()).expect("records");
    let directory = crate::daemons::state::StableDirectory::open(&records).expect("directory");
    let path = record_path(root.path(), &initial.id).expect("record path");
    let mut opened = read_opened_subagent_record_in(&directory, &path).expect("opened record");
    let mut revision = opened.record.clone();
    revision.status = "completed".to_string();
    revision.result = Some(json!({"publication": "recovered"}));
    revision.updated_at = Utc::now();
    inject_recoverable_revision_publication_failures(&revision.id, 1);

    persist_subagent_record_revision(root.path(), &revision, &mut opened.file)
        .expect("exact publication receipt must support finalization and adoption");

    let authoritative =
        read_opened_subagent_record_in(&directory, &path).expect("authoritative revision");
    assert_eq!(
        serde_json::to_value(&authoritative.record).expect("authoritative JSON"),
        serde_json::to_value(&revision).expect("expected JSON")
    );
    assert!(
        crate::daemons::state::same_open_file_identity(&opened.file, &authoritative.file)
            .expect("compare adopted receipt with authoritative revision")
    );
    directory
        .verify_file_identity(&path, &opened.file)
        .expect("adopted revision receipt remains authoritative");
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn initial_refresh_rejects_an_identity_distinct_json_equivalent_replacement() {
    let root = tempfile::tempdir().expect("root");
    let attempted = record_fixture(root.path(), "initial-equivalent-refresh", "running");
    let path = record_path(root.path(), &attempted.id).expect("record path");
    let displaced = path.with_extension("initial-publication-displaced");
    let attempted_bytes = serde_json::to_vec_pretty(&attempted).expect("attempted JSON");

    let error = match write_subagent_record_with_refresh_hook(root.path(), &attempted, || {
        std::fs::rename(&path, &displaced).map_err(|error| error.to_string())?;
        std::fs::write(&path, &attempted_bytes).map_err(|error| error.to_string())
    }) {
        Ok(_) => panic!("JSON equality cannot authorize adopting a distinct file generation"),
        Err(error) => error,
    };

    assert!(error.message.contains("identity-distinct"), "{error:?}");
    assert!(
        error
            .receipt
            .as_ref()
            .is_some_and(|receipt| receipt.exact_identity),
        "{error:?}"
    );
    assert_eq!(
        std::fs::read(&path).expect("equivalent replacement"),
        attempted_bytes
    );
    assert!(displaced.exists(), "original publication remains displaced");
}

#[test]
fn post_publication_failure_receipt_preserves_a_substituted_record() {
    let root = tempfile::tempdir().expect("root");
    let attempted = record_fixture(root.path(), "post-publish-failure", "running");
    let path = record_path(root.path(), &attempted.id).expect("record path");
    let displaced = path.with_extension("failed-publication-displaced");
    let attempted_bytes = serde_json::to_vec_pretty(&attempted).expect("attempted JSON");

    let error = match write_subagent_record_with_refresh_hook(root.path(), &attempted, || {
        std::fs::rename(&path, &displaced).map_err(|error| error.to_string())?;
        std::fs::write(&path, &attempted_bytes).map_err(|error| error.to_string())?;
        Err("injected post-publication validation failure".to_string())
    }) {
        Ok(_) => panic!("post-publication failure must retain its publication receipt"),
        Err(error) => error,
    };
    assert!(error.receipt.is_some(), "{error:?}");
    let cleanup = cleanup_record_after_publication_failure(root.path(), &attempted, &error)
        .expect_err("the original receipt cannot authorize deleting the replacement");
    if error
        .receipt
        .as_ref()
        .is_some_and(|receipt| receipt.exact_identity)
    {
        assert!(cleanup.contains("publication identity"), "{cleanup}");
    } else {
        assert!(cleanup.contains("identity is unavailable"), "{cleanup}");
    }
    assert_eq!(
        std::fs::read(&path).expect("substituted record remains"),
        attempted_bytes
    );
    assert!(displaced.exists(), "original publication remains displaced");
}

#[test]
fn precommit_cleanup_preserves_a_record_replaced_before_quarantine() {
    let root = tempfile::tempdir().expect("root");
    let attempted = record_fixture(root.path(), "precommit-replacement", "running");
    let _publication = write_subagent_record_with_refresh_hook(root.path(), &attempted, || Ok(()))
        .expect("attempted record");
    let path = record_path(root.path(), &attempted.id).expect("record path");
    let records = ensure_records_directory(root.path()).expect("records");
    let directory = crate::daemons::state::StableDirectory::open(&records).expect("directory");
    let opened = read_opened_subagent_record_in(&directory, &path).expect("opened record");
    let displaced = path.with_extension("displaced");
    let mut replacement = attempted.clone();
    replacement.status = "failed".to_string();
    replacement.error = Some("replacement sentinel".to_string());
    replacement.updated_at = Utc::now();
    let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("replacement JSON");

    let error =
        cleanup_precommit_record_with_hook(root.path(), &attempted, Some(&opened.file), || {
            std::fs::rename(&path, &displaced).map_err(|error| error.to_string())?;
            std::fs::write(&path, &replacement_bytes).map_err(|error| error.to_string())?;
            Ok(())
        })
        .expect_err("replacement must defeat conditional precommit deletion");
    assert!(
        error.contains("changed") || error.contains("identity"),
        "{error}"
    );
    assert_eq!(
        std::fs::read(&path).expect("replacement remains"),
        replacement_bytes
    );
    assert!(displaced.exists(), "attempted generation remains displaced");
}

#[test]
fn precommit_cleanup_preserves_an_in_place_record_mutation() {
    let root = tempfile::tempdir().expect("root");
    let attempted = record_fixture(root.path(), "precommit-in-place", "running");
    let publication = write_subagent_record_with_refresh_hook(root.path(), &attempted, || Ok(()))
        .expect("attempted record");
    let path = record_path(root.path(), &attempted.id).expect("record path");
    let mut replacement = attempted.clone();
    replacement.status = "failed".to_string();
    replacement.error = Some("in-place replacement".to_string());
    replacement.updated_at = Utc::now();
    let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("replacement JSON");

    let error = cleanup_precommit_record_with_hook(
        root.path(),
        &attempted,
        Some(&publication.receipt.file),
        || std::fs::write(&path, &replacement_bytes).map_err(|error| error.to_string()),
    )
    .expect_err("in-place mutation must defeat semantic cleanup");

    assert!(error.contains("bytes changed"), "{error}");
    assert_eq!(
        std::fs::read(&path).expect("mutated record remains"),
        replacement_bytes
    );
}

#[test]
fn precommit_cleanup_preserves_an_identity_distinct_json_equivalent_replacement() {
    let root = tempfile::tempdir().expect("root");
    let attempted = record_fixture(root.path(), "precommit-equivalent", "running");
    let _publication = write_subagent_record_with_refresh_hook(root.path(), &attempted, || Ok(()))
        .expect("attempted record");
    let path = record_path(root.path(), &attempted.id).expect("record path");
    let records = ensure_records_directory(root.path()).expect("records");
    let directory = crate::daemons::state::StableDirectory::open(&records).expect("directory");
    let opened = read_opened_subagent_record_in(&directory, &path).expect("opened record");
    let displaced = path.with_extension("attempted-displaced");
    let attempted_bytes = serde_json::to_vec_pretty(&attempted).expect("attempted JSON");
    std::fs::rename(&path, &displaced).expect("displace attempted publication");
    std::fs::write(&path, &attempted_bytes).expect("publish JSON-equivalent replacement");

    let error = cleanup_precommit_record(root.path(), &attempted, Some(&opened.file))
        .expect_err("JSON equality cannot authorize deletion of a distinct file identity");
    assert!(error.contains("publication identity"), "{error}");
    assert_eq!(
        std::fs::read(&path).expect("equivalent replacement remains"),
        attempted_bytes
    );
    assert!(
        displaced.exists(),
        "attempted publication remains displaced"
    );
}

#[test]
fn registration_failure_receipt_preserves_a_json_equivalent_substitution() {
    let root = tempfile::tempdir().expect("root");
    let attempted = record_fixture(root.path(), "registration-equivalent", "running");
    let publication = write_subagent_record_with_refresh_hook(root.path(), &attempted, || Ok(()))
        .expect("attempted record");
    let path = record_path(root.path(), &attempted.id).expect("record path");
    let displaced = path.with_extension("registered-publication-displaced");
    let attempted_bytes = serde_json::to_vec_pretty(&attempted).expect("attempted JSON");
    std::fs::rename(&path, &displaced).expect("displace registered publication");
    std::fs::write(&path, &attempted_bytes).expect("publish JSON-equivalent replacement");

    let error = cleanup_record_after_registration_failure(root.path(), &attempted, &publication)
        .expect_err("registration compensation cannot delete a substituted record");
    if publication.receipt.exact_identity {
        assert!(error.contains("publication identity"), "{error}");
    } else {
        assert!(error.contains("identity is unavailable"), "{error}");
    }
    assert_eq!(
        std::fs::read(&path).expect("registration replacement remains"),
        attempted_bytes
    );
    assert!(
        displaced.exists(),
        "the exact attempted publication remains displaced"
    );
}

#[test]
fn registration_failure_uses_only_an_exact_publication_receipt_for_cleanup() {
    let root = tempfile::tempdir().expect("root");
    let attempted = record_fixture(root.path(), "registration-cleanup", "running");
    let publication = write_subagent_record_with_refresh_hook(root.path(), &attempted, || Ok(()))
        .expect("attempted record");
    let path = record_path(root.path(), &attempted.id).expect("record path");

    let cleanup = cleanup_record_after_registration_failure(root.path(), &attempted, &publication);
    if publication.receipt.exact_identity {
        cleanup.expect("exact receipt authorizes conditional cleanup");
        assert!(!path.exists());
    } else {
        let error = cleanup.expect_err("non-exact receipt must preserve the publication");
        assert!(error.contains("identity is unavailable"), "{error}");
        assert!(path.exists());
    }
}

#[test]
fn stale_record_revision_cannot_overwrite_a_newer_generation() {
    let root = tempfile::tempdir().expect("root");
    let record = record_fixture(root.path(), "stale-revision", "running");
    write_subagent_record(root.path(), &record).expect("initial record");
    let records = ensure_records_directory(root.path()).expect("records");
    let directory = crate::daemons::state::StableDirectory::open(&records).expect("directory");
    let path = record_path(root.path(), &record.id).expect("record path");
    let mut stale = read_opened_subagent_record_in(&directory, &path).expect("stale revision");
    update_subagent_record(root.path(), &record.id, |current| {
        current.status = "completed".to_string();
        current.updated_at = Utc::now();
        Ok(())
    })
    .expect("newer revision");
    stale.record.status = "failed".to_string();
    let error = persist_subagent_record_revision(root.path(), &stale.record, &mut stale.file)
        .expect_err("stale expected handle must fail");
    assert!(
        error.contains("identity") || error.contains("changed"),
        "{error}"
    );
    assert_eq!(
        get_subagent_record(root.path(), &record.id)
            .expect("authoritative revision")
            .status,
        "completed"
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn records_namespace_requires_native_origin_or_explicit_offline_attestation() {
    let existing = tempfile::tempdir().expect("existing root");
    let existing_records = records_dir(existing.path());
    std::fs::create_dir_all(&existing_records).expect("pre-existing clean records namespace");
    let before = directory_tree_snapshot(&existing_records);

    let error = ensure_records_directory(existing.path())
        .expect_err("a clean but unmarked existing namespace is not proof of quiescence");
    assert!(error.contains("confirm-no-legacy-processes"), "{error}");
    assert_eq!(directory_tree_snapshot(&existing_records), before);
    assert!(!existing_records
        .join(LEGACY_RECORD_LOCK_MIGRATION_RECEIPT)
        .exists());

    assert_eq!(
        confirm_no_legacy_subagent_processes(existing.path())
            .expect("operator attestation authorizes the existing clean namespace"),
        0
    );
    ensure_records_directory(existing.path()).expect("completed epoch authorizes ordinary use");
    let existing_directory =
        crate::daemons::state::StableDirectory::open(&existing_records).expect("records");
    let receipt = load_legacy_record_lock_migration_receipt(&existing_directory)
        .expect("receipt load")
        .expect("completed operator receipt");
    assert_eq!(receipt.phase, LegacyRecordLockMigrationPhase::Completed);
    assert_eq!(
        receipt.records_identity,
        records_directory_identity(&existing_directory).expect("records identity")
    );

    let native = tempfile::tempdir().expect("native root");
    let native_records = ensure_records_directory(native.path())
        .expect("current build creates a marked records namespace");
    let native_directory =
        crate::daemons::state::StableDirectory::open(&native_records).expect("native records");
    let receipt = load_legacy_record_lock_migration_receipt(&native_directory)
        .expect("native receipt load")
        .expect("native-origin receipt");
    assert_eq!(receipt.phase, LegacyRecordLockMigrationPhase::Completed);
    assert!(receipt.artifacts.is_empty());
    assert_eq!(
        receipt.records_identity,
        records_directory_identity(&native_directory).expect("native records identity")
    );

    let interrupted = tempfile::tempdir().expect("interrupted native root");
    let staging = interrupted
        .path()
        .join(".nib")
        .join(NATIVE_RECORDS_STAGING_DIRECTORY);
    std::fs::create_dir_all(&staging).expect("interrupted native staging");
    std::fs::write(staging.join("interrupted.sentinel"), b"incomplete")
        .expect("interrupted staging sentinel");
    let interrupted_before = directory_tree_snapshot(&staging);
    let error = ensure_records_directory(interrupted.path())
        .expect_err("ordinary operation preserves incomplete native staging");
    assert!(error.contains("confirm-no-legacy-processes"), "{error}");
    assert_eq!(directory_tree_snapshot(&staging), interrupted_before);
    assert!(!records_dir(interrupted.path()).exists());
    let error = confirm_no_legacy_subagent_processes(interrupted.path())
        .expect_err("explicit attestation cannot claim ownership of hostile staging");
    assert!(error.contains("preserved for inspection"), "{error}");
    assert_eq!(
        directory_tree_snapshot(&staging),
        interrupted_before,
        "doctor must preserve every hostile staging byte"
    );
    assert!(!records_dir(interrupted.path()).exists());

    let resumable = tempfile::tempdir().expect("resumable native root");
    let nib = resumable.path().join(".nib");
    std::fs::create_dir(&nib).expect("resumable nib directory");
    let nib_directory =
        crate::daemons::state::StableDirectory::open(&nib).expect("resumable nib capability");
    let resumable_staging = nib.join(NATIVE_RECORDS_STAGING_DIRECTORY);
    let staged = create_native_records_staging(
        &nib_directory,
        &resumable_staging,
        Instant::now() + Duration::from_secs(2),
    )
    .expect("valid exact native staging");
    drop(staged);
    let exact_receipt = std::fs::read(resumable_staging.join(LEGACY_RECORD_LOCK_MIGRATION_RECEIPT))
        .expect("exact native receipt");
    assert_eq!(
        confirm_no_legacy_subagent_processes(resumable.path())
            .expect("valid exact staging resumes safely"),
        0
    );
    assert!(!resumable_staging.exists());
    ensure_records_directory(resumable.path())
        .expect("resumed native publication authorizes ordinary use");

    let mismatch = tempfile::tempdir().expect("mismatched native root");
    let mismatch_staging = mismatch
        .path()
        .join(".nib")
        .join(NATIVE_RECORDS_STAGING_DIRECTORY);
    std::fs::create_dir_all(&mismatch_staging).expect("mismatched native staging");
    std::fs::write(
        mismatch_staging.join(LEGACY_RECORD_LOCK_MIGRATION_RECEIPT),
        exact_receipt,
    )
    .expect("foreign native receipt");
    let mismatch_before = directory_tree_snapshot(&mismatch_staging);
    let error = confirm_no_legacy_subagent_processes(mismatch.path())
        .expect_err("foreign identity receipt must not authorize staging");
    assert!(error.contains("preserved for inspection"), "{error}");
    assert_eq!(directory_tree_snapshot(&mismatch_staging), mismatch_before);
    assert!(!records_dir(mismatch.path()).exists());
}

#[cfg(any(unix, windows))]
#[test]
fn pending_offline_epoch_is_resumable_only_by_explicit_doctor_attestation() {
    let root = tempfile::tempdir().expect("root");
    let records = ensure_records_directory(root.path()).expect("native records");
    let legacy = legacy_record_lock_path(&records, "pending-epoch").expect("legacy path");
    let anchor = crate::daemons::state::daemon_lock_anchor_path(&legacy).expect("legacy anchor");
    std::fs::create_dir(legacy.parent().expect("legacy parent")).expect("legacy directory");
    std::fs::write(&legacy, b"pending epoch").expect("legacy visible");
    std::fs::hard_link(&legacy, &anchor).expect("legacy anchor");
    let directory =
        crate::daemons::state::StableDirectory::open(&records).expect("records directory");
    let scan = scan_legacy_record_lock_namespaces(&directory, None).expect("legacy scan");
    let receipt = LegacyRecordLockMigrationReceipt {
        version: LEGACY_RECORD_LOCK_MIGRATION_RECEIPT_VERSION,
        epoch_id: uuid::Uuid::new_v4().to_string(),
        records_identity: records_directory_identity(&directory).expect("records identity"),
        phase: LegacyRecordLockMigrationPhase::Pending,
        attested_at: Utc::now(),
        completed_at: None,
        artifacts: scan.artifacts,
    };
    save_legacy_record_lock_migration_receipt(&directory, &receipt, None)
        .expect("persist interrupted pending epoch");

    let error = ensure_records_directory(root.path())
        .expect_err("pending epoch never authorizes an ordinary operation");
    assert!(error.contains("confirm-no-legacy-processes"), "{error}");
    assert!(legacy.exists());
    assert!(anchor.exists());

    assert_eq!(
        confirm_no_legacy_subagent_processes(root.path())
            .expect("explicit renewed attestation resumes the exact pending manifest"),
        2
    );
    assert!(!legacy.exists());
    assert!(!anchor.exists());
    ensure_records_directory(root.path()).expect("completed resumed epoch authorizes use");
}

#[cfg(any(unix, windows))]
#[test]
fn global_subagent_legacy_lock_migration_is_bounded_and_fails_closed_for_live_owner() {
    let root = tempfile::tempdir().expect("root");
    let records = records_dir(root.path());
    let legacy = records.join(".locks");
    std::fs::create_dir_all(&legacy).expect("legacy lock directory");
    let mut legacy_pairs = Vec::new();
    let legacy_pair_count = if cfg!(windows) {
        SUBAGENT_RECORD_LOCK_STRIPES + 1
    } else {
        96
    };
    for index in 0..legacy_pair_count {
        let visible = legacy.join(format!("untouched-{index}.lock"));
        let anchor =
            crate::daemons::state::daemon_lock_anchor_path(&visible).expect("legacy anchor path");
        std::fs::write(&visible, b"legacy").expect("legacy lock");
        std::fs::hard_link(&visible, &anchor).expect("legacy anchor");
        legacy_pairs.push((visible, anchor));
    }

    let error = ensure_records_directory(root.path())
        .expect_err("ordinary startup must not consume an unattested legacy namespace");
    assert!(error.contains("confirm-no-legacy-processes"), "{error}");
    assert!(legacy_pairs
        .iter()
        .all(|(visible, anchor)| visible.exists() && anchor.exists()));

    assert_eq!(
        confirm_no_legacy_subagent_processes(root.path())
            .expect("explicit offline legacy migration"),
        legacy_pairs.len() * 2
    );
    for (visible, anchor) in &legacy_pairs {
        assert!(!visible.exists());
        assert!(!anchor.exists());
    }
    let fixed_visible = std::fs::read_dir(root.path().join(".nib"))
        .expect("fixed lock directory")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".subagent-record-stripe-")
        })
        .count();
    let fixed_anchors = std::fs::read_dir(root.path())
        .expect("fixed anchor directory")
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.contains("subagent-record-stripe-") && name.ends_with(".anchor")
        })
        .count();
    assert!(fixed_visible <= SUBAGENT_RECORD_LOCK_STRIPES);
    assert_eq!(fixed_visible, fixed_anchors);

    let live_visible = legacy.join("untouched-live.lock");
    let live_anchor = crate::daemons::state::daemon_lock_anchor_path(&live_visible)
        .expect("live legacy anchor path");
    std::fs::write(&live_visible, b"live legacy").expect("live legacy lock");
    std::fs::hard_link(&live_visible, &live_anchor).expect("live legacy anchor");
    let owner = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&live_anchor)
        .expect("legacy owner");
    owner.try_lock().expect("hold legacy lock");
    let error = ensure_records_directory(root.path())
        .expect_err("ordinary operation must reject post-epoch legacy state");
    assert!(error.contains("confirm-no-legacy-processes"), "{error}");
    assert!(live_visible.exists());
    assert!(live_anchor.exists());
    let error = confirm_no_legacy_subagent_processes(root.path())
        .expect_err("attested migration must still fail closed for a live owner");
    assert!(error.contains("still owned"), "{error}");
    drop(owner);
    assert_eq!(
        confirm_no_legacy_subagent_processes(root.path())
            .expect("fresh attestation migrates the released legacy lock"),
        2
    );
    ensure_records_directory(root.path()).expect("completed receipt authorizes ordinary use");
    assert!(!live_visible.exists());
    assert!(!live_anchor.exists());
}

#[cfg(any(unix, windows))]
#[test]
fn missing_legacy_directory_still_reconciles_live_and_dead_canonical_anchors() {
    let root = tempfile::tempdir().expect("root");
    let records = ensure_records_directory(root.path()).expect("records");
    let legacy_locks = records.join(".locks");
    assert!(!legacy_locks.exists(), "fixture has no legacy directory");
    let visible = legacy_locks.join("anchor-only.lock");
    let anchor = crate::daemons::state::daemon_lock_anchor_path(&visible).expect("legacy anchor");
    std::fs::write(&anchor, b"anchor-only").expect("anchor-only legacy lock");
    let owner = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&anchor)
        .expect("legacy anchor owner");
    owner.try_lock().expect("hold legacy anchor");

    let error = migrate_legacy_record_locks(
        root.path(),
        &records,
        Some(Instant::now() + Duration::from_secs(1)),
    )
    .expect_err("ordinary operation must reject an anchor introduced after the epoch");
    assert!(error.contains("confirm-no-legacy-processes"), "{error}");
    assert!(anchor.exists(), "live legacy anchor was preserved");
    assert!(!legacy_locks.exists(), "migration did not create .locks");

    let error = confirm_no_legacy_subagent_processes(root.path())
        .expect_err("explicit migration still rejects a live anchor owner");
    assert!(error.contains("still owned"), "{error}");

    drop(owner);
    assert_eq!(
        confirm_no_legacy_subagent_processes(root.path())
            .expect("fresh attestation cleans the dead anchor-only legacy lock"),
        1
    );
    assert!(!anchor.exists(), "dead legacy anchor was removed");
    assert!(!legacy_locks.exists(), "cleanup did not create .locks");
}

#[cfg(any(unix, windows))]
#[test]
fn missing_legacy_directory_retries_anchor_quarantine_without_creating_state() {
    let root = tempfile::tempdir().expect("root");
    let records = ensure_records_directory(root.path()).expect("records");
    let legacy_locks = records.join(".locks");
    let visible = legacy_locks.join("anchor-quarantine.lock");
    let anchor = crate::daemons::state::daemon_lock_anchor_path(&visible).expect("legacy anchor");
    std::fs::write(&anchor, b"anchor-quarantine").expect("legacy anchor");
    let anchor_directory =
        crate::daemons::state::StableDirectory::open(&records).expect("records directory");
    let quarantine = anchor_directory
        .deterministic_artifact_path(&anchor, ".nib-legacy-lock-delete-", ".quarantine")
        .expect("anchor quarantine");
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let deadline = Instant::now() + expiry_checkpoint_timeout();
    let worker_anchor = anchor.clone();
    let worker_quarantine = quarantine.clone();
    let worker = std::thread::spawn(move || {
        let mut paused = false;
        crate::daemons::state::cleanup_legacy_lock_pair_optional_with_guard(
            None,
            &visible,
            &anchor_directory,
            &worker_anchor,
            || {
                if !paused && worker_quarantine.exists() && !worker_anchor.exists() {
                    paused = true;
                    ready_tx.send(()).expect("publish anchor quarantine pause");
                    resume_rx.recv().expect("resume anchor cleanup");
                }
                ensure_subagent_reconciliation_deadline(Some(deadline))
            },
        )
    });
    ready_rx
        .recv_timeout(expiry_checkpoint_wait())
        .expect("anchor cleanup reached its quarantine boundary");
    std::thread::sleep(
        deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(200),
    );
    resume_tx.send(()).expect("resume expired anchor cleanup");
    let error = worker
        .join()
        .expect("anchor cleanup worker")
        .expect_err("expired cleanup retains anchor quarantine");
    assert!(error.contains("deadline elapsed"), "{error}");
    assert!(quarantine.exists());
    assert!(!anchor.exists());
    assert!(!legacy_locks.exists());

    assert_eq!(
        confirm_no_legacy_subagent_processes(root.path())
            .expect("fresh attestation removes retained anchor quarantine"),
        1
    );
    assert!(!anchor.exists());
    assert!(!quarantine.exists());
    assert!(!legacy_locks.exists(), "retry did not create .locks");
}

#[cfg(any(unix, windows))]
#[test]
fn missing_legacy_directory_preserves_ambiguous_anchor_quarantine() {
    let root = tempfile::tempdir().expect("root");
    let records = ensure_records_directory(root.path()).expect("records");
    let legacy_locks = records.join(".locks");
    let visible = legacy_locks.join("ambiguous-anchor.lock");
    let anchor = crate::daemons::state::daemon_lock_anchor_path(&visible).expect("legacy anchor");
    std::fs::write(&anchor, b"canonical-anchor").expect("canonical anchor");
    let directory =
        crate::daemons::state::StableDirectory::open(&records).expect("records directory");
    let quarantine = directory
        .deterministic_artifact_path(&anchor, ".nib-legacy-lock-delete-", ".quarantine")
        .expect("anchor quarantine");
    std::fs::write(&quarantine, b"mismatched-quarantine").expect("ambiguous anchor quarantine");

    let error = confirm_no_legacy_subagent_processes(root.path())
        .expect_err("mismatched anchor quarantine must fail closed");
    assert!(error.contains("different identities"), "{error}");
    assert_eq!(
        std::fs::read(&anchor).expect("canonical anchor"),
        b"canonical-anchor"
    );
    assert_eq!(
        std::fs::read(&quarantine).expect("ambiguous quarantine"),
        b"mismatched-quarantine"
    );
    assert!(!legacy_locks.exists(), "ambiguity did not create .locks");
}
