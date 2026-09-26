use super::*;

#[test]
fn preexpired_open_does_not_create_managed_process_namespace() {
    let root = tempfile::tempdir().expect("temp project");

    let error = ProcessScopeStore::open_with_lock_deadline(
        root.path(),
        Instant::now() - Duration::from_millis(1),
    )
    .expect_err("an expired open must fail before namespace creation");

    assert!(error.contains("managed-process scope lock deadline elapsed"));
    assert!(
        !root.path().join(".nib").exists(),
        "expired open created managed-process state"
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn supervisor_self_registration_is_exact_idempotent_and_late_cas_fails() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let owner = ProcessIdentity::current().expect("owner identity");
    let cleanup_lease_id = uuid::Uuid::new_v4().to_string();
    let nonce = uuid::Uuid::new_v4().to_string();
    #[cfg(target_os = "linux")]
    let backend = ProcessScopeBackend::LinuxPidNamespace;
    #[cfg(windows)]
    let backend = ProcessScopeBackend::WindowsJobObject;
    #[cfg(target_os = "macos")]
    let backend = ProcessScopeBackend::MacosProcessGroup;
    let prepared = store
        .prepare_subagent_launch(
            "sub-self-register",
            41,
            &cleanup_lease_id,
            &nonce,
            owner.clone(),
            backend,
        )
        .expect("preplanned scope");
    let supervisor = ProcessIdentity::current().expect("supervisor identity");
    assert!(store
        .observe_registered_launch_supervisor(
            &prepared.scope_id,
            prepared.execution_generation,
            &prepared.cleanup_lease_id,
            &nonce,
            &supervisor,
        )
        .expect("unregistered observation")
        .is_none());
    let registered = store
        .self_register_launch_supervisor(
            &prepared.scope_id,
            prepared.execution_generation,
            &prepared.cleanup_lease_id,
            &nonce,
            supervisor.clone(),
        )
        .expect("self registration");
    assert_eq!(registered.supervisor.as_ref(), Some(&supervisor));
    assert_eq!(
        store
            .self_register_launch_supervisor(
                &prepared.scope_id,
                prepared.execution_generation,
                &prepared.cleanup_lease_id,
                &nonce,
                supervisor.clone(),
            )
            .expect("idempotent self registration"),
        registered
    );
    assert_eq!(
        store
            .observe_registered_launch_supervisor(
                &prepared.scope_id,
                prepared.execution_generation,
                &prepared.cleanup_lease_id,
                &nonce,
                &supervisor,
            )
            .expect("registered observation"),
        Some(registered.clone())
    );
    let mismatch = ProcessIdentity {
        pid: supervisor.pid,
        start_marker: format!("{}-mismatch", supervisor.start_marker),
    };
    let error = store
        .self_register_launch_supervisor(
            &prepared.scope_id,
            prepared.execution_generation,
            &prepared.cleanup_lease_id,
            &nonce,
            mismatch,
        )
        .expect_err("second supervisor identity must fail closed");
    assert!(error.contains("another launch supervisor"), "{error}");
    let error = store
        .observe_registered_launch_supervisor(
            &prepared.scope_id,
            prepared.execution_generation,
            &prepared.cleanup_lease_id,
            &uuid::Uuid::new_v4().to_string(),
            &supervisor,
        )
        .expect_err("wrong registration nonce must fail closed");
    assert!(error.contains("exact prepared launch"), "{error}");

    let late_cleanup = uuid::Uuid::new_v4().to_string();
    let late_nonce = uuid::Uuid::new_v4().to_string();
    let late = store
        .prepare_subagent_launch(
            "sub-late-self-register",
            42,
            &late_cleanup,
            &late_nonce,
            owner,
            backend,
        )
        .expect("late scope");
    store
        .remove_prepared(&late)
        .expect("restart wins exact removal");
    let error = store
        .self_register_launch_supervisor(
            &late.scope_id,
            late.execution_generation,
            &late.cleanup_lease_id,
            &late_nonce,
            supervisor,
        )
        .expect_err("late supervisor CAS must fail after retirement");
    assert!(
        error.contains("failed to open") || error.contains("No such file"),
        "{error}"
    );
    assert!(store
        .try_load(&late.scope_id)
        .expect("late scope lookup")
        .is_none());
}

#[test]
fn bounded_process_store_setup_retries_directory_and_lock_finalization() {
    let root = git_project();
    let scope_directory = root.path().join(".nib").join(SCOPE_DIRECTORY);
    // Leave enough setup headroom for a loaded hosted filesystem; the hook
    // deterministically expires the same absolute deadline once this phase exists.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut paused_after_scope_create = false;
    let error =
        ProcessScopeStore::open_with_lock_deadline_and_setup_hook(root.path(), deadline, || {
            if scope_directory.is_dir() && !paused_after_scope_create {
                paused_after_scope_create = true;
                thread::sleep(deadline.saturating_duration_since(Instant::now()));
            }
            Ok(())
        })
        .expect_err("expiry after process-scope directory create must win");
    assert!(paused_after_scope_create);
    assert!(
        error.contains("timed out acquiring daemon state lock")
            || error.contains("managed-process scope lock deadline elapsed"),
        "{error}"
    );
    assert!(
        scope_directory.is_dir(),
        "exact scope directory is retained"
    );

    ProcessScopeStore::open_with_lock_deadline(
        root.path(),
        Instant::now() + Duration::from_secs(2),
    )
    .expect("fresh deadline finalizes the retained process directory");

    let lock_root = git_project();
    let protected = lock_root.path().join(".nib").join(SCOPE_DIRECTORY);
    std::fs::create_dir(&protected).expect("process scope directory");
    let lock_path = lock_root.path().join(".nib").join(SCOPE_STORE_LOCK);
    let anchor_path =
        crate::daemons::state::daemon_lock_anchor_path(&lock_path).expect("process lock anchor");
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut paused_after_anchor_link = false;
    let error = ProcessScopeStore::open_with_lock_deadline_and_setup_hook(
        lock_root.path(),
        deadline,
        || {
            if lock_path.exists() && anchor_path.exists() && !paused_after_anchor_link {
                paused_after_anchor_link = true;
                thread::sleep(deadline.saturating_duration_since(Instant::now()));
            }
            Ok(())
        },
    )
    .expect_err("expiry after process lock anchor publication must win");
    assert!(paused_after_anchor_link);
    assert!(
        error.contains("timed out acquiring daemon state lock")
            || error.contains("managed-process scope lock deadline elapsed"),
        "{error}"
    );
    let visible = std::fs::File::open(&lock_path).expect("recoverable process lock");
    let anchor = std::fs::File::open(&anchor_path).expect("recoverable process anchor");
    assert!(
        crate::daemons::state::same_open_file_identity(&visible, &anchor)
            .expect("same process lock identity")
    );

    ProcessScopeStore::open_with_lock_deadline(
        lock_root.path(),
        Instant::now() + Duration::from_secs(2),
    )
    .expect("fresh deadline repairs and finalizes the process lock");
    assert!(
        !anchor_path.exists(),
        "successful process-store retry cleans its transient anchor"
    );
}

#[test]
fn preexpired_maintenance_rejects_a_cached_store() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    assert!(
        MAINTAINED_PROCESS_SCOPE_STORES
            .lock()
            .expect("maintenance registry")
            .contains(&store.directory),
        "fixture store was not cached"
    );
    let bounded = ProcessScopeStore {
        lock_deadline: Some(Instant::now() - Duration::from_millis(1)),
        ..store
    };

    let error = bounded
        .maintain_once()
        .expect_err("an expired cache hit must not report maintenance success");

    assert!(error.contains("managed-process scope lock deadline elapsed"));
}

#[test]
fn held_maintenance_registry_is_bounded_without_late_mutation() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let sentinel = store.directory.join("held-registry-sentinel");
    std::fs::write(&sentinel, b"preserve").expect("sentinel");
    let registry: ProcessScopeMaintenanceRegistry = Mutex::new(Default::default());
    let held_registry = registry.lock().expect("hold maintenance registry");
    let budget = Duration::from_millis(75);
    let bounded = ProcessScopeStore {
        lock_deadline: Some(Instant::now() + budget),
        ..store
    };
    let started = Instant::now();

    let error = bounded
        .maintain_once_in(&registry)
        .expect_err("maintenance registry contention must obey the deadline");

    assert!(error.contains("managed-process scope lock deadline elapsed"));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(held_registry.is_empty(), "contended registry was mutated");
    assert_eq!(
        std::fs::read(&sentinel).expect("sentinel remains"),
        b"preserve"
    );
    thread::sleep(budget * 2);
    assert_eq!(
        std::fs::read(&sentinel).expect("sentinel remains after grace period"),
        b"preserve",
        "deadline-aware maintenance mutated state after returning"
    );
}

#[test]
fn scope_publication_rechecks_deadline_at_the_commit_boundary() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let record = store
        .prepare(
            "sub-expired-commit",
            "subagent",
            71,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("scope fixture");
    let path = store.record_path(&record.scope_id).expect("scope path");
    let original = std::fs::read(&path).expect("original scope bytes");
    // Coverage instrumentation can spend longer than 40 ms reaching the
    // precommit hook. Keep the assertion about expiry at that hook.
    let deadline = Instant::now() + Duration::from_secs(2);
    let bounded = ProcessScopeStore {
        project_root: store.project_root.clone(),
        directory: store.directory.clone(),
        directory_capability: store
            .directory_capability
            .try_clone()
            .expect("clone scope capability"),
        records_binding: None,
        lock_deadline: Some(deadline),
        operation_timeout: SCOPE_LOCK_TIMEOUT,
    };
    let mut paused_namespace = None;

    let error = bounded
        .mutate_with_commit_check(
            &record.scope_id,
            record.execution_generation,
            &record.cleanup_lease_id,
            |scope| {
                scope.cleanup_reason = Some("must not publish".to_string());
                Ok(())
            },
            || {
                paused_namespace = Some(process_namespace_snapshot(&store.directory));
                while Instant::now() < deadline {
                    thread::yield_now();
                }
                Ok(())
            },
        )
        .expect_err("an expired precommit must reject the scope mutation");

    assert!(
        error.contains("timed out acquiring daemon state lock"),
        "{error}"
    );
    assert_eq!(
        std::fs::read(&path).expect("unchanged scope bytes"),
        original
    );
    let paused_namespace = paused_namespace.expect("captured precommit namespace");
    assert_eq!(
        process_namespace_snapshot(&store.directory),
        paused_namespace,
        "expired scope cleanup mutated transaction artifacts"
    );
    thread::sleep(Duration::from_millis(50));
    assert_eq!(
        process_namespace_snapshot(&store.directory),
        paused_namespace,
        "expired scope cleanup mutated transaction artifacts later"
    );
}

#[test]
fn long_lived_scope_mutation_uses_one_deadline_through_precommit() {
    let root = git_project();
    let mut store = ProcessScopeStore::open(root.path()).expect("scope store");
    let record = store
        .prepare(
            "sub-long-lived-precommit",
            "subagent",
            711,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("scope fixture");
    store.operation_timeout = Duration::from_millis(60);
    let path = store.record_path(&record.scope_id).expect("scope path");
    let original = std::fs::read(&path).expect("original scope bytes");
    let namespace = process_namespace_snapshot(&store.directory);

    let error = store
        .mutate_with_commit_check(
            &record.scope_id,
            record.execution_generation,
            &record.cleanup_lease_id,
            |scope| {
                scope.cleanup_reason = Some("must not publish late".to_string());
                Ok(())
            },
            || {
                thread::sleep(Duration::from_millis(90));
                Ok(())
            },
        )
        .expect_err("long-lived mutation must not renew its precommit deadline");

    assert!(
        error.contains("timed out acquiring daemon state lock"),
        "{error}"
    );
    assert_eq!(std::fs::read(&path).expect("scope bytes"), original);
    assert_eq!(
        process_namespace_snapshot(&store.directory),
        namespace,
        "expired long-lived mutation changed the process namespace"
    );
}

#[test]
fn long_lived_scope_atomic_recovery_obeys_the_outer_deadline() {
    let root = git_project();
    let mut store = ProcessScopeStore::open(root.path()).expect("scope store");
    let record = store
        .prepare(
            "sub-long-lived-recovery",
            "subagent",
            712,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("scope fixture");
    store.operation_timeout = Duration::from_millis(60);
    let path = store.record_path(&record.scope_id).expect("scope path");
    let previous = store
        .directory_capability
        .deterministic_previous_artifact_path(&path, SCOPE_WRITE_PREFIX)
        .expect("previous path");
    std::fs::rename(&path, &previous).expect("install recoverable previous state");
    let namespace = process_namespace_snapshot(&store.directory);
    let deadline = store
        .effective_operation_deadline()
        .expect("operation deadline");
    let mut paused = false;

    let error = store
        .with_scope_lock_until_and_recovery_hook(
            &record.scope_id,
            deadline,
            |_directory, _path, _deadline| Ok(()),
            || {
                if !paused {
                    paused = true;
                    thread::sleep(Duration::from_millis(90));
                }
                Ok(())
            },
        )
        .expect_err("nested recovery must use the outer operation deadline");

    assert!(
        error.contains("timed out acquiring daemon state lock"),
        "{error}"
    );
    assert_eq!(
        process_namespace_snapshot(&store.directory),
        namespace,
        "expired nested recovery changed its transaction namespace"
    );
    assert!(!path.exists());
    assert!(previous.is_file());
    store.operation_timeout = SCOPE_LOCK_TIMEOUT;
    assert_eq!(
        store.load(&record.scope_id).expect("fresh recovery retry"),
        record
    );
    assert!(path.is_file());
    assert!(!previous.exists());
}

#[test]
fn preexpired_cleanup_lease_publication_and_cold_maintenance_do_not_mutate() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let record = store
        .prepare(
            "sub-expired-lease",
            "subagent",
            72,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("scope fixture");
    let lease_path = store
        .cleanup_lease_path(&record.scope_id)
        .expect("lease path");
    let stale = store
        .directory
        .join(format!("{SCOPE_WRITE_PREFIX}{}-stale.tmp", record.scope_id));
    std::fs::write(&stale, b"preserve stale maintenance fixture").expect("stale fixture");
    let bounded = ProcessScopeStore {
        project_root: store.project_root.clone(),
        directory: store.directory.clone(),
        directory_capability: store
            .directory_capability
            .try_clone()
            .expect("clone scope capability"),
        records_binding: None,
        lock_deadline: Some(Instant::now() - Duration::from_millis(1)),
        operation_timeout: SCOPE_LOCK_TIMEOUT,
    };

    let lease_error = bounded
        .acquire_cleanup_lease(&record)
        .err()
        .expect("expired cleanup lease publication must fail");
    assert!(lease_error.contains("managed-process scope lock deadline elapsed"));
    assert!(!lease_path.exists(), "expired cleanup lease was published");

    let registry: ProcessScopeMaintenanceRegistry = Mutex::new(Default::default());
    let maintenance_error = bounded
        .maintain_once_in(&registry)
        .expect_err("expired cold maintenance must fail");
    assert!(maintenance_error.contains("managed-process scope lock deadline elapsed"));
    assert_eq!(
        std::fs::read(&stale).expect("stale fixture remains"),
        b"preserve stale maintenance fixture"
    );
    assert!(
        registry.lock().expect("maintenance registry").is_empty(),
        "expired maintenance cached the store"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn managed_process_probe_retries_a_transient_cleaned_launch_failure() {
    let mut attempts = 0;
    run_linux_managed_process_probe_attempts(|| {
        attempts += 1;
        if attempts == 1 {
            Err("supervised Linux launch gate closed before reporting readiness".to_string())
        } else {
            Ok(())
        }
    })
    .expect("transient cleaned probe failure is retried");
    assert_eq!(attempts, 2);
}

#[cfg(target_os = "linux")]
#[test]
fn managed_process_probe_retry_budget_preserves_every_failure() {
    let mut attempts = 0;
    let error = run_linux_managed_process_probe_attempts(|| {
        attempts += 1;
        Err(format!(
            "supervised Linux launch gate readiness timed out ({attempts})"
        ))
    })
    .expect_err("repeated cleaned probe failures exhaust the retry budget");
    assert_eq!(attempts, LINUX_MANAGED_PROCESS_PROBE_ATTEMPTS);
    for attempt in 1..=LINUX_MANAGED_PROCESS_PROBE_ATTEMPTS {
        assert!(error.contains(&format!("attempt {attempt}:")), "{error}");
        assert!(error.contains(&format!("timed out ({attempt})")), "{error}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn managed_process_probe_does_not_retry_unproven_cleanup() {
    let mut attempts = 0;
    let error = run_linux_managed_process_probe_attempts(|| {
            attempts += 1;
            Err(
                "supervised Linux launch gate closed before reporting readiness; supervised launch cleanup was not proven: namespace survived"
                    .to_string(),
            )
        })
        .expect_err("unproven cleanup must fail immediately");
    assert_eq!(attempts, 1);
    assert!(error.contains("after 1 attempt(s)"), "{error}");
    assert!(error.contains("cleanup was not proven"), "{error}");
}

#[cfg(target_os = "linux")]
#[test]
fn managed_process_probe_does_not_retry_diagnosed_monitor_exit() {
    let mut attempts = 0;
    let error = run_linux_managed_process_probe_attempts(|| {
            attempts += 1;
            Err(
                "supervised Linux launch gate closed before reporting readiness; bubblewrap monitor status: exit status: 1; bubblewrap stderr: mount denied"
                    .to_string(),
            )
        })
        .expect_err("diagnosed monitor exit must not be retried");
    assert_eq!(attempts, 1);
    assert!(error.contains("mount denied"), "{error}");
}

#[cfg(target_os = "linux")]
#[test]
fn failed_linux_launch_diagnostics_capture_status_and_stderr() {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "printf 'mount denied' >&2; exit 42"])
        .stderr(Stdio::piped())
        .spawn()
        .expect("diagnostic fixture");
    child.wait().expect("diagnostic fixture exit");

    let diagnostic = append_linux_launch_diagnostics("launch failed".to_string(), &mut child);

    assert!(
        diagnostic.contains("bubblewrap monitor status:"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("42"), "{diagnostic}");
    assert!(
        diagnostic.contains("bubblewrap stderr: mount denied"),
        "{diagnostic}"
    );
}

#[test]
fn process_identity_rejects_pid_reuse_markers() {
    let identity = ProcessIdentity::current().expect("current identity");
    assert!(identity.still_matches());
    let mut forged = identity;
    forged.start_marker.push_str("-reused");
    assert!(!forged.still_matches());
}

#[test]
fn scope_transitions_require_exact_generation_and_cleanup_proof() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let owner = ProcessIdentity::current().expect("owner identity");
    let record = store
        .prepare(
            "sub-test",
            "subagent",
            41,
            owner.clone(),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare scope");
    let lease = store.acquire_cleanup_lease(&record).expect("cleanup lease");
    let error = store
        .mutate(
            &record.scope_id,
            record.execution_generation,
            &record.cleanup_lease_id,
            |record| {
                record.status = ProcessScopeStatus::CleanupInProgress;
                Ok(())
            },
        )
        .expect_err("central transition validator rejects prepared cleanup");
    assert!(error.contains("not a legal monotonic successor"), "{error}");
    let error = store
        .begin_cleanup(
            &record.scope_id,
            record.execution_generation,
            &record.cleanup_lease_id,
            "not-running",
        )
        .expect_err("prepared scope cannot enter cleanup");
    assert!(error.contains("cannot begin cleanup from status Prepared"));
    assert_eq!(
        store
            .load(&record.scope_id)
            .expect("prepared scope remains"),
        record
    );
    let child = owner;
    let running = store
        .mark_running(
            &record.scope_id,
            record.execution_generation,
            &record.cleanup_lease_id,
            ProcessIdentity::current().expect("supervisor identity"),
            child,
        )
        .expect("mark running");
    assert_eq!(running.status, ProcessScopeStatus::Running);
    assert!(store
        .begin_cleanup(
            &record.scope_id,
            record.execution_generation + 1,
            &record.cleanup_lease_id,
            "stale",
        )
        .is_err());
    store
        .begin_cleanup(
            &record.scope_id,
            record.execution_generation,
            &record.cleanup_lease_id,
            "owner_eof",
        )
        .expect("begin cleanup");
    assert!(store
        .complete_cleanup(
            &record.scope_id,
            record.execution_generation,
            &record.cleanup_lease_id,
            "owner_eof",
            false,
        )
        .is_err());
    let complete = store
        .complete_cleanup(
            &record.scope_id,
            record.execution_generation,
            &record.cleanup_lease_id,
            "owner_eof",
            true,
        )
        .expect("complete cleanup");
    let proof = complete.cleanup_proof.as_ref().expect("cleanup proof");
    lease.release_after_proof(proof).expect("release lease");
    assert_eq!(store.load("sub-test").expect("reload"), complete);
}

#[test]
fn legacy_prepared_mark_running_is_one_successful_committed_transition() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let prepared = store
        .prepare(
            "sub-legacy-running",
            "subagent",
            411,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare scope");
    let path = store.record_path(&prepared.scope_id).expect("scope path");
    let mut legacy = prepared.clone();
    legacy.launch_committed = None;
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&legacy).expect("encode legacy scope"),
    )
    .expect("install legacy v2 scope");
    let supervisor = ProcessIdentity::current().expect("supervisor identity");
    let direct_child = supervisor.clone();

    let running = store
        .mark_running(
            &legacy.scope_id,
            legacy.execution_generation,
            &legacy.cleanup_lease_id,
            supervisor.clone(),
            direct_child.clone(),
        )
        .expect("legacy mark-running succeeds atomically");

    assert_eq!(running.status, ProcessScopeStatus::Running);
    assert_eq!(running.launch_committed, Some(true));
    assert_eq!(running.supervisor, Some(supervisor));
    assert_eq!(running.direct_child, Some(direct_child));
    assert_eq!(
        store
            .load(&legacy.scope_id)
            .expect("persisted running scope"),
        running,
        "successful return and durable state must be the same transition"
    );
}

#[test]
fn load_rejects_a_scope_whose_filename_key_does_not_match_its_id() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let record = store
        .prepare(
            "sub-canonical-key",
            "subagent",
            412,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare scope");
    let source = store.record_path(&record.scope_id).expect("scope path");
    let mismatched = store
        .record_path("sub-mismatched-key")
        .expect("mismatched scope path");
    std::fs::copy(source, mismatched).expect("install mismatched-key scope");

    let error = store
        .load("sub-mismatched-key")
        .expect_err("scope filename/id mismatch must fail closed");

    assert!(error.contains("mismatched key"), "{error}");
}

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn cleanup_lease_final_delete_preserves_quarantine_after_deadline_expiry() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let identity = ProcessIdentity::current().expect("process identity");
    let prepared = store
        .prepare(
            "sub-cleanup-lease-deadline",
            "subagent",
            42,
            identity.clone(),
            ProcessScopeBackend::current().expect("process backend"),
        )
        .expect("prepare scope");
    let initial_lease = store
        .acquire_cleanup_lease(&prepared)
        .expect("initial cleanup lease");
    store
        .mark_running(
            &prepared.scope_id,
            prepared.execution_generation,
            &prepared.cleanup_lease_id,
            identity.clone(),
            identity,
        )
        .expect("running scope");
    store
        .begin_cleanup(
            &prepared.scope_id,
            prepared.execution_generation,
            &prepared.cleanup_lease_id,
            "deadline-fixture",
        )
        .expect("begin cleanup");
    let complete = store
        .complete_cleanup(
            &prepared.scope_id,
            prepared.execution_generation,
            &prepared.cleanup_lease_id,
            "deadline-fixture",
            true,
        )
        .expect("complete cleanup");
    let proof = complete.cleanup_proof.clone().expect("cleanup proof");
    drop(initial_lease);

    let mut long_lived = store
        .rebind_long_lived_after_handoff()
        .expect("capability-identical long-lived store");
    long_lived.operation_timeout = Duration::from_millis(100);
    let lease = long_lived
        .acquire_cleanup_lease(&complete)
        .expect("long-lived cleanup lease");
    let lease_path = long_lived
        .cleanup_lease_path(&complete.scope_id)
        .expect("cleanup lease path");
    let directory = crate::daemons::state::StableDirectory::open(&long_lived.directory)
        .expect("process state directory");
    let quarantine = directory
        .deterministic_artifact_path(&lease_path, CLEANUP_LEASE_DELETE_PREFIX, ".quarantine")
        .expect("cleanup lease quarantine");
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let cleanup_path = lease_path.clone();
    let cleanup_quarantine = quarantine.clone();
    let worker = std::thread::spawn(move || {
        let mut paused = false;
        lease.release_after_proof_with_guard(&proof, || {
            if !paused && cleanup_quarantine.exists() && !cleanup_path.exists() {
                paused = true;
                ready_tx
                    .send(())
                    .expect("publish cleanup-lease quarantine pause");
                resume_rx.recv().expect("resume cleanup-lease deletion");
            }
            Ok(())
        })
    });

    ready_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("cleanup lease reached final quarantine");
    assert!(!lease_path.exists(), "cleanup lease source was quarantined");
    assert!(quarantine.is_file(), "cleanup lease quarantine is retained");
    std::thread::sleep(Duration::from_millis(300));
    let quarantined = std::fs::read(&quarantine).expect("cleanup lease bytes");
    let scope_bytes = std::fs::read(
        long_lived
            .record_path(&complete.scope_id)
            .expect("scope record path"),
    )
    .expect("complete scope bytes");
    resume_tx.send(()).expect("resume expired lease deletion");

    let error = worker
        .join()
        .expect("cleanup lease worker")
        .expect_err("expired cleanup-lease deletion must fail closed");
    assert!(
        error.contains("managed-process scope lock deadline elapsed"),
        "{error}"
    );
    assert_eq!(
        std::fs::read(&quarantine).expect("retained cleanup lease quarantine"),
        quarantined
    );
    assert_eq!(
        std::fs::read(
            long_lived
                .record_path(&complete.scope_id)
                .expect("scope record path")
        )
        .expect("retained complete scope"),
        scope_bytes
    );
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        std::fs::read(&quarantine).expect("later cleanup lease quarantine"),
        quarantined,
        "expired cleanup lease mutated after its owner returned"
    );
    assert_eq!(
        store
            .cleanup_lease_state(&complete)
            .expect("recover retained cleanup lease quarantine"),
        CleanupLeaseState::Missing
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn launch_abort_lease_delete_preserves_quarantine_and_retries_with_fresh_deadline() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let identity = ProcessIdentity::current().expect("process identity");
    let prepared = store
        .prepare(
            "sub-launch-abort-lease-deadline",
            "subagent",
            43,
            identity.clone(),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare scope");
    let initial_lease = store
        .acquire_cleanup_lease(&prepared)
        .expect("initial cleanup lease");
    let registered = store
        .register_launch_supervisor(
            &prepared.scope_id,
            prepared.execution_generation,
            &prepared.cleanup_lease_id,
            identity,
        )
        .expect("registered supervisor");
    let complete = store
        .complete_launch_abort(
            &registered.scope_id,
            registered.execution_generation,
            &registered.cleanup_lease_id,
        )
        .expect("complete launch abort");
    let proof = complete
        .launch_abort_proof
        .clone()
        .expect("launch-abort proof");
    drop(initial_lease);

    let mut long_lived = store
        .rebind_long_lived_after_handoff()
        .expect("capability-identical long-lived store");
    long_lived.operation_timeout = Duration::from_millis(100);
    let lease = long_lived
        .acquire_cleanup_lease(&complete)
        .expect("long-lived cleanup lease");
    let lease_path = long_lived
        .cleanup_lease_path(&complete.scope_id)
        .expect("cleanup lease path");
    let quarantine = long_lived
        .directory_capability
        .deterministic_artifact_path(&lease_path, CLEANUP_LEASE_DELETE_PREFIX, ".quarantine")
        .expect("cleanup lease quarantine");
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
    let cleanup_path = lease_path.clone();
    let cleanup_quarantine = quarantine.clone();
    let worker = std::thread::spawn(move || {
        let mut paused = false;
        lease.release_after_launch_abort_with_guard(&proof, || {
            if !paused && cleanup_quarantine.exists() && !cleanup_path.exists() {
                paused = true;
                ready_tx
                    .send(())
                    .expect("publish launch-abort lease quarantine pause");
                resume_rx
                    .recv()
                    .expect("resume launch-abort lease deletion");
            }
            Ok(())
        })
    });

    ready_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("launch-abort lease reached final quarantine");
    assert!(!lease_path.exists(), "cleanup lease source was quarantined");
    assert!(quarantine.is_file(), "cleanup lease quarantine is retained");
    let quarantined = std::fs::read(&quarantine).expect("cleanup lease bytes");
    let scope_bytes = std::fs::read(
        long_lived
            .record_path(&complete.scope_id)
            .expect("scope record path"),
    )
    .expect("complete scope bytes");
    thread::sleep(Duration::from_millis(150));
    resume_tx.send(()).expect("resume expired lease deletion");

    let error = worker
        .join()
        .expect("cleanup lease worker")
        .expect_err("expired launch-abort lease deletion must fail closed");
    assert!(
        error.contains("managed-process scope lock deadline elapsed"),
        "{error}"
    );
    assert_eq!(
        std::fs::read(&quarantine).expect("retained cleanup lease quarantine"),
        quarantined
    );
    assert_eq!(
        std::fs::read(
            long_lived
                .record_path(&complete.scope_id)
                .expect("scope record path")
        )
        .expect("retained complete scope"),
        scope_bytes
    );
    assert_eq!(
        store
            .cleanup_lease_state(&complete)
            .expect("fresh retry recovers launch-abort lease quarantine"),
        CleanupLeaseState::Missing
    );
    assert!(!lease_path.exists());
    assert!(!quarantine.exists());
}

#[test]
fn live_cleanup_lease_excludes_a_second_supervisor() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let record = store
        .prepare(
            "sub-live",
            "subagent",
            9,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare scope");
    let _lease = store.acquire_cleanup_lease(&record).expect("cleanup lease");
    assert_eq!(
        store
            .cleanup_lease_state(&record)
            .expect("inspect live cleanup lease"),
        CleanupLeaseState::Live
    );
    let directory = crate::daemons::state::StableDirectory::open(&store.directory)
        .expect("stable scope directory");
    let path = store
        .cleanup_lease_path(&record.scope_id)
        .expect("cleanup lease path");
    let readable = directory.open_read(&path).expect("open live cleanup lease");
    let observed: CleanupLeaseRecord =
        read_bounded_json(&readable, &path).expect("read live cleanup lease");
    assert_eq!(observed.scope_id, record.scope_id);
    let error = store
        .acquire_cleanup_lease(&record)
        .err()
        .expect("second cleanup owner must be excluded");
    assert!(error.contains("already live"), "{error}");
}

#[cfg(not(target_os = "linux"))]
#[test]
fn linux_supervisor_loss_recovery_fails_closed_on_other_platforms() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let record = store
        .prepare(
            "sub-linux-recovery",
            "subagent",
            12,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare scope");

    let error = store
        .recover_linux_supervisor_loss(&record)
        .expect_err("non-Linux hosts cannot recover a Linux process scope");

    assert!(error.contains("unavailable on this platform"), "{error}");
    assert_eq!(
        store.load(&record.scope_id).expect("scope remains intact"),
        record
    );
    assert_eq!(
        store
            .cleanup_lease_state(&record)
            .expect("cleanup lease state"),
        CleanupLeaseState::Missing
    );
}

#[test]
fn stale_scope_snapshot_cannot_acquire_cleanup_lease() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let record = store
        .prepare(
            "sub-stale-snapshot",
            "subagent",
            10,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare scope");
    let mutated = store
        .mark_recovery_required(
            &record.scope_id,
            record.execution_generation,
            &record.cleanup_lease_id,
            "fixture mutation",
        )
        .expect("mutate authoritative scope");
    assert_eq!(mutated.status, ProcessScopeStatus::Prepared);
    assert_eq!(mutated.cleanup_reason.as_deref(), Some("fixture mutation"));
    let error = store
        .acquire_cleanup_lease(&record)
        .err()
        .expect("stale scope snapshot must be fenced");
    assert!(error.contains("scope changed"), "{error}");
}

#[cfg(target_os = "linux")]
#[test]
fn prepared_recovery_claims_lease_only_after_the_supervisor_exits() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let prepared = store
        .prepare(
            "sub-live-prepared-supervisor",
            "subagent",
            42,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare scope");
    let mut supervisor = Command::new("sh")
        .args(["-c", "exec sleep 60"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn live supervisor");
    let supervisor_identity =
        ProcessIdentity::capture(supervisor.id()).expect("supervisor identity");
    let prepared = store
        .register_launch_supervisor(
            &prepared.scope_id,
            prepared.execution_generation,
            &prepared.cleanup_lease_id,
            supervisor_identity.clone(),
        )
        .expect("register supervisor");

    let started = Instant::now();
    let error = store
        .recover_linux_supervisor_loss(&prepared)
        .expect_err("live supervisor cannot enter prepared recovery");
    assert!(error.contains("still live"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(
        store.load(&prepared.scope_id).expect("preserved scope"),
        prepared
    );
    assert_eq!(
        store
            .cleanup_lease_state(&prepared)
            .expect("pre-recovery lease state"),
        CleanupLeaseState::Missing
    );

    supervisor.kill().expect("kill supervisor");
    supervisor.wait().expect("reap supervisor");
    let completed = store
        .recover_linux_supervisor_loss(&prepared)
        .expect("recover stopped pre-lease supervisor");
    assert_eq!(completed.status, ProcessScopeStatus::Complete);
    let proof = completed
        .launch_abort_proof
        .as_ref()
        .expect("launch-abort proof");
    assert_eq!(proof.supervisor, supervisor_identity);
    assert!(proof.namespace_root.is_none());
    assert!(proof.workload_never_launched);
    assert_eq!(
        store
            .cleanup_lease_state(&completed)
            .expect("released recovery lease"),
        CleanupLeaseState::Missing
    );
}

#[cfg(target_os = "linux")]
#[test]
fn running_recovery_exact_signals_a_live_direct_child_after_supervisor_exit() {
    use std::os::unix::process::ExitStatusExt;

    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let prepared = store
        .prepare(
            "sub-live-direct-child-recovery",
            "subagent",
            45,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare scope");
    let cleanup_lease = store
        .acquire_cleanup_lease(&prepared)
        .expect("cleanup lease");

    let mut supervisor = Command::new("sh")
        .args(["-c", "sleep 60"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn supervisor fixture");
    let supervisor_identity =
        ProcessIdentity::capture(supervisor.id()).expect("supervisor identity");
    let mut supervisor_guard = LinuxIdentityKillGuard::new(supervisor_identity.clone());

    let mut direct_child = Command::new("sh")
        .args(["-c", "exec sleep 60"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn direct-child fixture");
    let direct_child_identity =
        ProcessIdentity::capture(direct_child.id()).expect("direct-child identity");
    let mut direct_child_guard = LinuxIdentityKillGuard::new(direct_child_identity.clone());
    let running = store
        .mark_running(
            &prepared.scope_id,
            prepared.execution_generation,
            &prepared.cleanup_lease_id,
            supervisor_identity.clone(),
            direct_child_identity.clone(),
        )
        .expect("persist running scope");
    drop(cleanup_lease);

    supervisor.kill().expect("kill supervisor fixture");
    supervisor.wait().expect("reap supervisor fixture");
    supervisor_guard.disarm();
    assert!(!supervisor_identity.still_matches());
    assert!(direct_child_identity.still_matches());
    assert_eq!(
        store
            .cleanup_lease_state(&running)
            .expect("recoverable cleanup lease"),
        CleanupLeaseState::Recoverable
    );

    let direct_child_reaper =
        std::thread::spawn(move || direct_child.wait().expect("reap direct-child fixture"));
    let completed = store
        .recover_linux_supervisor_loss(&running)
        .expect("recover live direct child");
    let direct_child_status = direct_child_reaper.join().expect("direct-child reaper");
    direct_child_guard.disarm();

    assert_eq!(direct_child_status.signal(), Some(libc::SIGKILL));
    assert!(!direct_child_identity.still_matches());
    assert_eq!(completed.status, ProcessScopeStatus::Complete);
    let proof = completed.cleanup_proof.as_ref().expect("cleanup proof");
    assert_eq!(proof.direct_child, direct_child_identity);
    assert_eq!(proof.outcome, "supervisor_lost_linux_pid_namespace");
    assert!(proof.descendants_reaped);
    assert_eq!(
        store
            .cleanup_lease_state(&completed)
            .expect("released cleanup lease"),
        CleanupLeaseState::Missing
    );
}

#[cfg(target_os = "linux")]
#[test]
fn live_identity_recovery_obeys_outer_deadline_without_late_mutation() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let live_identity = ProcessIdentity::current().expect("live process identity");
    let prepared = store
        .prepare(
            "sub-bounded-live-recovery",
            "subagent",
            43,
            live_identity.clone(),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare scope");
    let cleanup_lease = store
        .acquire_cleanup_lease(&prepared)
        .expect("cleanup lease");
    let running = store
        .mark_running(
            &prepared.scope_id,
            prepared.execution_generation,
            &prepared.cleanup_lease_id,
            live_identity.clone(),
            live_identity,
        )
        .expect("running scope");
    drop(cleanup_lease);
    assert_eq!(
        store
            .cleanup_lease_state(&running)
            .expect("recoverable cleanup lease"),
        CleanupLeaseState::Recoverable
    );

    let recovery_budget = Duration::from_millis(75);
    let deadline = Instant::now() + recovery_budget;
    let bounded = ProcessScopeStore::open_with_lock_deadline(root.path(), deadline)
        .expect("deadline-aware scope store");
    let started = Instant::now();
    let error = bounded
        .recover_linux_supervisor_loss(&running)
        .expect_err("live identities cannot prove recovery before the outer deadline");

    assert!(error.contains("remained unproven"), "{error}");
    assert!(error.contains("supervisor_live=true"), "{error}");
    assert!(error.contains("direct_child_live=true"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "live-identity recovery exceeded the inherited deadline"
    );
    assert_eq!(
        store.load(&running.scope_id).expect("scope after timeout"),
        running
    );

    thread::sleep(recovery_budget * 2);
    assert_eq!(
        store
            .load(&running.scope_id)
            .expect("scope after grace period"),
        running,
        "deadline-aware recovery mutated durable scope state after returning"
    );
    assert_eq!(
        store
            .cleanup_lease_state(&running)
            .expect("cleanup lease after timeout"),
        CleanupLeaseState::Recoverable
    );
}

#[cfg(target_os = "linux")]
#[test]
fn retry_recovers_uncommitted_scope_after_recovery_required_transition() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let prepared = store
        .prepare(
            "sub-uncommitted-recovery-retry",
            "subagent",
            44,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare scope");
    let cleanup_lease = store
        .acquire_cleanup_lease(&prepared)
        .expect("cleanup lease");
    let mut child = Command::new("sh")
        .args(["-c", "sleep 60"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn gated identity fixture");
    let identity = ProcessIdentity::capture(child.id()).expect("gated identity");
    let gated = store
        .mark_gated_running(
            &prepared.scope_id,
            prepared.execution_generation,
            &prepared.cleanup_lease_id,
            identity.clone(),
            identity,
        )
        .expect("persist gated scope");
    let recovery_required = store
        .mark_recovery_required(
            &gated.scope_id,
            gated.execution_generation,
            &gated.cleanup_lease_id,
            "first recovery attempt was interrupted",
        )
        .expect("persist recovery-required retry state");
    assert_eq!(
        recovery_required.status,
        ProcessScopeStatus::RecoveryRequired
    );
    assert_eq!(recovery_required.launch_committed, Some(false));
    drop(cleanup_lease);
    child.kill().expect("kill gated identity fixture");
    child.wait().expect("reap gated identity fixture");

    let completed = store
        .recover_linux_supervisor_loss(&recovery_required)
        .expect("retry proves launch abort");
    assert_eq!(completed.status, ProcessScopeStatus::Complete);
    assert!(completed.cleanup_proof.is_none());
    assert!(completed
        .launch_abort_proof
        .as_ref()
        .is_some_and(|proof| proof.workload_never_launched));
    assert_eq!(
        store
            .cleanup_lease_state(&completed)
            .expect("released cleanup lease"),
        CleanupLeaseState::Missing
    );
}

#[test]
fn caller_supplied_cleanup_proof_cannot_release_a_noncomplete_scope() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let owner = ProcessIdentity::current().expect("owner identity");
    let record = store
        .prepare(
            "sub-forged-proof",
            "subagent",
            11,
            owner.clone(),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare scope");
    let lease = store.acquire_cleanup_lease(&record).expect("cleanup lease");
    let running = store
        .mark_running(
            &record.scope_id,
            record.execution_generation,
            &record.cleanup_lease_id,
            owner.clone(),
            owner.clone(),
        )
        .expect("mark running");
    let forged = CleanupProof {
        execution_generation: record.execution_generation,
        cleanup_lease_id: record.cleanup_lease_id.clone(),
        backend: record.backend,
        direct_child: owner,
        outcome: "forged".to_string(),
        descendants_reaped: true,
        completed_at: Utc::now(),
    };
    let error = lease
        .release_after_proof(&forged)
        .expect_err("noncomplete scope must not release cleanup ownership");
    assert!(error.contains("authoritative completed"), "{error}");
    assert_eq!(
        store
            .cleanup_lease_state(&running)
            .expect("cleanup lease state"),
        CleanupLeaseState::Recoverable
    );
}

#[test]
fn scope_store_recovers_atomic_evacuation_before_reading() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let record = store
        .prepare(
            "sub-atomic-recovery",
            "subagent",
            12,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare scope");
    let directory = crate::daemons::state::StableDirectory::open(&store.directory)
        .expect("stable scope directory");
    let target = store.record_path(&record.scope_id).expect("record path");
    let previous = directory
        .deterministic_previous_artifact_path(&target, SCOPE_WRITE_PREFIX)
        .expect("previous path");
    std::fs::rename(&target, &previous).expect("simulate crash after evacuation");

    assert_eq!(
        store.load(&record.scope_id).expect("recovered record"),
        record
    );
    assert!(target.is_file());
    assert!(!previous.exists());

    let committed = store
        .mark_recovery_required(
            &record.scope_id,
            record.execution_generation,
            &record.cleanup_lease_id,
            "committed successor",
        )
        .expect("publish committed successor");
    std::fs::write(
        &previous,
        serde_json::to_vec_pretty(&record).expect("encode prior record"),
    )
    .expect("simulate retained evacuated prior");
    assert_eq!(
        store
            .load(&record.scope_id)
            .expect("finalize committed record"),
        committed
    );
    assert!(!previous.exists());

    let temporary = directory
        .deterministic_artifact_path(&target, SCOPE_WRITE_PREFIX, ".tmp")
        .expect("temporary path");
    std::fs::remove_file(&target).expect("remove visible successor for crash fixture");
    std::fs::write(
        &previous,
        serde_json::to_vec_pretty(&record).expect("encode previous record"),
    )
    .expect("write previous record");
    std::fs::write(
        &temporary,
        serde_json::to_vec_pretty(&committed).expect("encode temporary successor"),
    )
    .expect("write temporary successor");
    std::fs::hard_link(&temporary, &target).expect("publish linked successor");
    let peak_entries = 3;
    let peak_name_bytes = [&previous, &temporary, &target]
        .iter()
        .map(|path| {
            path.file_name()
                .expect("transaction filename")
                .as_encoded_bytes()
                .len()
        })
        .sum();
    let peak_bytes = [&previous, &temporary, &target]
        .iter()
        .map(|path| std::fs::metadata(path).expect("transaction metadata").len())
        .sum();
    maintain_process_scope_directory_with_limits(
        &directory,
        false,
        ProcessScopeDirectoryLimits {
            max_entries: peak_entries,
            max_name_bytes: peak_name_bytes,
            max_bytes: peak_bytes,
            ..PROCESS_SCOPE_DIRECTORY_LIMITS
        },
    )
    .expect("recover transaction at its exact reserved peak");
    assert_eq!(
        store.load(&record.scope_id).expect("recovered successor"),
        committed
    );
    assert!(!previous.exists());
    assert!(!temporary.exists());
}

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn completed_scope_quarantines_recover_and_retire_from_embedded_proof() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let identity = ProcessIdentity::current().expect("process identity");
    let record = store
        .prepare(
            "sub-retire",
            "subagent",
            13,
            identity.clone(),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare scope");
    let lease = store.acquire_cleanup_lease(&record).expect("cleanup lease");
    store
        .mark_running(
            &record.scope_id,
            record.execution_generation,
            &record.cleanup_lease_id,
            identity.clone(),
            identity,
        )
        .expect("running scope");
    store
        .begin_cleanup(
            &record.scope_id,
            record.execution_generation,
            &record.cleanup_lease_id,
            "fixture",
        )
        .expect("begin cleanup");
    let complete = store
        .complete_cleanup(
            &record.scope_id,
            record.execution_generation,
            &record.cleanup_lease_id,
            "fixture",
            true,
        )
        .expect("complete cleanup");
    let proof = complete.cleanup_proof.clone().expect("cleanup proof");
    let directory = crate::daemons::state::StableDirectory::open(&store.directory)
        .expect("stable scope directory");
    let lease_path = store
        .cleanup_lease_path(&record.scope_id)
        .expect("lease path");
    let lease_quarantine = directory
        .deterministic_artifact_path(&lease_path, CLEANUP_LEASE_DELETE_PREFIX, ".quarantine")
        .expect("lease quarantine");
    std::fs::rename(&lease_path, &lease_quarantine).expect("simulate crash after lease quarantine");
    assert_eq!(
        store
            .cleanup_lease_state(&complete)
            .expect("inspect live quarantined lease"),
        CleanupLeaseState::Live
    );
    assert!(lease_quarantine.exists());
    assert_eq!(
        store
            .try_load(&complete.scope_id)
            .expect("coherent lookup while cleanup quarantine is live"),
        Some(complete.clone()),
        "a cleanup-lease quarantine must never be classified as absent"
    );
    assert!(store.acquire_cleanup_lease(&complete).is_err());
    assert!(store
        .retire_complete(&record.scope_id, record.execution_generation, &proof)
        .is_err());
    drop(lease);

    assert_eq!(
        store
            .cleanup_lease_state(&complete)
            .expect("recover lease quarantine"),
        CleanupLeaseState::Missing
    );
    assert!(!lease_quarantine.exists());

    let scope_path = store.record_path(&record.scope_id).expect("scope path");
    let scope_quarantine = directory
        .deterministic_artifact_path(&scope_path, SCOPE_DELETE_PREFIX, ".quarantine")
        .expect("scope quarantine");
    std::fs::rename(&scope_path, &scope_quarantine).expect("simulate crash after scope quarantine");
    assert_eq!(
        store
            .try_load(&complete.scope_id)
            .expect("coherent lookup from scope quarantine"),
        Some(complete.clone()),
        "a scope deletion quarantine must remain exact retirement authority"
    );
    assert!(store
        .retire_complete(&record.scope_id, record.execution_generation + 1, &proof)
        .is_err());
    assert!(scope_quarantine.exists());

    let mut wrong_proof = proof.clone();
    wrong_proof.outcome.push_str("-wrong");
    assert!(store
        .retire_complete(&record.scope_id, record.execution_generation, &wrong_proof)
        .is_err());
    assert!(scope_quarantine.exists());

    assert!(store
        .retire_complete(&record.scope_id, record.execution_generation, &proof)
        .expect("retire completed scope"));
    assert!(!scope_quarantine.exists());
    assert!(store
        .try_load(&record.scope_id)
        .expect("retired scope lookup")
        .is_none());
}

#[cfg(unix)]
#[test]
fn process_scope_quarantine_crash_child() {
    let Some(mode) = std::env::var_os("NIB_TEST_PROCESS_RETIRE_CRASH_MODE") else {
        return;
    };
    let root = PathBuf::from(
        std::env::var_os("NIB_TEST_PROCESS_RETIRE_ROOT").expect("crash fixture root"),
    );
    let id = std::env::var("NIB_TEST_PROCESS_RETIRE_ID").expect("crash fixture id");
    let ready =
        PathBuf::from(std::env::var_os("NIB_TEST_PROCESS_RETIRE_READY").expect("crash ready path"));
    let store = ProcessScopeStore::open(&root).expect("child scope store");
    let scope = store.load(&id).expect("child complete scope");
    let proof = scope.cleanup_proof.clone().expect("child cleanup proof");
    let scope_path = store.record_path(&id).expect("scope path");
    let lease_path = store.cleanup_lease_path(&id).expect("lease path");
    let directory =
        crate::daemons::state::StableDirectory::open(&store.directory).expect("scope directory");
    let (canonical, quarantine) = if mode == "lease" {
        (
            lease_path.clone(),
            directory
                .deterministic_artifact_path(
                    &lease_path,
                    CLEANUP_LEASE_DELETE_PREFIX,
                    ".quarantine",
                )
                .expect("lease quarantine"),
        )
    } else {
        (
            scope_path.clone(),
            directory
                .deterministic_artifact_path(&scope_path, SCOPE_DELETE_PREFIX, ".quarantine")
                .expect("scope quarantine"),
        )
    };
    let mut pause = || {
        if quarantine.exists() && !canonical.exists() {
            std::fs::write(&ready, b"quarantined").expect("publish crash boundary");
            loop {
                thread::sleep(Duration::from_secs(1));
            }
        }
        Ok(())
    };
    if mode == "lease" {
        store
            .acquire_cleanup_lease(&scope)
            .expect("child cleanup lease")
            .release_after_proof_with_guard(&proof, &mut pause)
            .expect("parent kills child at lease quarantine");
    } else {
        store
            .retire_completed_scope_with_guard(
                &id,
                scope.execution_generation,
                CompletionAuthority::Cleanup(&proof),
                &mut pause,
            )
            .expect("parent kills child at scope quarantine");
    }
}
