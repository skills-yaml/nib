use super::*;

#[cfg(unix)]
#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn sigkill_at_scope_and_cleanup_quarantines_retries_exact_retirement_first() {
    for mode in ["lease", "scope"] {
        let root = git_project();
        let store = ProcessScopeStore::open(root.path()).expect("scope store");
        let id = format!("sub-retire-crash-{mode}");
        let identity = ProcessIdentity::current().expect("process identity");
        let prepared = store
            .prepare(
                &id,
                "subagent",
                if mode == "lease" { 81 } else { 82 },
                identity.clone(),
                ProcessScopeBackend::current().expect("process backend"),
            )
            .expect("prepare scope");
        let initial_lease = store
            .acquire_cleanup_lease(&prepared)
            .expect("initial cleanup lease");
        store
            .mark_running(
                &id,
                prepared.execution_generation,
                &prepared.cleanup_lease_id,
                identity.clone(),
                identity,
            )
            .expect("running scope");
        store
            .begin_cleanup(
                &id,
                prepared.execution_generation,
                &prepared.cleanup_lease_id,
                "crash-retirement-fixture",
            )
            .expect("begin cleanup");
        let complete = store
            .complete_cleanup(
                &id,
                prepared.execution_generation,
                &prepared.cleanup_lease_id,
                "crash-retirement-fixture",
                true,
            )
            .expect("complete cleanup");
        let proof = complete.cleanup_proof.clone().expect("cleanup proof");
        if mode == "scope" {
            initial_lease
                .release_after_proof(&proof)
                .expect("release lease before scope-retirement crash");
        } else {
            drop(initial_lease);
        }
        let external = root.path().join("external-compensation-sentinel");
        std::fs::write(&external, b"preserve until scope retirement").expect("external sentinel");
        let ready = root.path().join(format!("{mode}.quarantined"));
        let mut child = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "sandbox::process::tests::test_part_0::process_scope_quarantine_crash_child",
                "--nocapture",
            ])
            .env("NIB_TEST_PROCESS_RETIRE_CRASH_MODE", mode)
            .env("NIB_TEST_PROCESS_RETIRE_ROOT", root.path())
            .env("NIB_TEST_PROCESS_RETIRE_ID", &id)
            .env("NIB_TEST_PROCESS_RETIRE_READY", &ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn quarantine child");
        let wait_deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() && Instant::now() < wait_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.is_file(), "child did not reach {mode} quarantine");
        child.kill().expect("SIGKILL quarantine child");
        child.wait().expect("reap quarantine child");
        assert_eq!(
            std::fs::read(&external).expect("external state remains"),
            b"preserve until scope retirement"
        );

        let retry = ProcessScopeStore::open(root.path()).expect("fresh retry store");
        assert_eq!(
            retry.try_load(&id).expect("coherent retry lookup"),
            Some(complete.clone()),
            "quarantine must not be misclassified as absent"
        );
        assert_eq!(
            std::fs::read(&external).expect("external state before retirement"),
            b"preserve until scope retirement"
        );
        assert!(
            retry
                .retire_complete(&id, complete.execution_generation, &proof)
                .expect("fresh exact retirement"),
            "fresh retry retires canonical or quarantined exact scope"
        );
        assert!(
            retry
                .try_load(&id)
                .expect("post-retirement lookup")
                .is_none(),
            "scope and cleanup lease are absent only after exact retirement"
        );
        assert_eq!(
            std::fs::read(&external).expect("unrelated external state preserved"),
            b"preserve until scope retirement"
        );
    }
}

#[test]
fn process_scope_maintenance_enforces_aggregate_limits_and_unknown_entries() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    store
        .prepare(
            "sub-bounded",
            "subagent",
            14,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare scope");
    let directory = crate::daemons::state::StableDirectory::open(&store.directory)
        .expect("stable scope directory");
    let tiny_record_limit = ProcessScopeDirectoryLimits {
        max_records: 1,
        ..PROCESS_SCOPE_DIRECTORY_LIMITS
    };
    let error = maintain_process_scope_directory_with_limits(&directory, true, tiny_record_limit)
        .expect_err("reserved scope must exceed the record cap");
    assert!(error.contains("record limit"), "{error}");
    let tiny_byte_limit = ProcessScopeDirectoryLimits {
        max_bytes: 1,
        ..PROCESS_SCOPE_DIRECTORY_LIMITS
    };
    let error = maintain_process_scope_directory_with_limits(&directory, false, tiny_byte_limit)
        .expect_err("aggregate bytes must be bounded");
    assert!(error.contains("aggregate limit"), "{error}");

    std::fs::write(store.directory.join("foreign-state"), b"unknown")
        .expect("unknown state fixture");
    let error = store
        .prepare(
            "sub-rejected",
            "subagent",
            15,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect_err("unknown directory entries must fail closed");
    assert!(error.contains("unknown entry"), "{error}");
}

#[test]
fn version_one_scope_state_is_preserved_without_blocking_new_generations() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let mut legacy = store
        .prepare(
            "sub-version-one",
            "subagent",
            151,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare legacy fixture");
    let lease = store
        .acquire_cleanup_lease(&legacy)
        .expect("legacy cleanup lease");
    drop(lease);
    legacy.version = 1;
    let scope_path = store.record_path(&legacy.scope_id).expect("scope path");
    std::fs::write(
        &scope_path,
        serde_json::to_vec_pretty(&legacy).expect("encode legacy scope"),
    )
    .expect("write legacy scope");
    let lease_path = store
        .cleanup_lease_path(&legacy.scope_id)
        .expect("lease path");
    let mut legacy_lease: CleanupLeaseRecord =
        serde_json::from_slice(&std::fs::read(&lease_path).expect("read cleanup lease"))
            .expect("decode cleanup lease");
    legacy_lease.version = 1;
    std::fs::write(
        &lease_path,
        serde_json::to_vec_pretty(&legacy_lease).expect("encode legacy lease"),
    )
    .expect("write legacy lease");
    let directory = crate::daemons::state::StableDirectory::open(&store.directory)
        .expect("stable scope directory");
    let scope_previous = directory
        .deterministic_previous_artifact_path(&scope_path, SCOPE_WRITE_PREFIX)
        .expect("legacy scope previous path");
    let scope_temporary = directory
        .deterministic_artifact_path(&scope_path, SCOPE_WRITE_PREFIX, ".tmp")
        .expect("legacy scope temporary path");
    let legacy_scope_bytes =
        serde_json::to_vec_pretty(&legacy).expect("encode legacy scope transaction");
    std::fs::write(&scope_previous, &legacy_scope_bytes)
        .expect("write legacy scope previous artifact");
    std::fs::write(&scope_temporary, &legacy_scope_bytes)
        .expect("write legacy scope temporary artifact");
    let miskeyed_target = store
        .record_path("sub-miskeyed-version-one")
        .expect("miskeyed legacy target");
    let miskeyed_temporary = directory
        .deterministic_artifact_path(&miskeyed_target, SCOPE_WRITE_PREFIX, ".tmp")
        .expect("miskeyed legacy temporary path");
    std::fs::write(&miskeyed_temporary, &legacy_scope_bytes)
        .expect("write miskeyed legacy temporary artifact");
    let lease_temporary = directory
        .deterministic_artifact_path(&lease_path, CLEANUP_LEASE_WRITE_PREFIX, ".tmp")
        .expect("legacy lease temporary path");
    std::fs::write(
        &lease_temporary,
        serde_json::to_vec_pretty(&legacy_lease).expect("encode legacy lease temporary"),
    )
    .expect("write legacy lease temporary artifact");

    let error = store
        .load(&legacy.scope_id)
        .expect_err("legacy scope cannot be interpreted as version two");
    assert!(error.contains("unsupported managed-process scope version 1"));
    assert!(scope_previous.exists());
    assert!(scope_temporary.exists());
    assert!(lease_temporary.exists());
    store
        .prepare(
            "sub-after-version-one",
            "subagent",
            152,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("unrelated version-two scope remains available");
    assert!(scope_path.exists());
    assert!(lease_path.exists());
    assert!(scope_previous.exists());
    assert!(scope_temporary.exists());
    assert!(lease_temporary.exists());
    assert!(!miskeyed_temporary.exists());
}

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn process_scope_publications_reserve_all_limits_before_writing() {
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let mut oversized_owner = ProcessIdentity::current().expect("owner identity");
    oversized_owner.start_marker = "x".repeat(MAX_PROCESS_IDENTITY_MARKER_BYTES + 1);
    let error = store
        .prepare(
            "sub-oversized-owner",
            "subagent",
            16,
            oversized_owner,
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect_err("oversized identities must fail before publication");
    assert!(error.contains("owner identity"), "{error}");
    assert!(!store
        .record_path("sub-oversized-owner")
        .expect("record path")
        .exists());

    let record = store
        .prepare(
            "sub-publication-budget",
            "subagent",
            17,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare bounded scope");
    let error = store
        .mark_recovery_required(
            &record.scope_id,
            record.execution_generation,
            &record.cleanup_lease_id,
            "x".repeat(MAX_PROCESS_CLEANUP_TEXT_BYTES + 1),
        )
        .expect_err("oversized mutation must fail before publication");
    assert!(error.contains("cleanup reason"), "{error}");
    assert_eq!(
        store.load(&record.scope_id).expect("unchanged scope"),
        record
    );

    let directory = crate::daemons::state::StableDirectory::open(&store.directory)
        .expect("stable scope directory");
    let usage =
        process_scope_directory_usage_with_limits(&directory, PROCESS_SCOPE_DIRECTORY_LIMITS)
            .expect("scope directory usage");
    let lease_record = CleanupLeaseRecord {
        version: PROCESS_SCOPE_VERSION,
        scope_id: record.scope_id.clone(),
        execution_generation: record.execution_generation,
        cleanup_lease_id: record.cleanup_lease_id.clone(),
    };
    let lease_bytes =
        encode_process_state_bounded(&lease_record, "cleanup lease").expect("encode lease");
    let lease_path = store
        .cleanup_lease_path(&record.scope_id)
        .expect("cleanup lease path");
    let lease_temporary = directory
        .deterministic_artifact_path(&lease_path, CLEANUP_LEASE_WRITE_PREFIX, ".tmp")
        .expect("lease temporary path");
    let lease_transaction_name_bytes = lease_temporary
        .file_name()
        .expect("lease temporary name")
        .as_encoded_bytes()
        .len()
        + lease_path
            .file_name()
            .expect("lease target name")
            .as_encoded_bytes()
            .len();
    let lease_peak_bytes = usage.bytes + 2 * lease_bytes.len() as u64;
    let lease_peak_entries = usage.entries + 2;
    let lease_peak_name_bytes = usage.name_bytes + lease_transaction_name_bytes;

    ensure_process_scope_publication_budget(
        &directory,
        &lease_path,
        lease_bytes.len() as u64,
        ProcessAtomicKind::CleanupLease,
        ProcessScopeDirectoryLimits {
            max_bytes: lease_peak_bytes,
            max_entries: lease_peak_entries,
            max_name_bytes: lease_peak_name_bytes,
            ..PROCESS_SCOPE_DIRECTORY_LIMITS
        },
    )
    .expect("exact cleanup-lease transaction peak is admitted");

    let byte_limits = ProcessScopeDirectoryLimits {
        max_bytes: lease_peak_bytes - 1,
        ..PROCESS_SCOPE_DIRECTORY_LIMITS
    };
    let error = ensure_process_scope_publication_budget(
        &directory,
        &lease_path,
        lease_bytes.len() as u64,
        ProcessAtomicKind::CleanupLease,
        byte_limits,
    )
    .expect_err("prospective aggregate bytes must be reserved");
    assert!(error.contains("aggregate limit"), "{error}");

    let entry_limits = ProcessScopeDirectoryLimits {
        max_entries: lease_peak_entries - 1,
        ..PROCESS_SCOPE_DIRECTORY_LIMITS
    };
    let error = ensure_process_scope_publication_budget(
        &directory,
        &lease_path,
        lease_bytes.len() as u64,
        ProcessAtomicKind::CleanupLease,
        entry_limits,
    )
    .expect_err("prospective entries must be reserved");
    assert!(error.contains("entry limit"), "{error}");

    let name_limits = ProcessScopeDirectoryLimits {
        max_name_bytes: lease_peak_name_bytes - 1,
        ..PROCESS_SCOPE_DIRECTORY_LIMITS
    };
    let error = ensure_process_scope_publication_budget(
        &directory,
        &lease_path,
        lease_bytes.len() as u64,
        ProcessAtomicKind::CleanupLease,
        name_limits,
    )
    .expect_err("prospective filename bytes must be reserved");
    assert!(error.contains("filename limit"), "{error}");

    let scope_path = store.record_path(&record.scope_id).expect("scope path");
    let scope_bytes =
        encode_process_state_bounded(&record, "scope record").expect("encode replacement scope");
    let scope_temporary = directory
        .deterministic_artifact_path(&scope_path, SCOPE_WRITE_PREFIX, ".tmp")
        .expect("scope temporary path");
    let scope_previous = directory
        .deterministic_previous_artifact_path(&scope_path, SCOPE_WRITE_PREFIX)
        .expect("scope previous path");
    let scope_peak_name_bytes = usage.name_bytes
        + scope_temporary
            .file_name()
            .expect("scope temporary name")
            .as_encoded_bytes()
            .len()
        + scope_previous
            .file_name()
            .expect("scope previous name")
            .as_encoded_bytes()
            .len();
    ensure_process_scope_publication_budget(
        &directory,
        &scope_path,
        scope_bytes.len() as u64,
        ProcessAtomicKind::Scope,
        ProcessScopeDirectoryLimits {
            max_bytes: usage.bytes + 2 * scope_bytes.len() as u64,
            max_entries: usage.entries + 2,
            max_name_bytes: scope_peak_name_bytes,
            ..PROCESS_SCOPE_DIRECTORY_LIMITS
        },
    )
    .expect("exact replacement transaction peak is admitted");
    let error = ensure_process_scope_publication_budget(
        &directory,
        &scope_path,
        scope_bytes.len() as u64,
        ProcessAtomicKind::Scope,
        ProcessScopeDirectoryLimits {
            max_bytes: usage.bytes + 2 * scope_bytes.len() as u64 - 1,
            ..PROCESS_SCOPE_DIRECTORY_LIMITS
        },
    )
    .expect_err("replacement scratch bytes must be reserved");
    assert!(error.contains("aggregate limit"), "{error}");
    let error = ensure_process_scope_publication_budget(
        &directory,
        &scope_path,
        scope_bytes.len() as u64,
        ProcessAtomicKind::Scope,
        ProcessScopeDirectoryLimits {
            max_entries: usage.entries + 1,
            ..PROCESS_SCOPE_DIRECTORY_LIMITS
        },
    )
    .expect_err("replacement scratch entries must be reserved");
    assert!(error.contains("entry limit"), "{error}");
    let error = ensure_process_scope_publication_budget(
        &directory,
        &scope_path,
        scope_bytes.len() as u64,
        ProcessAtomicKind::Scope,
        ProcessScopeDirectoryLimits {
            max_name_bytes: scope_peak_name_bytes - 1,
            ..PROCESS_SCOPE_DIRECTORY_LIMITS
        },
    )
    .expect_err("replacement scratch filenames must be reserved");
    assert!(error.contains("filename limit"), "{error}");

    let next_scope_path = store
        .record_path("sub-next-scope")
        .expect("next scope path");
    let record_limits = ProcessScopeDirectoryLimits {
        max_records: usage.records,
        ..PROCESS_SCOPE_DIRECTORY_LIMITS
    };
    let error = ensure_process_scope_publication_budget(
        &directory,
        &next_scope_path,
        1,
        ProcessAtomicKind::Scope,
        record_limits,
    )
    .expect_err("prospective scope records must be reserved");
    assert!(error.contains("record limit"), "{error}");
    assert!(!lease_path.exists());
    assert!(encode_process_state_bounded(
        &"x".repeat(MAX_SCOPE_RECORD_BYTES as usize),
        "oversized fixture"
    )
    .is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn launch_gate_preserves_payload_stdin_bytes() {
    use std::os::unix::net::UnixStream;

    if !crate::sandbox::detect_capabilities().managed_process_available {
        assert!(
            std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_none(),
            "CI requires a usable bwrap PID namespace"
        );
        return;
    }
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let record = store
        .prepare(
            "sub-gated-stdin",
            "subagent",
            88,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare gated stdin scope");
    let launched = root.path().join("payload-launched");
    let payload = b"nib-launch\npayload after the internal gate\n";
    let (owner_read, owner_write) = UnixStream::pair().expect("owner lifetime pipe");
    let output = supervise_foreground_with_ready(
        &store,
        &record,
        owner_read,
        SupervisedCommand {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                OsString::from("-c"),
                OsString::from("printf launched > \"$1\"; cat"),
                OsString::from("nib-gated-stdin-fixture"),
                launched.as_os_str().to_os_string(),
            ],
            cwd: root.path().to_path_buf(),
            stdin: payload.to_vec(),
            environment: Vec::new(),
        },
        |running| {
            if running.status != ProcessScopeStatus::Running
                || store.load(&running.scope_id)?.status != ProcessScopeStatus::Running
                || launched.exists()
            {
                return Err(
                    "launch gate was released before durable Running publication".to_string(),
                );
            }
            Ok(())
        },
    )
    .expect("supervise gated stdin fixture");

    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout, payload);
    assert_eq!(
        std::fs::read(&launched).expect("payload launch marker"),
        b"launched"
    );
    assert!(output.cleanup_proof.descendants_reaped);
    drop(owner_write);
}

#[cfg(target_os = "linux")]
#[test]
fn failed_bwrap_info_handshake_reaps_the_unpublished_namespace() {
    if !crate::sandbox::detect_capabilities().managed_process_available {
        assert!(
            std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_none(),
            "CI requires a usable bwrap PID namespace"
        );
        return;
    }
    let root = git_project();
    let token = format!("nib-info-failure-{}", uuid::Uuid::new_v4());
    let error = spawn_supervised_command_inner(
        ProcessScopeBackend::LinuxPidNamespace,
        &SupervisedCommand {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                OsString::from("-c"),
                OsString::from(format!("sleep 60 # {token}")),
            ],
            cwd: root.path().to_path_buf(),
            stdin: Vec::new(),
            environment: Vec::new(),
        },
        true,
        false,
    )
    .err()
    .expect("injected info failure");
    assert!(error.contains("injected bubblewrap namespace information failure"));
    let deadline = Instant::now() + SUPERVISOR_CLEANUP_TIMEOUT;
    while linux_process_command_contains(&token) && Instant::now() < deadline {
        thread::sleep(SUPERVISOR_POLL_INTERVAL);
    }
    assert!(
        !linux_process_command_contains(&token),
        "failed info handshake left its gated bubblewrap process alive"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn owner_eof_reaps_a_setsid_descendant_before_cleanup_proof() {
    use std::os::unix::net::UnixStream;

    if !crate::sandbox::detect_capabilities().managed_process_available {
        assert!(
            std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_none(),
            "CI requires a usable bwrap PID namespace"
        );
        return;
    }
    let root = git_project();
    let store = ProcessScopeStore::open(root.path()).expect("scope store");
    let record = store
        .prepare(
            "sub-owner-eof",
            "subagent",
            77,
            ProcessIdentity::current().expect("owner identity"),
            ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepare scope");
    let descendant_path = root.path().join("descendant.started");
    let survived_path = root.path().join("descendant.survived");
    let command = format!(
        "setsid sh -c 'printf started > {}; sleep 2; printf survived > {}' & wait",
        descendant_path.display(),
        survived_path.display(),
    );
    let (owner_read, owner_write) = UnixStream::pair().expect("owner EOF pipe");
    let worker_store = store.try_clone().expect("clone scope store");
    let worker_record = record.clone();
    let worker_root = root.path().to_path_buf();
    let supervisor = std::thread::spawn(move || {
        supervise_foreground(
            &worker_store,
            &worker_record,
            owner_read,
            SupervisedCommand {
                program: PathBuf::from("/bin/sh"),
                args: vec![OsString::from("-c"), OsString::from(command)],
                cwd: worker_root,
                stdin: Vec::new(),
                environment: Vec::new(),
            },
        )
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !descendant_path.is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        descendant_path.is_file(),
        "the real setsid descendant must start before owner EOF"
    );
    drop(owner_write);

    let output = supervisor
        .join()
        .expect("supervisor thread")
        .expect("supervised output");
    assert!(output.owner_lost);
    assert!(output.cleanup_proof.descendants_reaped);
    std::thread::sleep(Duration::from_millis(2200));
    assert!(!survived_path.exists());
    assert_eq!(
        store.load(&record.scope_id).expect("scope record").status,
        ProcessScopeStatus::Complete
    );
    assert_eq!(
        store
            .cleanup_lease_state(&store.load(&record.scope_id).expect("scope record"))
            .expect("cleanup lease state"),
        CleanupLeaseState::Missing
    );
}
