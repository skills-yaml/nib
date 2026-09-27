use super::*;

pub(crate) struct SpawnPreparationTimeoutGuard {
    pub(crate) previous: Option<Duration>,
    pub(crate) _not_send_or_sync: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl SpawnPreparationTimeoutGuard {
    pub(crate) fn set(timeout: Duration) -> Self {
        let previous = TEST_SPAWN_PREPARATION_OPERATION_TIMEOUT.with(|slot| {
            let previous = slot.get();
            slot.set(Some(timeout));
            previous
        });
        Self {
            previous,
            _not_send_or_sync: std::marker::PhantomData,
        }
    }
}

impl Drop for SpawnPreparationTimeoutGuard {
    fn drop(&mut self) {
        TEST_SPAWN_PREPARATION_OPERATION_TIMEOUT.with(|slot| slot.set(self.previous));
    }
}

pub(crate) struct SpawnReconciliationTimeoutGuard {
    pub(crate) previous: Option<Duration>,
    pub(crate) _not_send_or_sync: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl SpawnReconciliationTimeoutGuard {
    pub(crate) fn set(timeout: Duration) -> Self {
        let previous = TEST_SPAWN_RECONCILIATION_TIMEOUT.with(|slot| {
            let previous = slot.get();
            slot.set(Some(timeout));
            previous
        });
        Self {
            previous,
            _not_send_or_sync: std::marker::PhantomData,
        }
    }
}

impl Drop for SpawnReconciliationTimeoutGuard {
    fn drop(&mut self) {
        TEST_SPAWN_RECONCILIATION_TIMEOUT.with(|slot| slot.set(self.previous));
    }
}

pub(crate) struct SpawnAuthorityVerifyHookGuard;

impl SpawnAuthorityVerifyHookGuard {
    pub(crate) fn install(hook: SpawnAuthorityVerifyHook) -> Self {
        assert!(
            SPAWN_AUTHORITY_VERIFY_HOOK
                .lock()
                .expect("spawn authority verify hook lock")
                .replace(hook)
                .is_none(),
            "spawn authority verify hook already installed"
        );
        Self
    }
}

impl Drop for SpawnAuthorityVerifyHookGuard {
    fn drop(&mut self) {
        SPAWN_AUTHORITY_VERIFY_HOOK
            .lock()
            .expect("spawn authority verify hook lock")
            .take();
    }
}

pub(crate) struct SpawnHandoffPhaseHookGuard;

impl SpawnHandoffPhaseHookGuard {
    pub(crate) fn install(hook: impl FnMut(&'static str) + 'static) -> Self {
        SPAWN_HANDOFF_PHASE_HOOK.with(|slot| {
            assert!(
                slot.borrow_mut().replace(Box::new(hook)).is_none(),
                "spawn handoff hook already installed"
            );
        });
        Self
    }
}

impl Drop for SpawnHandoffPhaseHookGuard {
    fn drop(&mut self) {
        SPAWN_HANDOFF_PHASE_HOOK.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

pub(crate) const MERGE_LOCK_CHILD_PROJECT_ROOT: &str = "NIB_TEST_MERGE_LOCK_PROJECT_ROOT";
pub(crate) const MERGE_LOCK_CHILD_EXPECTATION: &str = "NIB_TEST_MERGE_LOCK_EXPECTATION";
pub(crate) const RECORD_WRITE_CHILD_PROJECT_ROOT: &str = "NIB_TEST_SUBAGENT_WRITE_PROJECT_ROOT";
pub(crate) const LEGACY_OPEN_CHILD_PROJECT_ROOT: &str = "NIB_TEST_LEGACY_OPEN_PROJECT_ROOT";
pub(crate) const LEGACY_OPEN_CHILD_READY: &str = "NIB_TEST_LEGACY_OPEN_READY";
pub(crate) const LEGACY_OPEN_CHILD_RESUME: &str = "NIB_TEST_LEGACY_OPEN_RESUME";
pub(crate) const LEGACY_OPEN_CHILD_LOCKED: &str = "NIB_TEST_LEGACY_OPEN_LOCKED";
pub(crate) const LEGACY_OPEN_CHILD_RELEASE: &str = "NIB_TEST_LEGACY_OPEN_RELEASE";
pub(crate) const LEGACY_OPEN_CHILD_ID: &str = "open-before-lock-contender";
pub(crate) const OWNER_LOSS_CHILD_PROJECT_ROOT: &str = "NIB_TEST_OWNER_LOSS_PROJECT_ROOT";
pub(crate) const OWNER_LOSS_CHILD_READY: &str = "NIB_TEST_OWNER_LOSS_READY";
pub(crate) const OWNER_LOSS_SUBAGENT_ID: &str = "sub-owner-process-loss";
pub(crate) const PREPARATION_CRASH_CHILD_PROJECT_ROOT: &str =
    "NIB_TEST_PREPARATION_CRASH_PROJECT_ROOT";
pub(crate) const HANDOFF_CRASH_CHILD_PROJECT_ROOT: &str = "NIB_TEST_HANDOFF_CRASH_PROJECT_ROOT";
pub(crate) const HANDOFF_CRASH_CHILD_PHASE: &str = "NIB_TEST_HANDOFF_CRASH_PHASE";
pub(crate) const HANDOFF_CRASH_CHILD_READY: &str = "NIB_TEST_HANDOFF_CRASH_READY";

pub(crate) fn subagent_namespace_snapshot(path: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn visit(root: &Path, current: &Path, snapshot: &mut Vec<(PathBuf, Vec<u8>)>) {
        let mut entries = std::fs::read_dir(current)
            .expect("read subagent namespace")
            .map(|entry| entry.expect("subagent namespace entry"))
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let entry_path = entry.path();
            let relative = entry_path
                .strip_prefix(root)
                .expect("subagent namespace entry is below root")
                .to_path_buf();
            if entry
                .file_type()
                .expect("subagent namespace entry type")
                .is_dir()
            {
                snapshot.push((relative, Vec::new()));
                visit(root, &entry_path, snapshot);
            } else {
                snapshot.push((
                    relative,
                    crate::fs_security::read_namespace_snapshot_file(&entry_path)
                        .expect("subagent namespace bytes"),
                ));
            }
        }
    }

    let mut snapshot = Vec::new();
    visit(path, path, &mut snapshot);
    snapshot
}

pub(crate) fn assert_spawn_cleanup_snapshot(
    before: &[(PathBuf, Vec<u8>)],
    after: &[(PathBuf, Vec<u8>)],
    context: &str,
) {
    let before_map = before
        .iter()
        .cloned()
        .collect::<std::collections::BTreeMap<_, _>>();
    let after_map = after
        .iter()
        .cloned()
        .collect::<std::collections::BTreeMap<_, _>>();
    let changed = before_map
        .keys()
        .chain(after_map.keys())
        .filter(|path| before_map.get(*path) != after_map.get(*path))
        .filter(|path| {
            let rendered = path.to_string_lossy().replace('\\', "/");
            let complete_worktree_proof = rendered.starts_with(".nib/worktree-ownership/")
                && rendered.ends_with(".json")
                && !before_map.contains_key(*path)
                && after_map.get(*path).is_some_and(|bytes| {
                    serde_json::from_slice::<Value>(bytes).is_ok_and(|record| {
                        record.get("phase").and_then(Value::as_str) == Some("complete")
                            && record.get("path_cleanup").and_then(Value::as_str) == Some("removed")
                            && record.get("registration_cleanup").and_then(Value::as_str)
                                == Some("removed")
                            && record.get("branch_cleanup").and_then(Value::as_str)
                                == Some("removed")
                    })
                });
            let expected_cleanup_artifact = rendered == ".nib/subagents/.preparations"
                || (rendered.starts_with(".nib/.subagent-record-stripe-")
                    && rendered.ends_with(".lock"))
                || (rendered
                    .starts_with(".git/nib/locks/.nib-lock-4-.nib-.subagent-record-stripe-")
                    && rendered.ends_with(".lock.anchor"))
                || complete_worktree_proof;
            !expected_cleanup_artifact
        })
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    assert!(changed.is_empty(), "{context}: {changed:?}");
}

pub(crate) fn assert_subagent_namespace_unchanged(
    before: &[(PathBuf, Vec<u8>)],
    after: &[(PathBuf, Vec<u8>)],
    context: &str,
) {
    let before = before
        .iter()
        .cloned()
        .collect::<std::collections::BTreeMap<_, _>>();
    let after = after
        .iter()
        .cloned()
        .collect::<std::collections::BTreeMap<_, _>>();
    let added = after
        .keys()
        .filter(|path| !before.contains_key(*path))
        .cloned()
        .collect::<Vec<_>>();
    let removed = before
        .keys()
        .filter(|path| !after.contains_key(*path))
        .cloned()
        .collect::<Vec<_>>();
    let changed = before
        .iter()
        .filter_map(|(path, bytes)| {
            after
                .get(path)
                .filter(|after| *after != bytes)
                .map(|_| path.clone())
        })
        .collect::<Vec<_>>();
    assert!(
        added.is_empty() && removed.is_empty() && changed.is_empty(),
        "{context}: added={added:?}, removed={removed:?}, changed={changed:?}"
    );
}

pub(crate) fn initialize_spawn_test_repository(root: &Path) {
    for args in [
        vec!["init", "--quiet"],
        vec!["config", "user.email", "nib-tests@example.invalid"],
        vec!["config", "user.name", "nib tests"],
    ] {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("run git fixture command");
        assert!(status.success());
    }
    std::fs::write(root.join("README.md"), b"spawn fixture\n").expect("fixture file");
    let status = std::process::Command::new("git")
        .args(["add", "README.md"])
        .current_dir(root)
        .status()
        .expect("stage fixture");
    assert!(status.success());
    let status = std::process::Command::new("git")
        .args(["commit", "--quiet", "-m", "fixture"])
        .current_dir(root)
        .status()
        .expect("commit fixture");
    assert!(status.success());
}

pub(crate) fn prime_fixed_subagent_record_lock_namespace(project_root: &Path) {
    let records = ensure_records_directory_capability_until(project_root, None)
        .expect("authorized records for fixed lock priming");
    let timeout = if cfg!(windows) {
        Duration::from_secs(15)
    } else {
        SUBAGENT_RECORD_LOCK_TIMEOUT
    };
    let deadline = Instant::now() + timeout;
    let _fence = acquire_spawn_preparation_fence_until(&records, deadline)
        .expect("global preparation fence for fixed lock priming");
    for stripe in 0..SUBAGENT_RECORD_LOCK_STRIPES {
        let path = record_lock_path_for_stripe(&records, stripe).expect("fixed stripe path");
        let lock =
            crate::daemons::state::acquire_file_lock_in_until_bound(&path, &records, deadline)
                .expect("prime fixed record stripe");
        lock.verify_until(deadline)
            .expect("verify primed fixed record stripe");
    }
}

pub(crate) fn directory_tree_snapshot(path: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn visit(root: &Path, current: &Path, snapshot: &mut Vec<(PathBuf, Vec<u8>)>) {
        let mut entries = std::fs::read_dir(current)
            .expect("read namespace tree")
            .map(|entry| entry.expect("namespace tree entry"))
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let entry_path = entry.path();
            let relative = entry_path
                .strip_prefix(root)
                .expect("namespace entry is below root")
                .to_path_buf();
            let file_type = entry.file_type().expect("namespace entry type");
            if file_type.is_dir() {
                snapshot.push((relative.clone(), Vec::new()));
                visit(root, &entry_path, snapshot);
            } else if file_type.is_symlink() {
                snapshot.push((
                    relative,
                    std::fs::read_link(entry_path)
                        .expect("namespace symlink target")
                        .as_os_str()
                        .as_encoded_bytes()
                        .to_vec(),
                ));
            } else {
                snapshot.push((
                    relative,
                    std::fs::read(entry_path).expect("namespace tree bytes"),
                ));
            }
        }
    }

    let mut snapshot = Vec::new();
    visit(path, path, &mut snapshot);
    snapshot
}

pub(crate) fn record_fixture(root: &Path, id: &str, status: &str) -> SubagentRecord {
    SubagentRecord {
        id: id.to_string(),
        parent_session_id: Some("parent".to_string()),
        child_session_id: format!("child-{id}"),
        prompt: "fixture".to_string(),
        status: status.to_string(),
        execution_generation: None,
        owner_lease: None,
        worktree_path: root.join("worktree"),
        branch: format!("nib/subagent/{id}"),
        branch_oid: None,
        result: None,
        error: None,
        verification: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

pub(crate) fn attach_execution_ownership(
    record: &mut SubagentRecord,
    owner_lease: &SubagentOwnerLease,
) {
    record.execution_generation = Some(owner_lease.execution_generation);
    record.owner_lease = Some(owner_lease.lease_id.clone());
}

pub(crate) fn remove_visible_owner_lease(owner_lease: &SubagentOwnerLease) {
    let owner_file = owner_lease.file.as_ref().expect("owner handle");
    owner_lease
        .visible_directory
        .as_ref()
        .expect("visible directory")
        .remove_file_if_matches(
            &owner_lease.visible_path,
            owner_file,
            ".nib-test-owner-visible-delete-",
        )
        .expect("remove visible owner lease");
}

pub(crate) fn install_completed_process_scope(
    root: &Path,
    id: &str,
    execution_generation: u64,
) -> crate::sandbox::process::CleanupProof {
    install_completed_process_scope_with_outcome(root, id, execution_generation, "fixture")
}

pub(crate) fn install_completed_process_scope_with_outcome(
    root: &Path,
    id: &str,
    execution_generation: u64,
    outcome: &str,
) -> crate::sandbox::process::CleanupProof {
    let store = crate::sandbox::process::ProcessScopeStore::open(root).expect("scope store");
    let identity = crate::sandbox::process::ProcessIdentity::current().expect("identity");
    let mut scope = store
        .prepare(
            id,
            "subagent",
            execution_generation,
            identity.clone(),
            crate::sandbox::process::ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepared scope");
    let proof = crate::sandbox::process::CleanupProof {
        execution_generation,
        cleanup_lease_id: scope.cleanup_lease_id.clone(),
        backend: scope.backend,
        direct_child: identity.clone(),
        outcome: outcome.to_string(),
        descendants_reaped: true,
        completed_at: Utc::now(),
    };
    scope.status = crate::sandbox::process::ProcessScopeStatus::Complete;
    scope.launch_committed = Some(true);
    scope.supervisor = Some(identity.clone());
    scope.direct_child = Some(identity);
    scope.cleanup_reason = Some(outcome.to_string());
    scope.cleanup_proof = Some(proof.clone());
    scope.updated_at = Utc::now();
    std::fs::write(
        root.join(".nib/process-scopes").join(format!("{id}.json")),
        serde_json::to_vec_pretty(&scope).expect("encode complete scope"),
    )
    .expect("write complete scope");
    proof
}

pub(crate) fn install_completed_launch_abort_scope(
    root: &Path,
    id: &str,
    execution_generation: u64,
) -> crate::sandbox::process::LaunchAbortProof {
    let store = crate::sandbox::process::ProcessScopeStore::open(root).expect("scope store");
    let identity = crate::sandbox::process::ProcessIdentity::current().expect("identity");
    let prepared = store
        .prepare(
            id,
            "subagent",
            execution_generation,
            identity.clone(),
            crate::sandbox::process::ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepared scope");
    let mut scope = store
        .register_launch_supervisor(
            id,
            execution_generation,
            &prepared.cleanup_lease_id,
            identity.clone(),
        )
        .expect("registered launch supervisor");
    let proof = crate::sandbox::process::LaunchAbortProof {
        execution_generation,
        cleanup_lease_id: scope.cleanup_lease_id.clone(),
        backend: scope.backend,
        supervisor: identity,
        namespace_root: None,
        outcome: "gate_eof_before_running".to_string(),
        workload_never_launched: true,
        completed_at: Utc::now(),
    };
    scope.status = crate::sandbox::process::ProcessScopeStatus::Complete;
    scope.cleanup_reason = Some(proof.outcome.clone());
    scope.launch_abort_proof = Some(proof.clone());
    scope.updated_at = Utc::now();
    std::fs::write(
        root.join(".nib/process-scopes").join(format!("{id}.json")),
        serde_json::to_vec_pretty(&scope).expect("encode launch-abort scope"),
    )
    .expect("write launch-abort scope");
    proof
}

pub(crate) fn install_completed_process_scope_with_live_lease(
    root: &Path,
    id: &str,
    execution_generation: u64,
) -> (
    crate::sandbox::process::CleanupProof,
    crate::sandbox::process::CleanupLease,
) {
    let store = crate::sandbox::process::ProcessScopeStore::open(root).expect("scope store");
    let identity = crate::sandbox::process::ProcessIdentity::current().expect("identity");
    let mut scope = store
        .prepare(
            id,
            "subagent",
            execution_generation,
            identity.clone(),
            crate::sandbox::process::ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepared scope");
    let lease = store
        .acquire_cleanup_lease(&scope)
        .expect("live cleanup lease");
    let proof = crate::sandbox::process::CleanupProof {
        execution_generation,
        cleanup_lease_id: scope.cleanup_lease_id.clone(),
        backend: scope.backend,
        direct_child: identity.clone(),
        outcome: "fixture".to_string(),
        descendants_reaped: true,
        completed_at: Utc::now(),
    };
    scope.status = crate::sandbox::process::ProcessScopeStatus::Complete;
    scope.launch_committed = Some(true);
    scope.supervisor = Some(identity.clone());
    scope.direct_child = Some(identity);
    scope.cleanup_reason = Some("fixture".to_string());
    scope.cleanup_proof = Some(proof.clone());
    scope.updated_at = Utc::now();
    std::fs::write(
        root.join(".nib/process-scopes").join(format!("{id}.json")),
        serde_json::to_vec_pretty(&scope).expect("encode complete scope"),
    )
    .expect("write complete scope");
    (proof, lease)
}

pub(crate) fn terminal_record_fixture(
    root: &Path,
    id: &str,
    execution_generation: u64,
    owner_lease: &str,
    proof: &crate::sandbox::process::CleanupProof,
) -> SubagentRecord {
    let mut record = record_fixture(root, id, "completed");
    record.execution_generation = Some(execution_generation);
    record.owner_lease = Some(owner_lease.to_string());
    record.result = Some(json!({
        "cleanup_verified": true,
        "cleanup_proof": proof,
    }));
    record
}

pub(crate) fn launch_abort_terminal_record_fixture(
    root: &Path,
    id: &str,
    execution_generation: u64,
    owner_lease: &str,
    proof: &crate::sandbox::process::LaunchAbortProof,
) -> SubagentRecord {
    let mut record = record_fixture(root, id, "failed");
    record.execution_generation = Some(execution_generation);
    record.owner_lease = Some(owner_lease.to_string());
    record.result = Some(json!({
        "cleanup_verified": false,
        "launch_abort_verified": true,
        "workload_never_launched": true,
        "launch_abort_proof": proof,
    }));
    record
}

#[cfg(any(unix, windows))]
pub(crate) fn assert_post_scan_live_legacy_publication_is_rejected(anchor_only: bool) {
    let root = tempfile::tempdir().expect("root");
    let records = ensure_records_directory(root.path()).expect("records");
    let legacy_locks = records.join(".locks");
    let id = if anchor_only {
        "post-scan-live-anchor"
    } else {
        "post-scan-live-pair"
    };
    let visible = legacy_locks.join(format!("{id}.lock"));
    let anchor = crate::daemons::state::daemon_lock_anchor_path(&visible).expect("legacy anchor");
    let mut owner = None;

    let error = confirm_no_legacy_subagent_processes_with_scan_hook(root.path(), |pass| {
        if pass != 0 {
            return Ok(());
        }
        if !anchor_only {
            std::fs::create_dir(&legacy_locks)
                .map_err(|error| format!("create legacy directory: {error}"))?;
            std::fs::write(&visible, b"post-scan-live")
                .map_err(|error| format!("write legacy lock: {error}"))?;
            std::fs::hard_link(&visible, &anchor)
                .map_err(|error| format!("link legacy anchor: {error}"))?;
        } else {
            std::fs::write(&anchor, b"post-scan-live-anchor")
                .map_err(|error| format!("write legacy anchor: {error}"))?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&anchor)
            .map_err(|error| format!("open legacy owner: {error}"))?;
        file.try_lock()
            .map_err(|error| format!("hold legacy owner: {error}"))?;
        owner = Some(file);
        Ok(())
    })
    .expect_err("a live legacy publication after the initial scan must fail closed");

    assert!(error.contains("fresh operator confirmation"), "{error}");
    assert!(anchor.exists(), "live legacy anchor was preserved");
    if anchor_only {
        assert!(
            !legacy_locks.exists(),
            "anchor-only scan did not create .locks"
        );
    } else {
        assert!(visible.exists(), "live legacy lock was preserved");
    }

    drop(owner.take());
    confirm_no_legacy_subagent_processes(root.path())
        .expect("a fresh attestation cleans the released legacy publication");
    assert!(!visible.exists(), "released legacy lock was removed");
    assert!(!anchor.exists(), "released legacy anchor was removed");
    if anchor_only {
        assert!(
            !legacy_locks.exists(),
            "anchor-only retry did not create .locks"
        );
    }
}

#[cfg(any(unix, windows))]
pub(crate) fn sweep_owner_quarantine_expiry_fixture(half: Option<bool>) {
    let root = tempfile::tempdir().expect("root");
    let lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
    let visible = lease.visible_path.clone();
    let anchor = lease.anchor_path.clone();
    drop(lease);
    let anchor_only = half == Some(true);
    let visible_only = half == Some(false);
    if anchor_only {
        std::fs::remove_file(&visible).expect("create anchor-only sweep fixture");
    } else if visible_only {
        std::fs::remove_file(&anchor).expect("create visible-only sweep fixture");
    }
    let source = if anchor_only {
        anchor.clone()
    } else {
        visible.clone()
    };
    let source_bytes = std::fs::read(&source).expect("sweep source bytes");
    let paired = half
        .is_none()
        .then(|| std::fs::read(&anchor).expect("paired sweep anchor"));
    let directory =
        crate::daemons::state::StableDirectory::open(source.parent().expect("sweep source parent"))
            .expect("sweep source directory");
    let quarantine_prefix = if anchor_only {
        ".nib-subagent-owner-anchor-delete-"
    } else {
        ".nib-subagent-owner-visible-delete-"
    };
    let quarantine = directory
        .deterministic_artifact_path(&source, quarantine_prefix, ".quarantine")
        .expect("sweep quarantine");
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let project_root = root.path().to_path_buf();
    let worker_source = source.clone();
    let worker_quarantine = quarantine.clone();
    let worker = std::thread::spawn(move || {
        let mut paused = false;
        sweep_owner_lease_artifacts_with_timeout_and_guard(
            &project_root,
            &std::collections::HashSet::new(),
            Duration::from_millis(150),
            || {
                if !paused && worker_quarantine.exists() && !worker_source.exists() {
                    paused = true;
                    ready_tx.send(()).expect("publish sweep quarantine pause");
                    resume_rx.recv().expect("resume owner sweep");
                }
                Ok(())
            },
        )
    });

    ready_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("sweep reached its final deletion boundary");
    let quarantined = source_bytes;
    std::thread::sleep(Duration::from_millis(200));
    resume_tx.send(()).expect("resume expired owner sweep");
    let error = worker
        .join()
        .expect("owner sweep worker")
        .expect_err("expired owner sweep must fail closed");
    assert!(
        error.contains("timed out acquiring delegation state lock"),
        "{error}"
    );
    assert_eq!(
        std::fs::read(&quarantine).expect("retained sweep quarantine"),
        quarantined
    );
    if let Some(paired) = paired {
        assert_eq!(
            std::fs::read(&anchor).expect("retained sweep anchor"),
            paired
        );
    }
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        std::fs::read(&quarantine).expect("later sweep quarantine"),
        quarantined
    );
    sweep_owner_lease_artifacts_with_timeout_and_guard(
        root.path(),
        &std::collections::HashSet::new(),
        Duration::from_secs(2),
        || Ok(()),
    )
    .expect("fresh sweep finishes retained quarantine deletion");
    assert!(!visible.exists(), "fresh sweep removed visible owner state");
    assert!(!anchor.exists(), "fresh sweep removed anchor owner state");
    assert!(
        !quarantine.exists(),
        "fresh sweep removed the retained quarantine"
    );
}

#[cfg(any(unix, windows))]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn owner_cleanup_quarantine_expiry_fixture(half: Option<bool>) {
    let root = tempfile::tempdir().expect("root");
    let lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
    let execution_generation = lease.execution_generation;
    let lease_id = lease.lease_id.clone();
    let visible = lease.visible_path.clone();
    let anchor = lease.anchor_path.clone();
    drop(lease);
    let anchor_only = half == Some(true);
    let visible_only = half == Some(false);
    if anchor_only {
        std::fs::remove_file(&visible).expect("create anchor-only fixture");
    } else if visible_only {
        std::fs::remove_file(&anchor).expect("create visible-only fixture");
    }
    let source = if anchor_only {
        anchor.clone()
    } else {
        visible.clone()
    };
    let source_bytes = std::fs::read(&source).expect("owner source bytes");
    let paired = half
        .is_none()
        .then(|| std::fs::read(&anchor).expect("paired anchor bytes"));
    let directory = crate::daemons::state::StableDirectory::open(
        source.parent().expect("owner artifact parent"),
    )
    .expect("owner artifact directory");
    let quarantine_prefix = if anchor_only {
        ".nib-subagent-owner-anchor-delete-"
    } else {
        ".nib-subagent-owner-visible-delete-"
    };
    let quarantine = directory
        .deterministic_artifact_path(&source, quarantine_prefix, ".quarantine")
        .expect("owner deletion quarantine");
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let project_root = root.path().to_path_buf();
    let cleanup_quarantine = quarantine.clone();
    let cleanup_source = source.clone();
    let worker_lease_id = lease_id.clone();
    let worker = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_millis(150);
        let mut paused = false;
        remove_persisted_owner_lease_until_with_guard(
            &project_root,
            execution_generation,
            &worker_lease_id,
            Some(deadline),
            || {
                if !paused && cleanup_quarantine.exists() && !cleanup_source.exists() {
                    paused = true;
                    ready_tx.send(()).expect("publish owner quarantine pause");
                    resume_rx.recv().expect("resume owner cleanup");
                }
                Ok(())
            },
        )
    });

    ready_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("owner cleanup reached its final quarantine");
    assert!(!source.exists(), "owner source was quarantined");
    assert!(quarantine.is_file(), "recoverable quarantine is retained");
    if half.is_none() {
        assert!(anchor.is_file(), "the paired anchor remains authoritative");
    }
    std::thread::sleep(Duration::from_millis(200));
    let quarantined = source_bytes;
    resume_tx.send(()).expect("resume expired owner cleanup");

    let error = worker
        .join()
        .expect("owner cleanup worker")
        .expect_err("expired owner cleanup must fail closed");
    assert!(error.contains("deadline elapsed"), "{error}");
    assert_eq!(
        std::fs::read(&quarantine).expect("retained owner quarantine"),
        quarantined
    );
    if let Some(paired) = paired {
        assert_eq!(
            std::fs::read(&anchor).expect("retained paired anchor"),
            paired
        );
    }
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        std::fs::read(&quarantine).expect("later retained owner quarantine"),
        quarantined,
        "expired cleanup mutated its recoverable quarantine after returning"
    );
    remove_persisted_owner_lease_until(
        root.path(),
        execution_generation,
        &lease_id,
        Some(Instant::now() + Duration::from_secs(2)),
    )
    .expect("fresh owner cleanup finishes retained quarantine deletion");
    assert!(
        !visible.exists(),
        "fresh cleanup removes visible owner state"
    );
    assert!(!anchor.exists(), "fresh cleanup removes anchor owner state");
    assert!(
        !quarantine.exists(),
        "fresh cleanup removes the retained owner quarantine"
    );
}

#[path = "test_overflow.rs"]
mod test_overflow;
#[path = "test_part_0.rs"]
mod test_part_0;
#[path = "test_part_1.rs"]
mod test_part_1;
#[path = "test_part_2.rs"]
mod test_part_2;
#[path = "test_part_3.rs"]
mod test_part_3;
