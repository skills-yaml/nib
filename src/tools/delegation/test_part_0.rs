use super::*;

#[test]
fn supervisor_protocol_rejects_partial_eof_on_every_platform() {
    let mut partial = std::io::Cursor::new(br#"{"version":1,"phase":"commit"}"#.to_vec());
    let error = read_subagent_supervisor_frame(&mut partial, "COMMIT")
        .expect_err("a frame without its newline is incomplete");
    assert!(
        error.contains("closed before a complete COMMIT frame"),
        "{error}"
    );
}

#[test]
fn supervisor_protocol_rejects_exact_identity_mismatch_on_every_platform() {
    #[cfg(target_os = "linux")]
    let backend = crate::sandbox::process::ProcessScopeBackend::LinuxPidNamespace;
    #[cfg(windows)]
    let backend = crate::sandbox::process::ProcessScopeBackend::WindowsJobObject;
    #[cfg(target_os = "macos")]
    let backend = crate::sandbox::process::ProcessScopeBackend::MacosProcessGroup;
    let now = Utc::now();
    let scope = crate::sandbox::process::ProcessScopeRecord {
        version: 2,
        scope_id: "sub-protocol-identity".to_string(),
        workload_kind: "subagent".to_string(),
        execution_generation: 17,
        cleanup_lease_id: uuid::Uuid::new_v4().to_string(),
        supervisor_registration_nonce: Some(uuid::Uuid::new_v4().to_string()),
        owner: crate::sandbox::process::ProcessIdentity::current().expect("process identity"),
        backend,
        status: crate::sandbox::process::ProcessScopeStatus::Prepared,
        launch_committed: Some(false),
        supervisor: None,
        direct_child: None,
        cleanup_reason: None,
        cleanup_proof: None,
        launch_abort_proof: None,
        created_at: now,
        updated_at: now,
    };
    let expected = SubagentSupervisorFrame {
        version: SUBAGENT_SUPERVISOR_PROTOCOL_VERSION,
        phase: "ready".to_string(),
        handoff_nonce: uuid::Uuid::new_v4().to_string(),
        subagent_id: scope.scope_id.clone(),
        execution_generation: scope.execution_generation,
        owner_lease: uuid::Uuid::new_v4().to_string(),
        process_scope: scope,
    };
    let mut mismatched = expected.clone();
    mismatched.phase = "commit".to_string();
    mismatched.process_scope.cleanup_lease_id = uuid::Uuid::new_v4().to_string();
    let error = validate_subagent_supervisor_frame(&expected, &mismatched, "commit")
        .expect_err("a mismatched process-scope authority must be rejected");
    assert!(error.contains("exact execution authority"), "{error}");
}

#[test]
fn fallback_preparation_crash_child() {
    let Some(project_root) = std::env::var_os(PREPARATION_CRASH_CHILD_PROJECT_ROOT) else {
        return;
    };
    #[cfg(windows)]
    let _timeout = SpawnPreparationTimeoutGuard::set(Duration::from_secs(15));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("child runtime");
    runtime.block_on(async {
        let _ = spawn_subagent(
            &json!({"prompt": "pause after durable fallback audit preparation"}),
            &PathBuf::from(project_root),
        );
    });
    panic!("crash child unexpectedly passed the preparation boundary");
}

#[test]
fn spawn_handoff_crash_child() {
    let Some(project_root) = std::env::var_os(HANDOFF_CRASH_CHILD_PROJECT_ROOT) else {
        return;
    };
    #[cfg(windows)]
    let _timeout = SpawnPreparationTimeoutGuard::set(Duration::from_secs(10));
    let phase = std::env::var(HANDOFF_CRASH_CHILD_PHASE).expect("handoff crash phase");
    let ready = PathBuf::from(
        std::env::var_os(HANDOFF_CRASH_CHILD_READY).expect("handoff crash ready path"),
    );
    let _hook = SpawnHandoffPhaseHookGuard::install(move |observed| {
        if observed == phase {
            std::fs::write(&ready, observed).expect("publish handoff crash readiness");
            loop {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("handoff crash child runtime");
    let result = runtime.block_on(async {
        spawn_subagent(
            &json!({"prompt": "pause at durable handoff boundary"}),
            &PathBuf::from(project_root),
        )
    });
    panic!("handoff crash child unexpectedly passed its boundary: {result:?}");
}

#[cfg(any(unix, windows))]
#[test]
fn killed_spawn_handoff_boundaries_never_leave_unlaunchable_running_state() {
    for phase in [
        "record_published",
        "manager_registered",
        "handoff_established",
        "handoff_proven",
        "handoff_committed",
        "launch_released",
        "before_intent_retirement",
        "intent_retired",
    ] {
        let root = tempfile::tempdir().expect("handoff crash project");
        initialize_spawn_test_repository(root.path());
        let ready = root.path().join(format!("handoff-{phase}.ready"));
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tools::delegation::tests::test_part_0::spawn_handoff_crash_child",
                "--nocapture",
            ])
            .env(HANDOFF_CRASH_CHILD_PROJECT_ROOT, root.path())
            .env(HANDOFF_CRASH_CHILD_PHASE, phase)
            .env(HANDOFF_CRASH_CHILD_READY, &ready)
            .spawn()
            .expect("spawn handoff crash child");
        let started = Instant::now();
        while !ready.exists() {
            if let Some(status) = child.try_wait().expect("inspect handoff crash child") {
                panic!("handoff crash child exited before {phase}: {status}");
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "handoff crash child did not reach {phase}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        child.kill().expect("kill handoff crash child");
        child.wait().expect("reap handoff crash child");

        let listed = list_subagents(root.path())
            .unwrap_or_else(|error| panic!("{phase}: restart reconciliation failed: {error}"));
        assert!(
            listed.iter().all(|record| !matches!(
                record.get("status").and_then(Value::as_str).unwrap_or(""),
                "running" | "recovery_required"
            )),
            "{phase}: restart exposed an unlaunchable record: {listed:?}"
        );
        let preparations = spawn_preparation_directory_path(root.path());
        assert!(
            !preparations.exists()
                || std::fs::read_dir(&preparations)
                    .expect("read reconciled handoff preparations")
                    .next()
                    .is_none(),
            "{phase}: restart did not retire preparation last"
        );
        assert!(
            !owner_lease_directory(root.path())
                .read_dir()
                .is_ok_and(|mut entries| entries.next().is_some()),
            "{phase}: restart left owner authority"
        );
    }
}

#[cfg(any(unix, windows))]
#[test]
fn killed_fallback_preparation_is_reconciled_without_orphans() {
    let root = tempfile::tempdir().expect("project root");
    initialize_spawn_test_repository(root.path());
    let ready = root.path().join("audit-preparation.ready");
    let resume = root.path().join("audit-preparation.resume");
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "tools::delegation::tests::test_part_0::fallback_preparation_crash_child",
            "--nocapture",
        ])
        .env(PREPARATION_CRASH_CHILD_PROJECT_ROOT, root.path())
        .env("NIB_TEST_SUBAGENT_AUDIT_PREPARED_READY", &ready)
        .env("NIB_TEST_SUBAGENT_AUDIT_PREPARED_RESUME", &resume)
        .spawn()
        .expect("spawn preparation child");
    let started = Instant::now();
    while !ready.exists() {
        if let Some(status) = child.try_wait().expect("inspect preparation child") {
            panic!("preparation child exited before crash boundary: {status}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "preparation child did not reach crash boundary"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let subagent_id = std::fs::read_to_string(&ready).expect("prepared subagent id");
    let intent = spawn_preparation_directory_path(root.path()).join(format!("{subagent_id}.json"));
    assert!(
        intent.is_file(),
        "write-ahead intent precedes crash boundary"
    );
    let audit_session = root
        .path()
        .join(".nib/profiles/default/sessions")
        .join(format!("{subagent_id}.json"));
    assert!(
        audit_session.is_file(),
        "audit leaf is published before crash"
    );

    child.kill().expect("kill preparation child");
    child.wait().expect("reap preparation child");
    let listed = list_subagents(root.path()).expect("restart reconciliation");
    assert!(
        listed.is_empty(),
        "preparing intent is never public workload"
    );
    assert!(!intent.exists(), "reconciled intent removed last");
    assert!(!audit_session.exists(), "exact prepared audit leaf removed");
    assert!(!owner_lease_directory(root.path())
        .read_dir()
        .is_ok_and(|mut entries| entries.next().is_some()));
    assert!(!root
        .path()
        .join(".nib/worktrees/subagents")
        .join(&subagent_id)
        .exists());

    SPAWN_RECORD_FAILURES.store(1, std::sync::atomic::Ordering::Release);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("retry runtime");
    let retry = runtime.block_on(async {
        spawn_subagent(
            &json!({"prompt": "fresh retry after preparation recovery"}),
            root.path(),
        )
    });
    let error = retry.expect_err("injected retry publication failure");
    assert!(error.contains("injected initial subagent record publication failure"));
}

#[cfg(any(unix, windows))]
#[test]
fn killed_planned_intent_precedes_every_spawn_resource_and_reconciles() {
    let root = tempfile::tempdir().expect("project root");
    initialize_spawn_test_repository(root.path());
    let ready = root.path().join("spawn-intent-planned.ready");
    let resume = root.path().join("spawn-intent-planned.resume");
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "tools::delegation::tests::test_part_0::fallback_preparation_crash_child",
            "--nocapture",
        ])
        .env(PREPARATION_CRASH_CHILD_PROJECT_ROOT, root.path())
        .env("NIB_TEST_SUBAGENT_INTENT_PLANNED_READY", &ready)
        .env("NIB_TEST_SUBAGENT_INTENT_PLANNED_RESUME", &resume)
        .spawn()
        .expect("spawn planned-intent child");
    let started = Instant::now();
    while !ready.exists() {
        if let Some(status) = child.try_wait().expect("inspect planned child") {
            panic!("planned-intent child exited before boundary: {status}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "planned-intent child did not reach boundary"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let id = std::fs::read_to_string(&ready).expect("planned subagent id");
    let intent = spawn_preparation_directory_path(root.path()).join(format!("{id}.json"));
    assert!(intent.is_file(), "planned intent is durable");
    assert!(
        !root
            .path()
            .join(".nib/worktrees/subagents")
            .join(&id)
            .exists(),
        "planned boundary precedes worktree mutation"
    );
    assert!(
        !owner_lease_directory(root.path())
            .read_dir()
            .is_ok_and(|mut entries| entries.next().is_some()),
        "planned boundary precedes owner mutation"
    );
    assert!(
        !root
            .path()
            .join(".nib/profiles/default/sessions")
            .join(format!("{id}.json"))
            .exists(),
        "planned boundary precedes audit mutation"
    );
    child.kill().expect("kill planned child");
    child.wait().expect("reap planned child");
    assert!(list_subagents(root.path())
        .expect("restart reconciliation")
        .is_empty());
    assert!(!intent.exists(), "planned intent reconciled last");
}

#[cfg(any(unix, windows))]
#[test]
fn spawn_intent_and_session_atomic_phase_crashes_reconcile_exactly() {
    let _reconciliation_timeout = SpawnReconciliationTimeoutGuard::set(Duration::from_secs(15));
    let categories = [
        ("intent-initial", ".preparations", "\"revision\": 0"),
        ("intent-resources", ".preparations", "\"revision\": 1"),
        ("intent-audit-planned", ".preparations", "\"revision\": 2"),
        ("intent-audit-published", ".preparations", "\"revision\": 3"),
        ("session-leaf", "sessions", "\"messages\": []"),
    ];
    for (category, component, content) in categories {
        for phase in [
            "temporary_create",
            "after_evacuation",
            "canonical_publish",
            "directory_sync",
            "receipt_return",
        ] {
            let root = tempfile::tempdir().expect("project root");
            initialize_spawn_test_repository(root.path());
            let ready = root.path().join(format!("{category}-{phase}.ready"));
            let resume = root.path().join(format!("{category}-{phase}.resume"));
            let mut child =
                std::process::Command::new(std::env::current_exe().expect("test binary"))
                    .args([
                        "--exact",
                        "tools::delegation::tests::test_part_0::fallback_preparation_crash_child",
                        "--nocapture",
                    ])
                    .env(PREPARATION_CRASH_CHILD_PROJECT_ROOT, root.path())
                    .env("NIB_TEST_ATOMIC_PUBLICATION_PHASE", phase)
                    .env("NIB_TEST_ATOMIC_PUBLICATION_PATH_COMPONENT", component)
                    .env("NIB_TEST_ATOMIC_PUBLICATION_CONTENT", content)
                    .env("NIB_TEST_ATOMIC_PUBLICATION_READY", &ready)
                    .env("NIB_TEST_ATOMIC_PUBLICATION_RESUME", &resume)
                    .spawn()
                    .expect("spawn atomic-phase child");
            let started = Instant::now();
            while !ready.exists() {
                if let Some(status) = child.try_wait().expect("inspect atomic child") {
                    panic!("atomic child exited before {category}/{phase} boundary: {status}");
                }
                assert!(
                    started.elapsed() < Duration::from_secs(20),
                    "atomic child did not reach {category}/{phase} boundary"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            let target =
                PathBuf::from(std::fs::read_to_string(&ready).expect("UTF-8 atomic target path"));
            let id = target
                .file_stem()
                .and_then(|value| value.to_str())
                .expect("atomic target id")
                .to_string();
            child.kill().expect("kill atomic child");
            child.wait().expect("reap atomic child");
            assert!(list_subagents(root.path())
                .unwrap_or_else(|error| { panic!("reconcile {category}/{phase} restart: {error}") })
                .is_empty());
            assert!(
                !spawn_preparation_directory_path(root.path())
                    .join(format!("{id}.json"))
                    .exists(),
                "{category}/{phase} preparation remained"
            );
            assert!(
                !root
                    .path()
                    .join(".nib/worktrees/subagents")
                    .join(&id)
                    .exists(),
                "{category}/{phase} worktree remained"
            );
            assert!(
                !root
                    .path()
                    .join(".nib/profiles/default/sessions")
                    .join(format!("{id}.json"))
                    .exists(),
                "{category}/{phase} audit session remained"
            );
            assert!(
                !root.path().join(".nib/profiles/default").exists(),
                "{category}/{phase} left the transaction-owned profile/session directory tree"
            );
            let namespace = subagent_namespace_snapshot(root.path());
            assert!(
                    namespace.iter().all(|(path, _)| {
                        !path
                            .file_name()
                            .is_some_and(|name| name.as_encoded_bytes().starts_with(b".nib-session-"))
                            && path.file_name()
                                != Some(std::ffi::OsStr::new(".session-directory.identity"))
                    }),
                    "{category}/{phase} left a session transaction, marker, or anchor artifact: {namespace:?}"
                );
        }
    }
}

#[cfg(any(unix, windows))]
#[test]
fn killed_fallback_namespace_phases_reconcile_exact_planned_artifacts() {
    for phase in ["directory", "marker", "anchor", "sync", "final"] {
        let root = tempfile::tempdir().expect("project root");
        initialize_spawn_test_repository(root.path());
        let ready = root
            .path()
            .join(format!("session-preparation-{phase}.ready"));
        let resume = root
            .path()
            .join(format!("session-preparation-{phase}.resume"));
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tools::delegation::tests::test_part_0::fallback_preparation_crash_child",
                "--nocapture",
            ])
            .env(PREPARATION_CRASH_CHILD_PROJECT_ROOT, root.path())
            .env("NIB_TEST_SESSION_PREPARATION_PHASE", phase)
            .env("NIB_TEST_SESSION_PREPARATION_READY", &ready)
            .env("NIB_TEST_SESSION_PREPARATION_RESUME", &resume)
            .spawn()
            .expect("spawn phase child");
        let started = Instant::now();
        while !ready.exists() {
            if let Some(status) = child.try_wait().expect("inspect phase child") {
                panic!("phase child exited before {phase} boundary: {status}");
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "phase child did not reach {phase} boundary"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read_to_string(&ready).expect("phase readiness"),
            phase
        );
        let preparation_dir = spawn_preparation_directory_path(root.path());
        let intents = std::fs::read_dir(&preparation_dir)
            .expect("durable preparation namespace")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.path().extension().and_then(|value| value.to_str()) == Some("json")
            })
            .collect::<Vec<_>>();
        assert_eq!(intents.len(), 1, "{phase}: intent precedes mutation");
        let subagent_id = intents[0]
            .path()
            .file_stem()
            .and_then(|value| value.to_str())
            .expect("intent id")
            .to_string();

        child.kill().expect("kill phase child");
        child.wait().expect("reap phase child");
        assert!(
            list_subagents(root.path())
                .unwrap_or_else(|error| panic!("{phase}: restart reconciliation: {error}"))
                .is_empty(),
            "{phase}: preparing state stays private"
        );
        assert!(
            !intents[0].path().exists(),
            "{phase}: intent is removed last"
        );
        assert!(
            !root.path().join(".nib/profiles").exists(),
            "{phase}: exact planned audit hierarchy is removed"
        );
        assert!(!root
            .path()
            .join(".nib/worktrees/subagents")
            .join(&subagent_id)
            .exists());
        assert!(!owner_lease_directory(root.path())
            .read_dir()
            .is_ok_and(|mut entries| entries.next().is_some()));
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn direct_spawn_variants_reject_non_utf8_audit_target_before_any_partial_state() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("UTF-8 project root");
    let non_utf8_state = root
        .path()
        .join(OsString::from_vec(b"profile-state-\xff".to_vec()));
    std::fs::create_dir(&non_utf8_state).expect("non-UTF-8 state target");
    let nib = root.path().join(".nib");
    std::fs::create_dir(&nib).expect("nib state root");
    symlink(&non_utf8_state, nib.join("selected-state")).expect("in-root state symlink");
    let mut config = crate::config::NibConfig::default();
    config.profiles = crate::config::ProfilesConfig {
        default: "selected".to_string(),
        active: vec![crate::config::ProfileConfig {
            id: "selected".to_string(),
            root: PathBuf::from("."),
            state_dir: Some(PathBuf::from(".nib/selected-state")),
            ..crate::config::ProfileConfig::default()
        }],
    };
    crate::config::save_nib_config_full(root.path(), &mut config).expect("profile config");
    assert!(
        non_utf8_state.join("sessions").to_str().is_none(),
        "fixture audit destination must contain non-UTF-8"
    );
    let namespace_before = directory_tree_snapshot(root.path());

    let args = json!({"prompt": "must fail before direct delegation state"});
    let sync_error = spawn_subagent(&args, root.path())
        .expect_err("sync spawn must reject a non-serializable audit target");
    assert_eq!(sync_error, SUBAGENT_AUDIT_DESTINATION_ENCODING_ERROR);
    let cancellable_error = spawn_subagent_cancellable(&args, root.path(), None)
        .await
        .expect_err("cancellable spawn must reject a non-serializable audit target");
    assert_eq!(cancellable_error, SUBAGENT_AUDIT_DESTINATION_ENCODING_ERROR);

    assert_eq!(directory_tree_snapshot(root.path()), namespace_before);
    for path in [
        non_utf8_state.join("sessions"),
        non_utf8_state.join("daemons"),
        non_utf8_state.join("managed-skills"),
        root.path().join(".nib/subagents"),
        root.path().join(".nib/subagent-owner-leases"),
        root.path().join(".nib/worktrees/subagents"),
    ] {
        assert!(
            !path.exists(),
            "direct audit-target failure created delegation state at {}",
            path.display()
        );
    }
}

#[tokio::test]
async fn already_cancelled_cancellable_spawn_is_a_whole_namespace_noop() {
    let root = tempfile::tempdir().expect("project root");
    let before = subagent_namespace_snapshot(root.path());
    let cancellation = crate::agent::CancellationSignal::new();
    assert!(cancellation.cancel());

    let error = spawn_subagent_cancellable(
        &json!({"prompt": "must not mutate"}),
        root.path(),
        Some(&cancellation),
    )
    .await
    .expect_err("already-cancelled spawn must stop before mutation");
    assert!(error.contains("cancelled before mutation"));
    assert_eq!(subagent_namespace_snapshot(root.path()), before);
}

#[test]
fn fallback_child_bootstrap_uses_the_workspace_selected_profile_snapshot() {
    let root = tempfile::tempdir().expect("project root");
    let default_workspace = root.path().join("default-workspace");
    std::fs::create_dir(&default_workspace).expect("default workspace");
    let mut config = crate::config::NibConfig::default();
    config.profiles = crate::config::ProfilesConfig {
        default: "default-profile".to_string(),
        active: vec![
            crate::config::ProfileConfig {
                id: "default-profile".to_string(),
                root: PathBuf::from("default-workspace"),
                env_file: Some(PathBuf::from("default.env")),
                active_skills: vec!["default-secret-skill".to_string()],
                skill_paths: vec![PathBuf::from("default-skills")],
                ..crate::config::ProfileConfig::default()
            },
            crate::config::ProfileConfig {
                id: "selected-profile".to_string(),
                root: PathBuf::from("."),
                env_file: Some(PathBuf::from("selected.env")),
                active_skills: vec!["selected-skill".to_string()],
                skill_paths: vec![PathBuf::from("selected-skills")],
                ..crate::config::ProfileConfig::default()
            },
        ],
    };
    std::fs::write(
        default_workspace.join("default.env"),
        "TOKEN=default-secret\n",
    )
    .expect("default env");
    std::fs::create_dir(default_workspace.join("default-skills")).expect("default skills");
    std::fs::write(root.path().join("selected.env"), "TOKEN=selected-secret\n")
        .expect("selected env");
    std::fs::create_dir(root.path().join("selected-skills")).expect("selected skills");
    crate::config::save_nib_config_full(root.path(), &mut config).expect("profile config");

    let deadline = Instant::now() + Duration::from_secs(5);
    let preflight =
        crate::session::SessionStore::preflight_project_sessions_dir_until(root.path(), deadline)
            .expect("selected profile preflight");
    assert_eq!(
        preflight.runtime_config().profiles.default,
        "selected-profile"
    );

    let child = root.path().join("child-bootstrap");
    std::fs::create_dir(&child).expect("child root");
    std::fs::write(child.join("selected.env"), "TOKEN=selected-secret\n")
        .expect("child selected env");
    std::fs::create_dir(child.join("selected-skills")).expect("child selected skills");
    prepare_child_runtime_config(preflight.runtime_config(), &child).expect("child runtime config");
    let child_config = crate::config::load_nib_config_full_preflight_read_only_until(
        &child,
        Instant::now() + Duration::from_secs(5),
    )
    .expect("child config");
    assert_eq!(child_config.profiles.default, "selected-profile");
    assert_eq!(child_config.profiles.active.len(), 1);
    let selected = &child_config.profiles.active[0];
    assert_eq!(selected.id, "selected-profile");
    assert_eq!(
        selected.env_file.as_deref(),
        Some(Path::new("selected.env"))
    );
    assert_eq!(selected.active_skills, ["selected-skill"]);
    assert!(!serde_json::to_string(&child_config)
        .expect("encoded child config")
        .contains("default-secret"));
}

#[tokio::test]
async fn worktree_and_owner_failures_leave_no_fallback_audit_preparation() {
    let non_git = tempfile::tempdir().expect("non-git project");
    let before = subagent_namespace_snapshot(non_git.path());
    let error = spawn_subagent(&json!({"prompt": "worktree failure"}), non_git.path())
        .expect_err("non-git worktree creation must fail");
    assert!(error.contains("git"));
    assert_subagent_namespace_unchanged(
        &before,
        &subagent_namespace_snapshot(non_git.path()),
        "non-Git failure namespace",
    );

    let root = tempfile::tempdir().expect("git project");
    initialize_spawn_test_repository(root.path());
    ensure_records_directory(root.path()).expect("prime authoritative records namespace");
    prime_fixed_subagent_record_lock_namespace(root.path());
    let primed = crate::sandbox::worktree::Worktree::create(root.path(), "prime-owner")
        .expect("prime worktree namespace");
    crate::sandbox::worktree::Worktree::remove(root.path(), &primed.id)
        .expect("remove priming worktree");
    let before = subagent_namespace_snapshot(root.path());
    SPAWN_OWNER_FAILURES.store(1, std::sync::atomic::Ordering::Release);
    let error = spawn_subagent(&json!({"prompt": "owner failure"}), root.path())
        .expect_err("injected owner creation must fail");
    assert!(error.contains("injected subagent owner creation failure"));
    assert_subagent_namespace_unchanged(
        &before,
        &subagent_namespace_snapshot(root.path()),
        "owner failure namespace",
    );
}

#[tokio::test]
async fn sync_and_cancellable_record_failures_rollback_exact_fallback_audit_preparation() {
    let _timeout = SpawnPreparationTimeoutGuard::set(Duration::from_secs(30));
    for cancellable in [false, true] {
        let root = tempfile::tempdir().expect("git project");
        initialize_spawn_test_repository(root.path());
        ensure_records_directory(root.path()).expect("prime records namespace");
        prime_fixed_subagent_record_lock_namespace(root.path());
        let owner = SubagentOwnerLease::create(root.path()).expect("prime owner namespace");
        owner.remove().expect("remove priming owner");
        let worktree = crate::sandbox::worktree::Worktree::create(root.path(), "prime-record")
            .expect("prime worktree namespace");
        crate::sandbox::worktree::Worktree::remove(root.path(), &worktree.id)
            .expect("remove priming worktree");
        crate::session::SessionStore::for_project(root.path())
            .expect("prime profile session namespace");
        let before = subagent_namespace_snapshot(root.path());

        SPAWN_RECORD_FAILURES.store(1, std::sync::atomic::Ordering::Release);
        let args = json!({"prompt": "record publication failure"});
        let error = if cancellable {
            spawn_subagent_cancellable(&args, root.path(), None)
                .await
                .expect_err("cancellable record publication must fail")
        } else {
            spawn_subagent(&args, root.path()).expect_err("sync record publication must fail")
        };
        assert!(
            error.contains("injected initial subagent record publication failure"),
            "unexpected record-publication failure (cancellable={cancellable}): {error}"
        );
        assert_subagent_namespace_unchanged(
                &before,
                &subagent_namespace_snapshot(root.path()),
                &format!(
                    "record failure left fallback audit/delegation artifacts (cancellable={cancellable})"
                ),
            );
    }
}

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
async fn preparation_supersession_requires_every_persisted_authority_field() {
    // This fixture stages every durable spawn resource before exercising
    // authority mismatch validation. Keep loaded Windows runners from
    // expiring the unrelated positive-progress setup deadline.
    let _timeout = SpawnPreparationTimeoutGuard::set(Duration::from_secs(30));
    let root = tempfile::tempdir().expect("git project");
    initialize_spawn_test_repository(root.path());
    let project_root = canonical_project_root(root.path()).expect("canonical root");
    let id = format!("sub-{}", uuid::Uuid::new_v4());
    let args = json!({"prompt": "exact supersession fixture"});
    let audit_plan = preflight_subagent_audit_target(&args, &project_root).expect("preflight");
    let namespace_plan = audit_plan
        .fallback_namespace_plan(&id, None)
        .expect("namespace plan")
        .expect("fallback plan");
    let worktree_plan =
        crate::sandbox::worktree::Worktree::plan_preparation_authority(&project_root, &id)
            .expect("worktree plan");
    let owner_plan = SubagentOwnerLease::plan();
    let records =
        ensure_records_directory_capability_until(&project_root, None).expect("authorized records");
    let sessions_dir = audit_plan
        .fallback_sessions_dir()
        .expect("fallback sessions")
        .to_path_buf();
    let mut intent = SpawnPreparationIntent::create(
        &records,
        &id,
        owner_plan.clone(),
        worktree_plan.clone(),
        &id,
        &sessions_dir,
        Some(namespace_plan),
        None,
    )
    .expect("planned intent");
    let worktree = crate::sandbox::worktree::Worktree::create_from_preparation_authority(
        &project_root,
        &worktree_plan,
    )
    .expect("planned worktree");
    let owner = create_spawn_owner_lease(&project_root, &owner_plan, &intent.authority)
        .expect("planned owner");
    intent
        .revise(SpawnPreparationPhase::ResourcesPrepared, None, None, None)
        .expect("resource revision");
    let prepared =
        commit_subagent_audit_target(audit_plan, &id, Some(&worktree), Some(&mut intent))
            .expect("audit preparation");
    let target = prepared.encoded.clone();
    let base = SubagentRecord {
        id: id.clone(),
        parent_session_id: None,
        child_session_id: id.clone(),
        prompt: "fixture".to_string(),
        status: "running".to_string(),
        execution_generation: Some(owner_plan.execution_generation),
        owner_lease: Some(owner_plan.lease_id.clone()),
        worktree_path: worktree.path.clone(),
        branch: worktree.branch.clone(),
        branch_oid: Some(worktree.branch_oid.clone()),
        result: Some(json!({
            "_ownership_audit_target": target,
            "_worktree_ownership_receipt": worktree.preparation_authority().ownership_receipt_id,
        })),
        error: None,
        verification: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    validate_spawn_intent_record_identity(&intent.data, &base).expect("exact record identity");

    let mut mismatches = Vec::new();
    let mut changed = base.clone();
    changed.execution_generation = Some(owner_plan.execution_generation.saturating_add(1));
    mismatches.push(changed);
    let mut changed = base.clone();
    changed.owner_lease = Some(uuid::Uuid::new_v4().to_string());
    mismatches.push(changed);
    let mut changed = base.clone();
    changed.worktree_path = project_root.join("replacement-worktree");
    mismatches.push(changed);
    let mut changed = base.clone();
    changed.branch.push_str("-replacement");
    mismatches.push(changed);
    let mut changed = base.clone();
    changed.branch_oid = Some("0".repeat(40));
    mismatches.push(changed);
    let mut changed = base.clone();
    changed.child_session_id.push_str("-replacement");
    mismatches.push(changed);
    let mut changed = base.clone();
    changed.status = "preparing".to_string();
    mismatches.push(changed);
    let mut changed = base.clone();
    changed.result.as_mut().expect("result")[WORKTREE_PREPARATION_RECEIPT_KEY] =
        Value::String(uuid::Uuid::new_v4().to_string());
    mismatches.push(changed);
    let mut changed = base.clone();
    changed.result.as_mut().expect("result")[OWNERSHIP_AUDIT_TARGET_KEY]["sessions_dir"] =
        Value::String("replacement".to_string());
    mismatches.push(changed);
    for changed in mismatches {
        let error = validate_spawn_intent_record_identity(&intent.data, &changed)
            .expect_err("mismatched authority must preserve preparation");
        assert!(error.contains("does not exactly match"), "{error}");
    }

    prepared.cleanup().expect("audit cleanup");
    cleanup_precommit_worktree(&project_root, &worktree)
        .await
        .expect("worktree cleanup");
    owner.remove().expect("owner cleanup");
    intent.cleanup().expect("intent cleanup");
}

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn handoff_execution_evidence_requires_exact_committed_scope_authority() {
    let root = tempfile::tempdir().expect("git project");
    initialize_spawn_test_repository(root.path());
    let project_root = canonical_project_root(root.path()).expect("canonical root");
    let id = format!("sub-{}", uuid::Uuid::new_v4());
    let records =
        ensure_records_directory_capability_until(&project_root, None).expect("authorized records");
    let generation = 7_001;
    let owner_plan = SubagentOwnerPlan {
        execution_generation: generation,
        lease_id: uuid::Uuid::new_v4().to_string(),
    };
    let worktree =
        crate::sandbox::worktree::Worktree::plan_preparation_authority(&project_root, &id)
            .expect("worktree plan");
    let store =
        crate::sandbox::process::ProcessScopeStore::open(&project_root).expect("scope store");
    let process_scope_plan = SubagentProcessScopePlan {
        cleanup_lease_id: uuid::Uuid::new_v4().to_string(),
        supervisor_registration_nonce: uuid::Uuid::new_v4().to_string(),
    };
    let prepared = store
        .prepare_subagent_launch(
            &id,
            generation,
            &process_scope_plan.cleanup_lease_id,
            &process_scope_plan.supervisor_registration_nonce,
            crate::sandbox::process::ProcessIdentity::current().expect("owner identity"),
            crate::sandbox::process::ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepared scope");
    let identity = crate::sandbox::process::ProcessIdentity::current().expect("execution identity");
    let mut ready = prepared.clone();
    ready.status = crate::sandbox::process::ProcessScopeStatus::Running;
    ready.launch_committed = Some(false);
    ready.supervisor = Some(identity.clone());
    ready.direct_child = Some(identity);
    ready.updated_at = Utc::now();
    let intent = SpawnPreparationIntentData {
        version: SPAWN_PREPARATION_VERSION,
        revision: 5,
        phase: SpawnPreparationPhase::HandoffProven,
        subagent_id: id.clone(),
        owner: owner_plan.clone(),
        worktree,
        audit_session_id: id.clone(),
        audit_sessions_dir: project_root.join(".nib/test-audit-sessions"),
        audit_namespace_plan: None,
        audit_target: None,
        audit_receipt: None,
        process_scope_plan: Some(process_scope_plan),
        handoff_process_scope: Some(ready.clone()),
        created_at: Utc::now(),
    };
    let mut record = record_fixture(&project_root, &id, "running");
    record.execution_generation = Some(generation);
    record.owner_lease = Some(owner_plan.lease_id.clone());
    let scope_path = project_root
        .join(".nib/process-scopes")
        .join(format!("{id}.json"));
    let install = |scope: &crate::sandbox::process::ProcessScopeRecord| {
        std::fs::write(
            &scope_path,
            serde_json::to_vec_pretty(scope).expect("encode scope fixture"),
        )
        .expect("install scope fixture");
    };
    let evidence_for = |candidate: &SpawnPreparationIntentData, record: &SubagentRecord| {
        spawn_handoff_has_execution_evidence(
            &project_root,
            &records,
            candidate,
            record,
            Instant::now() + Duration::from_secs(2),
        )
    };
    let evidence = |record: &SubagentRecord| evidence_for(&intent, record);

    let mut committed_control = ready.clone();
    committed_control.launch_committed = Some(true);
    committed_control.updated_at = Utc::now();
    install(&committed_control);
    assert!(evidence(&record).expect("matching v4 plan and READY scope"));
    let mut changed_ready_authorities = Vec::new();
    let mut changed = intent.clone();
    changed
        .handoff_process_scope
        .as_mut()
        .expect("READY scope")
        .cleanup_lease_id = uuid::Uuid::new_v4().to_string();
    changed_ready_authorities.push(changed);
    let mut changed = intent.clone();
    changed
        .handoff_process_scope
        .as_mut()
        .expect("READY scope")
        .supervisor_registration_nonce = Some(uuid::Uuid::new_v4().to_string());
    changed_ready_authorities.push(changed);
    let mut changed = intent.clone();
    changed
        .handoff_process_scope
        .as_mut()
        .expect("READY scope")
        .supervisor_registration_nonce = None;
    changed_ready_authorities.push(changed);
    let committed_namespace = subagent_namespace_snapshot(root.path());
    for changed in &changed_ready_authorities {
        assert!(
            evidence_for(changed, &record).is_err(),
            "changed or missing READY plan authority must fail closed"
        );
        assert_eq!(
            subagent_namespace_snapshot(root.path()),
            committed_namespace,
            "rejected READY authority changed intent or resource bytes"
        );
    }

    for status in [
        crate::sandbox::process::ProcessScopeStatus::Running,
        crate::sandbox::process::ProcessScopeStatus::CleanupInProgress,
        crate::sandbox::process::ProcessScopeStatus::RecoveryRequired,
    ] {
        let mut committed = ready.clone();
        committed.status = status;
        committed.launch_committed = Some(true);
        committed.cleanup_reason = (status != crate::sandbox::process::ProcessScopeStatus::Running)
            .then(|| "committed recovery fixture".to_string());
        committed.updated_at = Utc::now();
        install(&committed);
        assert!(evidence(&record).expect("exact committed scope evidence"));
    }

    for launch_committed in [Some(false), None] {
        let mut uncommitted = ready.clone();
        uncommitted.launch_committed = launch_committed;
        uncommitted.updated_at = Utc::now();
        install(&uncommitted);
        assert!(
            !matches!(evidence(&record), Ok(true)),
            "uncommitted or legacy scope must never supersede its intent"
        );
    }
    install(&prepared);
    assert!(
        !matches!(evidence(&record), Ok(true)),
        "Prepared must never supersede its intent"
    );
    let mut legacy_prepared = prepared.clone();
    legacy_prepared.launch_committed = None;
    install(&legacy_prepared);
    assert!(
        !matches!(evidence(&record), Ok(true)),
        "legacy Prepared must never supersede its intent"
    );
    let mut recovery_uncommitted = ready.clone();
    recovery_uncommitted.status = crate::sandbox::process::ProcessScopeStatus::RecoveryRequired;
    recovery_uncommitted.launch_committed = Some(false);
    recovery_uncommitted.cleanup_reason = Some("uncommitted recovery fixture".to_string());
    install(&recovery_uncommitted);
    assert!(
        !matches!(evidence(&record), Ok(true)),
        "uncommitted RecoveryRequired must never supersede its intent"
    );

    for mut mismatched in [
        {
            let mut scope = ready.clone();
            scope.workload_kind = "daemon".to_string();
            scope
        },
        {
            let mut scope = ready.clone();
            scope.execution_generation += 1;
            scope
        },
        {
            let mut scope = ready.clone();
            scope.cleanup_lease_id = uuid::Uuid::new_v4().to_string();
            scope
        },
        {
            let mut scope = ready.clone();
            scope.owner.start_marker.push_str("-other");
            scope
        },
    ] {
        mismatched.launch_committed = Some(true);
        install(&mismatched);
        assert!(
            evidence(&record).is_err(),
            "mismatched execution authority must fail closed"
        );
    }

    let mut wrong_id = ready.clone();
    wrong_id.scope_id = format!("{id}-wrong");
    wrong_id.launch_committed = Some(true);
    install(&wrong_id);
    assert!(
        evidence(&record).is_err(),
        "wrong scope id must fail closed"
    );

    let mut complete = ready.clone();
    complete.status = crate::sandbox::process::ProcessScopeStatus::Complete;
    complete.launch_committed = Some(true);
    complete.cleanup_reason = Some("completed evidence fixture".to_string());
    let proof = crate::sandbox::process::CleanupProof {
        execution_generation: generation,
        cleanup_lease_id: complete.cleanup_lease_id.clone(),
        backend: complete.backend,
        direct_child: complete.direct_child.clone().expect("direct child"),
        outcome: "completed evidence fixture".to_string(),
        descendants_reaped: true,
        completed_at: Utc::now(),
    };
    complete.cleanup_proof = Some(proof.clone());
    complete.updated_at = Utc::now();
    install(&complete);
    record.status = "failed".to_string();
    record.result = Some(json!({
        "cleanup_verified": true,
        "cleanup_proof": proof,
    }));
    assert!(evidence(&record).expect("exact terminal cleanup evidence"));
    record.result.as_mut().expect("terminal result")["cleanup_proof"]["outcome"] =
        Value::String("different proof".to_string());
    assert!(
        evidence(&record).is_err(),
        "terminal proof mismatch must fail closed"
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn legacy_preparation_schema_and_evidence_reject_v2_ready_without_mutation() {
    let root = tempfile::tempdir().expect("git project");
    initialize_spawn_test_repository(root.path());
    let project_root = canonical_project_root(root.path()).expect("canonical root");
    let records =
        ensure_records_directory_capability_until(&project_root, None).expect("authorized records");
    let audit_store =
        crate::session::SessionStore::for_project(&project_root).expect("audit store");
    let audit_target = subagent_audit_target_for_store(&audit_store).expect("audit target");
    let id = format!("sub-{}", uuid::Uuid::new_v4());
    let generation = 7_101;
    let owner = SubagentOwnerPlan {
        execution_generation: generation,
        lease_id: uuid::Uuid::new_v4().to_string(),
    };
    let worktree =
        crate::sandbox::worktree::Worktree::plan_preparation_authority(&project_root, &id)
            .expect("worktree plan");
    let process_store =
        crate::sandbox::process::ProcessScopeStore::open(&project_root).expect("scope store");
    let prepared = process_store
        .prepare_subagent_launch(
            &id,
            generation,
            &uuid::Uuid::new_v4().to_string(),
            &uuid::Uuid::new_v4().to_string(),
            crate::sandbox::process::ProcessIdentity::current().expect("owner identity"),
            crate::sandbox::process::ProcessScopeBackend::LinuxPidNamespace,
        )
        .expect("prepared scope");
    let execution =
        crate::sandbox::process::ProcessIdentity::current().expect("execution identity");
    let mut ready = prepared;
    ready.status = crate::sandbox::process::ProcessScopeStatus::Running;
    ready.launch_committed = Some(false);
    ready.supervisor = Some(execution.clone());
    ready.direct_child = Some(execution);
    ready.updated_at = Utc::now();
    let valid_v2 = SpawnPreparationIntentData {
        version: LEGACY_SPAWN_PREPARATION_VERSION,
        revision: 5,
        phase: SpawnPreparationPhase::HandoffProven,
        subagent_id: id.clone(),
        owner: owner.clone(),
        worktree,
        audit_session_id: id.clone(),
        audit_sessions_dir: audit_target.sessions_dir.clone(),
        audit_namespace_plan: None,
        audit_target: Some(audit_target),
        audit_receipt: None,
        process_scope_plan: None,
        handoff_process_scope: None,
        created_at: Utc::now(),
    };
    validate_spawn_preparation_intent_structure(&valid_v2).expect("valid v2 structure");
    encode_spawn_preparation_intent(&valid_v2).expect("valid v2 encoding");

    let mut record = record_fixture(&project_root, &id, "running");
    record.execution_generation = Some(generation);
    record.owner_lease = Some(owner.lease_id.clone());
    let mut committed = ready.clone();
    committed.launch_committed = Some(true);
    committed.updated_at = Utc::now();
    std::fs::write(
        project_root
            .join(".nib/process-scopes")
            .join(format!("{id}.json")),
        serde_json::to_vec_pretty(&committed).expect("encode committed scope"),
    )
    .expect("install committed scope");
    let before = subagent_namespace_snapshot(root.path());
    assert!(
        spawn_handoff_has_execution_evidence(
            &project_root,
            &records,
            &valid_v2,
            &record,
            Instant::now() + Duration::from_secs(2),
        )
        .is_err(),
        "v2 cannot infer exact handoff evidence from a matching committed scope"
    );
    assert_eq!(subagent_namespace_snapshot(root.path()), before);

    let mut v2_with_ready = valid_v2.clone();
    v2_with_ready.handoff_process_scope = Some(ready.clone());
    assert!(validate_spawn_preparation_intent_structure(&v2_with_ready).is_err());
    assert!(encode_spawn_preparation_intent(&v2_with_ready).is_err());
    assert!(
        spawn_handoff_has_execution_evidence(
            &project_root,
            &records,
            &v2_with_ready,
            &record,
            Instant::now() + Duration::from_secs(2),
        )
        .is_err(),
        "matching disk evidence cannot legitimize a forbidden v2 READY field"
    );
    assert_eq!(subagent_namespace_snapshot(root.path()), before);

    let mut valid_v3 = valid_v2.clone();
    valid_v3.version = HANDOFF_SCOPE_SPAWN_PREPARATION_VERSION;
    valid_v3.handoff_process_scope = Some(ready);
    validate_spawn_preparation_intent_structure(&valid_v3).expect("valid v3 READY structure");
    encode_spawn_preparation_intent(&valid_v3).expect("valid v3 encoding");
    assert!(
        spawn_handoff_has_execution_evidence(
            &project_root,
            &records,
            &valid_v3,
            &record,
            Instant::now() + Duration::from_secs(2),
        )
        .expect("exact v3 evidence"),
        "valid v3 READY must retain its exact legacy evidence semantics"
    );
    assert_eq!(subagent_namespace_snapshot(root.path()), before);
    let mut mismatched_v3 = valid_v3.clone();
    mismatched_v3
        .handoff_process_scope
        .as_mut()
        .expect("v3 READY")
        .cleanup_lease_id = uuid::Uuid::new_v4().to_string();
    assert!(
        spawn_handoff_has_execution_evidence(
            &project_root,
            &records,
            &mismatched_v3,
            &record,
            Instant::now() + Duration::from_secs(2),
        )
        .is_err(),
        "v3 evidence must fail closed when its exact READY authority differs"
    );
    assert_eq!(subagent_namespace_snapshot(root.path()), before);

    let mut v3_without_ready = valid_v3.clone();
    v3_without_ready.handoff_process_scope = None;
    assert!(validate_spawn_preparation_intent_structure(&v3_without_ready).is_err());
    let mut v3_ready_too_early = valid_v3;
    v3_ready_too_early.phase = SpawnPreparationPhase::ManagerRegistered;
    v3_ready_too_early.revision = 4;
    assert!(validate_spawn_preparation_intent_structure(&v3_ready_too_early).is_err());
    assert_eq!(subagent_namespace_snapshot(root.path()), before);
}

#[test]
fn atomic_preparation_revision_rejects_process_plan_substitution_byte_exactly() {
    for variant in ["cleanup-lease", "registration-nonce", "matching-control"] {
        let root = tempfile::tempdir().expect("git project");
        initialize_spawn_test_repository(root.path());
        let project_root = canonical_project_root(root.path()).expect("canonical root");
        let id = format!("sub-{}", uuid::Uuid::new_v4());
        let args = json!({"prompt": "atomic process plan fixture"});
        let audit_plan =
            preflight_subagent_audit_target(&args, &project_root).expect("audit preflight");
        let records = ensure_records_directory_capability_until(&project_root, None)
            .expect("authorized records");
        let namespace_plan = audit_plan
            .fallback_namespace_plan_after_records(&id, &records)
            .expect("namespace plan")
            .expect("fallback namespace plan");
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
        let previous = intent
            .directory
            .deterministic_previous_artifact_path(&intent.path, ".nib-subagent-preparation-")
            .expect("previous revision path");
        let mut successor = intent.data.clone();
        successor.revision = 1;
        successor.phase = SpawnPreparationPhase::ResourcesPrepared;
        match variant {
            "cleanup-lease" => {
                successor
                    .process_scope_plan
                    .as_mut()
                    .expect("v4 plan")
                    .cleanup_lease_id = uuid::Uuid::new_v4().to_string();
            }
            "registration-nonce" => {
                successor
                    .process_scope_plan
                    .as_mut()
                    .expect("v4 plan")
                    .supervisor_registration_nonce = uuid::Uuid::new_v4().to_string();
            }
            "matching-control" => {}
            _ => unreachable!(),
        }
        let successor_bytes = encode_spawn_preparation_intent(&successor).expect("successor bytes");
        std::fs::rename(&intent.path, &previous).expect("evacuate prior revision");
        std::fs::write(&intent.path, &successor_bytes).expect("publish successor fixture");
        let before = subagent_namespace_snapshot(root.path());
        let result = recover_spawn_preparation_transactions(
            &intent.directory,
            &records,
            Instant::now() + Duration::from_secs(2),
        );
        if variant == "matching-control" {
            result.expect("matching immutable plan finalizes prior revision");
            assert!(!previous.exists(), "matching prior revision was finalized");
            assert_eq!(
                std::fs::read(&intent.path).expect("matching successor"),
                successor_bytes
            );
        } else {
            let error = result.expect_err("substituted v4 plan must fail closed");
            assert!(error.contains("ambiguous"), "{error}");
            assert_eq!(
                subagent_namespace_snapshot(root.path()),
                before,
                "atomic {variant} substitution changed intent or resource bytes"
            );
        }
        drop(intent);
    }
}

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn atomic_legacy_preparation_recovery_rejects_v2_ready_in_every_artifact() {
    for variant in [
        "temporary-v2-ready",
        "previous-v2-ready",
        "target-v2-ready",
        "valid-v2-control",
        "valid-v3-control",
    ] {
        let root = tempfile::tempdir().expect("git project");
        initialize_spawn_test_repository(root.path());
        let project_root = canonical_project_root(root.path()).expect("canonical root");
        let records = ensure_records_directory_capability_until(&project_root, None)
            .expect("authorized records");
        let audit_store =
            crate::session::SessionStore::for_project(&project_root).expect("audit store");
        let audit_target = subagent_audit_target_for_store(&audit_store).expect("audit target");
        let audit_sessions_dir = audit_target.sessions_dir.clone();
        let id = format!("sub-{}", uuid::Uuid::new_v4());
        let intent = SpawnPreparationIntent::create(
            &records,
            &id,
            SubagentOwnerLease::plan(),
            crate::sandbox::worktree::Worktree::plan_preparation_authority(&project_root, &id)
                .expect("worktree plan"),
            &id,
            &audit_sessions_dir,
            None,
            Some(audit_target),
        )
        .expect("planned intent");
        let mut legacy = intent.data.clone();
        legacy.version = if variant == "valid-v3-control" {
            HANDOFF_SCOPE_SPAWN_PREPARATION_VERSION
        } else {
            LEGACY_SPAWN_PREPARATION_VERSION
        };
        legacy.process_scope_plan = None;
        legacy.handoff_process_scope = None;
        let valid_planned = encode_spawn_preparation_intent(&legacy).expect("legacy planned");
        std::fs::write(&intent.path, &valid_planned).expect("install legacy planned intent");

        #[cfg(target_os = "linux")]
        let backend = crate::sandbox::process::ProcessScopeBackend::LinuxPidNamespace;
        #[cfg(windows)]
        let backend = crate::sandbox::process::ProcessScopeBackend::WindowsJobObject;
        #[cfg(target_os = "macos")]
        let backend = crate::sandbox::process::ProcessScopeBackend::MacosProcessGroup;
        let identity =
            crate::sandbox::process::ProcessIdentity::current().expect("process identity");
        let now = Utc::now();
        let ready = crate::sandbox::process::ProcessScopeRecord {
            version: 2,
            scope_id: id.clone(),
            workload_kind: "subagent".to_string(),
            execution_generation: legacy.owner.execution_generation,
            cleanup_lease_id: uuid::Uuid::new_v4().to_string(),
            supervisor_registration_nonce: None,
            owner: identity.clone(),
            backend,
            status: crate::sandbox::process::ProcessScopeStatus::Running,
            launch_committed: Some(false),
            supervisor: Some(identity.clone()),
            direct_child: Some(identity),
            cleanup_reason: None,
            cleanup_proof: None,
            launch_abort_proof: None,
            created_at: now,
            updated_at: now,
        };
        let previous = intent
            .directory
            .deterministic_previous_artifact_path(&intent.path, ".nib-subagent-preparation-")
            .expect("previous artifact");
        let temporary = intent
            .directory
            .deterministic_artifact_path(&intent.path, ".nib-subagent-preparation-", ".tmp")
            .expect("temporary artifact");
        let mut manager = legacy.clone();
        manager.phase = SpawnPreparationPhase::ManagerRegistered;
        manager.revision = 4;
        let mut proven = manager.clone();
        proven.phase = SpawnPreparationPhase::HandoffProven;
        proven.revision = 5;

        match variant {
            "temporary-v2-ready" => {
                let mut invalid = legacy.clone();
                invalid.handoff_process_scope = Some(ready.clone());
                std::fs::write(
                    &temporary,
                    serde_json::to_vec_pretty(&invalid).expect("invalid temp bytes"),
                )
                .expect("install invalid temporary");
            }
            "previous-v2-ready" => {
                let mut invalid = manager.clone();
                invalid.handoff_process_scope = Some(ready.clone());
                std::fs::rename(&intent.path, &previous).expect("evacuate prior intent");
                std::fs::write(
                    &previous,
                    serde_json::to_vec_pretty(&invalid).expect("invalid previous bytes"),
                )
                .expect("install invalid previous");
            }
            "target-v2-ready" => {
                std::fs::write(
                    &intent.path,
                    encode_spawn_preparation_intent(&manager).expect("valid v2 prior"),
                )
                .expect("install valid v2 prior");
                std::fs::rename(&intent.path, &previous).expect("evacuate valid prior");
                proven.handoff_process_scope = Some(ready.clone());
                std::fs::write(
                    &intent.path,
                    serde_json::to_vec_pretty(&proven).expect("invalid target bytes"),
                )
                .expect("install invalid target");
            }
            "valid-v2-control" => {
                std::fs::write(
                    &intent.path,
                    encode_spawn_preparation_intent(&manager).expect("valid v2 prior"),
                )
                .expect("install valid v2 prior");
                std::fs::rename(&intent.path, &previous).expect("evacuate valid v2 prior");
                std::fs::write(
                    &intent.path,
                    encode_spawn_preparation_intent(&proven).expect("valid v2 target"),
                )
                .expect("install valid v2 target");
            }
            "valid-v3-control" => {
                std::fs::write(
                    &intent.path,
                    encode_spawn_preparation_intent(&manager).expect("valid v3 prior"),
                )
                .expect("install valid v3 prior");
                std::fs::rename(&intent.path, &previous).expect("evacuate valid v3 prior");
                proven.handoff_process_scope = Some(ready);
                std::fs::write(
                    &intent.path,
                    encode_spawn_preparation_intent(&proven).expect("valid v3 target"),
                )
                .expect("install valid v3 target");
            }
            _ => unreachable!(),
        }

        let before = subagent_namespace_snapshot(root.path());
        let result = recover_spawn_preparation_transactions(
            &intent.directory,
            &records,
            Instant::now() + Duration::from_secs(2),
        );
        if variant.starts_with("valid-") {
            result.expect("valid legacy atomic successor");
            assert!(!previous.exists(), "valid prior artifact was finalized");
            assert!(intent.path.is_file(), "valid target remains canonical");
        } else {
            let error = result.expect_err("v2 READY injection must fail closed");
            assert!(error.contains("preserved"), "{variant}: {error}");
            assert_eq!(
                subagent_namespace_snapshot(root.path()),
                before,
                "{variant} changed the project namespace"
            );
        }
        drop(intent);
    }
}

#[test]
fn restart_adopts_exact_intent_delete_quarantine_and_rejects_ambiguity() {
    let root = tempfile::tempdir().expect("git project");
    initialize_spawn_test_repository(root.path());
    let project_root = canonical_project_root(root.path()).expect("canonical root");
    let id = format!("sub-{}", uuid::Uuid::new_v4());
    let args = json!({"prompt": "quarantine retry fixture"});
    let audit_plan = preflight_subagent_audit_target(&args, &project_root).expect("preflight");
    let worktree_plan =
        crate::sandbox::worktree::Worktree::plan_preparation_authority(&project_root, &id)
            .expect("worktree plan");
    let owner_plan = SubagentOwnerLease::plan();
    let records =
        ensure_records_directory_capability_until(&project_root, None).expect("authorized records");
    let namespace_plan = audit_plan
        .fallback_namespace_plan_after_records(&id, &records)
        .expect("namespace plan")
        .expect("fallback plan");
    let sessions_dir = audit_plan
        .fallback_sessions_dir()
        .expect("fallback sessions")
        .to_path_buf();
    let intent = SpawnPreparationIntent::create(
        &records,
        &id,
        owner_plan,
        worktree_plan,
        &id,
        &sessions_dir,
        Some(namespace_plan),
        None,
    )
    .expect("planned intent");
    let quarantine = intent
        .directory
        .deterministic_artifact_path(
            &intent.path,
            ".nib-subagent-preparation-delete-",
            ".quarantine",
        )
        .expect("quarantine path");
    std::fs::rename(&intent.path, &quarantine).expect("simulate interrupted quarantine delete");
    drop(intent);
    assert!(list_subagents(&project_root)
        .expect("fresh retry adopts exact quarantine")
        .is_empty());
    assert!(!quarantine.exists(), "exact quarantine removed on retry");

    let root = tempfile::tempdir().expect("ambiguous git project");
    initialize_spawn_test_repository(root.path());
    let project_root = canonical_project_root(root.path()).expect("canonical root");
    let records =
        ensure_records_directory_capability_until(&project_root, None).expect("authorized records");
    let id = format!("sub-{}", uuid::Uuid::new_v4());
    let audit_plan = preflight_subagent_audit_target(&args, &project_root).expect("preflight");
    let namespace_plan = audit_plan
        .fallback_namespace_plan_after_records(&id, &records)
        .expect("namespace plan")
        .expect("fallback plan");
    let worktree_plan =
        crate::sandbox::worktree::Worktree::plan_preparation_authority(&project_root, &id)
            .expect("worktree plan");
    let intent = SpawnPreparationIntent::create(
        &records,
        &id,
        SubagentOwnerLease::plan(),
        worktree_plan,
        &id,
        audit_plan.fallback_sessions_dir().expect("sessions"),
        Some(namespace_plan),
        None,
    )
    .expect("second planned intent");
    let quarantine = intent
        .directory
        .deterministic_artifact_path(
            &intent.path,
            ".nib-subagent-preparation-delete-",
            ".quarantine",
        )
        .expect("quarantine path");
    std::fs::copy(&intent.path, &quarantine).expect("install ambiguous quarantine");
    drop(intent);
    let before = subagent_namespace_snapshot(root.path());
    let error = list_subagents(&project_root).expect_err("ambiguity must fail closed");
    assert!(
        error.contains("ambiguous canonical and quarantine"),
        "{error}"
    );
    assert_subagent_namespace_unchanged(
        &before,
        &subagent_namespace_snapshot(root.path()),
        "ambiguous intent quarantine",
    );
}

#[test]
fn live_planned_intent_blocks_list_and_get_reconciliation_until_writer_retires() {
    let root = tempfile::tempdir().expect("git project");
    initialize_spawn_test_repository(root.path());
    let project_root = canonical_project_root(root.path()).expect("canonical root");
    let id = format!("sub-{}", uuid::Uuid::new_v4());
    let args = json!({"prompt": "live planned intent fixture"});
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

    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let list_root = project_root.clone();
    let list_started = started_tx.clone();
    let list_results = result_tx.clone();
    let list_thread = std::thread::spawn(move || {
        list_started.send("list").expect("announce list start");
        list_results
            .send((
                "list",
                list_subagents(&list_root).map(|records| records.len()),
            ))
            .expect("send list result");
    });
    let get_root = project_root.clone();
    let get_id = id.clone();
    let get_thread = std::thread::spawn(move || {
        started_tx.send("get").expect("announce get start");
        result_tx
            .send((
                "get",
                get_subagent_record(&get_root, &get_id).map(|_| 1_usize),
            ))
            .expect("send get result");
    });
    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("first reader started");
    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("second reader started");
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        matches!(
            result_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ),
        "a reader entered reconciliation while the planned writer held its lifetime authority"
    );
    assert!(intent.path.exists(), "live planned intent was preserved");

    intent.cleanup().expect("writer retires intent");
    let first = result_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("first reader completes after retirement");
    let second = result_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("second reader completes after retirement");
    for (kind, result) in [first, second] {
        match kind {
            "list" => assert_eq!(result.expect("list after retirement"), 0),
            "get" => assert!(result.is_err(), "missing record must remain missing"),
            _ => panic!("unexpected reader kind"),
        }
    }
    list_thread.join().expect("join list reader");
    get_thread.join().expect("join get reader");
}
