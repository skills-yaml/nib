use super::*;

#[cfg(unix)]
#[test]
fn preparation_reconciliation_rejects_records_replacement_after_intent_open() {
    let root = tempfile::tempdir().expect("git project");
    initialize_spawn_test_repository(root.path());
    let project_root = canonical_project_root(root.path()).expect("canonical root");
    let id = format!("sub-{}", uuid::Uuid::new_v4());
    let args = json!({"prompt": "records replacement fixture"});
    let audit_plan = preflight_subagent_audit_target(&args, &project_root).expect("preflight");
    let worktree_plan =
        crate::sandbox::worktree::Worktree::plan_preparation_authority(&project_root, &id)
            .expect("worktree plan");
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
        worktree_plan,
        &id,
        audit_plan.fallback_sessions_dir().expect("sessions"),
        Some(namespace_plan),
        None,
    )
    .expect("planned intent");
    drop(intent);
    let original_before = directory_tree_snapshot(records.path());
    let records_path = records.path().to_path_buf();
    let displaced = project_root.join(".nib/subagents.displaced-preparation");
    let displaced_for_hook = displaced.clone();
    let replacement_before = std::sync::Arc::new(std::sync::Mutex::new(None));
    let replacement_capture = replacement_before.clone();
    AFTER_PREPARATION_INTENT_OPEN_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            std::fs::rename(&records_path, &displaced_for_hook).expect("displace records");
            std::fs::create_dir(&records_path).expect("create replacement records");
            std::fs::write(records_path.join("replacement.sentinel"), b"replacement")
                .expect("replacement sentinel");
            *replacement_capture
                .lock()
                .expect("replacement snapshot lock") = Some(directory_tree_snapshot(&records_path));
        }));
    });
    let error = list_subagents(&project_root)
        .expect_err("detached preparation capability must fail closed");
    assert!(
        error.contains("identity changed") || error.contains("no longer attached"),
        "{error}"
    );
    assert_eq!(directory_tree_snapshot(&displaced), original_before);
    assert_eq!(
        directory_tree_snapshot(&project_root.join(".nib/subagents")),
        replacement_before
            .lock()
            .expect("replacement snapshot lock")
            .clone()
            .expect("replacement snapshot")
    );
    assert!(displaced
        .join(".preparations")
        .join(format!("{id}.json"))
        .exists());
}

#[cfg(unix)]
#[tokio::test]
async fn spawn_forward_mutations_stop_at_detached_preparation_authority() {
    for cancellable in [false, true] {
        for boundary in ["worktree", "child_config", "owner", "audit"] {
            let root = tempfile::tempdir().expect("git project");
            initialize_spawn_test_repository(root.path());
            ensure_records_directory(root.path()).expect("authorized records namespace");
            let records_path = records_dir(root.path());
            let displaced = root
                .path()
                .join(format!(".records-displaced-{boundary}-{cancellable}"));
            let expected_after_detach = std::sync::Arc::new(std::sync::Mutex::new(None));
            let expected_capture = expected_after_detach.clone();
            let root_path = root.path().to_path_buf();
            let requested = boundary;
            let mut replaced = false;
            SPAWN_FORWARD_MUTATION_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move |observed| {
                    if replaced || observed != requested {
                        return;
                    }
                    replaced = true;
                    std::fs::rename(&records_path, &displaced)
                        .expect("detach authoritative records directory");
                    std::fs::create_dir(&records_path)
                        .expect("create replacement records directory");
                    std::fs::write(records_path.join("replacement.sentinel"), b"replacement")
                        .expect("write replacement records sentinel");
                    *expected_capture.lock().expect("snapshot lock") =
                        Some(directory_tree_snapshot(&root_path));
                }));
            });
            let args = json!({"prompt": format!("detach before {boundary}")});
            let result = if cancellable {
                spawn_subagent_cancellable(&args, root.path(), None).await
            } else {
                spawn_subagent(&args, root.path())
            };
            SPAWN_FORWARD_MUTATION_HOOK.with(|hook| hook.borrow_mut().take());
            let error = result.expect_err("detached preparation authority must fail closed");
            assert!(
                error.contains("identity changed")
                    || error.contains("no longer attached")
                    || error.contains("state directory changed"),
                "{boundary} cancellable={cancellable}: {error}"
            );
            let expected = expected_after_detach
                .lock()
                .expect("snapshot lock")
                .clone()
                .expect("replacement boundary ran");
            assert_eq!(
                directory_tree_snapshot(root.path()),
                expected,
                "{boundary} cancellable={cancellable} mutated a resource after records detachment"
            );
        }
    }
}

#[tokio::test]
async fn partial_cleanup_failures_retain_intent_until_restart_reconciles_last() {
    #[cfg(windows)]
    let _timeout = SpawnPreparationTimeoutGuard::set(Duration::from_secs(10));
    for cancellable in [false, true] {
        for failure in ["session", "worktree", "owner", "audit"] {
            let root = tempfile::tempdir().expect("git project");
            initialize_spawn_test_repository(root.path());
            ensure_records_directory(root.path()).expect("authorized records namespace");
            let primed_owner = SubagentOwnerLease::create(root.path()).expect("prime owner");
            primed_owner.remove().expect("remove primed owner");
            let primed_worktree =
                crate::sandbox::worktree::Worktree::create(root.path(), "cleanup-prime")
                    .expect("prime worktree");
            crate::sandbox::worktree::Worktree::remove(root.path(), &primed_worktree.id)
                .expect("remove primed worktree");
            crate::session::SessionStore::for_project(root.path())
                .expect("prime fallback session namespace");
            let before = subagent_namespace_snapshot(root.path());

            match failure {
                "session" => {
                    SPAWN_SESSION_PUBLICATION_FAILURES
                        .store(1, std::sync::atomic::Ordering::Release);
                    SPAWN_SESSION_CLEANUP_FAILURES.store(1, std::sync::atomic::Ordering::Release);
                }
                "worktree" => {
                    SPAWN_RECORD_FAILURES.store(1, std::sync::atomic::Ordering::Release);
                    SPAWN_WORKTREE_CLEANUP_FAILURES.store(1, std::sync::atomic::Ordering::Release);
                }
                "owner" => {
                    SPAWN_RECORD_FAILURES.store(1, std::sync::atomic::Ordering::Release);
                    SPAWN_OWNER_CLEANUP_FAILURES.store(1, std::sync::atomic::Ordering::Release);
                }
                "audit" => {
                    SPAWN_RECORD_FAILURES.store(1, std::sync::atomic::Ordering::Release);
                    SPAWN_AUDIT_CLEANUP_FAILURES.store(1, std::sync::atomic::Ordering::Release);
                }
                _ => unreachable!(),
            }
            let args = json!({"prompt": format!("{failure} cleanup failure")});
            let error = if cancellable {
                spawn_subagent_cancellable(&args, root.path(), None)
                    .await
                    .expect_err("cancellable partial cleanup must fail")
            } else {
                spawn_subagent(&args, root.path()).expect_err("sync partial cleanup must fail")
            };
            SPAWN_RECORD_FAILURES.store(0, std::sync::atomic::Ordering::Release);
            SPAWN_WORKTREE_CLEANUP_FAILURES.store(0, std::sync::atomic::Ordering::Release);
            SPAWN_OWNER_CLEANUP_FAILURES.store(0, std::sync::atomic::Ordering::Release);
            SPAWN_AUDIT_CLEANUP_FAILURES.store(0, std::sync::atomic::Ordering::Release);
            SPAWN_SESSION_CLEANUP_FAILURES.store(0, std::sync::atomic::Ordering::Release);
            SPAWN_SESSION_PUBLICATION_FAILURES.store(0, std::sync::atomic::Ordering::Release);
            assert!(error.contains("failure"), "{failure}: {error}");
            let preparations = spawn_preparation_directory_path(root.path());
            assert!(
                preparations.exists()
                    && std::fs::read_dir(&preparations)
                        .expect("read retained preparations")
                        .next()
                        .is_some(),
                "{failure} cancellable={cancellable} retired intent before exact cleanup"
            );
            if failure == "audit" {
                let intent_path = std::fs::read_dir(&preparations)
                    .expect("read retained audit intent")
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .find(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
                    .expect("retained canonical audit intent");
                let intent: SpawnPreparationIntentData = serde_json::from_slice(
                    &std::fs::read(&intent_path).expect("read retained audit intent"),
                )
                .expect("decode retained audit intent");
                assert_eq!(intent.phase, SpawnPreparationPhase::AuditPublished);
                assert!(
                    intent
                        .audit_sessions_dir
                        .join(format!("{}.json", intent.audit_session_id))
                        .is_file(),
                    "audit cleanup failure was retried by Drop instead of restart"
                );
            }

            assert!(list_subagents(root.path())
                .expect("restart reconciliation")
                .is_empty());
            assert!(
                !preparations.exists()
                    || std::fs::read_dir(&preparations)
                        .expect("read reconciled preparations")
                        .next()
                        .is_none(),
                "{failure} cancellable={cancellable} left a durable preparation intent"
            );
            let after = subagent_namespace_snapshot(root.path());
            assert_spawn_cleanup_snapshot(
                &before,
                &after,
                &format!("{failure} cancellable={cancellable} restart changed namespace entries"),
            );
        }
    }
}

#[tokio::test]
async fn post_audit_cancellation_cleanup_failure_retains_intent_until_restart() {
    let root = tempfile::tempdir().expect("git project");
    initialize_spawn_test_repository(root.path());
    ensure_records_directory(root.path()).expect("authorized records namespace");
    let primed_owner = SubagentOwnerLease::create(root.path()).expect("prime owner");
    primed_owner.remove().expect("remove primed owner");
    let primed_worktree =
        crate::sandbox::worktree::Worktree::create(root.path(), "post-audit-prime")
            .expect("prime worktree");
    crate::sandbox::worktree::Worktree::remove(root.path(), &primed_worktree.id)
        .expect("remove primed worktree");
    crate::session::SessionStore::for_project(root.path())
        .expect("prime fallback session namespace");
    let before = subagent_namespace_snapshot(root.path());
    let cancellation = crate::agent::CancellationSignal::new();
    SPAWN_POST_AUDIT_CANCELLATIONS.store(1, std::sync::atomic::Ordering::Release);
    SPAWN_AUDIT_CLEANUP_FAILURES.store(1, std::sync::atomic::Ordering::Release);

    let error = spawn_subagent_cancellable(
        &json!({"prompt": "cancel after audit publication"}),
        root.path(),
        Some(&cancellation),
    )
    .await
    .expect_err("post-audit cancellation cleanup failure must fail closed");
    SPAWN_POST_AUDIT_CANCELLATIONS.store(0, std::sync::atomic::Ordering::Release);
    SPAWN_AUDIT_CLEANUP_FAILURES.store(0, std::sync::atomic::Ordering::Release);
    assert!(error.contains("cancelled before commit"), "{error}");
    assert!(error.contains("audit cleanup failed"), "{error}");
    let preparations = spawn_preparation_directory_path(root.path());
    assert!(
        preparations.exists()
            && std::fs::read_dir(&preparations)
                .expect("read retained post-audit preparation")
                .next()
                .is_some(),
        "post-audit cancellation retired intent after partial cleanup"
    );
    let intent_path = std::fs::read_dir(&preparations)
        .expect("read retained post-audit preparation")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .expect("retained post-audit intent");
    let intent: SpawnPreparationIntentData = serde_json::from_slice(
        &std::fs::read(&intent_path).expect("read retained post-audit intent"),
    )
    .expect("decode retained post-audit intent");
    assert_eq!(intent.phase, SpawnPreparationPhase::AuditPublished);
    assert!(
        intent
            .audit_sessions_dir
            .join(format!("{}.json", intent.audit_session_id))
            .is_file(),
        "post-audit cleanup failure was retried by Drop instead of restart"
    );

    assert!(list_subagents(root.path())
        .expect("restart post-audit reconciliation")
        .is_empty());
    assert!(
        std::fs::read_dir(&preparations)
            .expect("read reconciled post-audit preparations")
            .next()
            .is_none(),
        "restart did not retire post-audit intent last"
    );
    assert_spawn_cleanup_snapshot(
        &before,
        &subagent_namespace_snapshot(root.path()),
        "post-audit restart left an external artifact",
    );
}

#[tokio::test]
async fn final_intent_retirement_expiry_is_fail_closed_for_sync_and_async_handoffs() {
    for cancellable in [false, true] {
        let root = tempfile::tempdir().expect("handoff expiry project");
        initialize_spawn_test_repository(root.path());
        let operation_timeout = if cfg!(windows) {
            Duration::from_secs(15)
        } else {
            Duration::from_secs(5)
        };
        let expiry_delay = operation_timeout + Duration::from_millis(100);
        let cancellation_timeout = if cfg!(windows) {
            Duration::from_secs(15)
        } else {
            Duration::from_secs(2)
        };
        let _timeout = SpawnPreparationTimeoutGuard::set(operation_timeout);
        let _cancellation_timeout = SubagentCancellationTimeoutGuard::set(cancellation_timeout);
        let _hook = SpawnHandoffPhaseHookGuard::install(move |phase| {
            if phase == "before_intent_retirement" {
                std::thread::sleep(expiry_delay);
            }
        });
        let args = json!({"prompt": "expire only at final intent retirement"});
        let error = if cancellable {
            spawn_subagent_cancellable(&args, root.path(), None)
                .await
                .expect_err("async final retirement expiry must fail closed")
        } else {
            spawn_subagent(&args, root.path())
                .expect_err("sync final retirement expiry must fail closed")
        };
        assert!(
            error.contains("retire authoritative spawn preparation after handoff"),
            "cancellable={cancellable}: {error}"
        );
        drop(_hook);
        drop(_timeout);
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(25)).await;

        let listed = list_subagents(root.path()).unwrap_or_else(|reconcile| {
            panic!("cancellable={cancellable}: restart reconciliation: {reconcile}")
        });
        assert!(
            listed.iter().all(|record| !matches!(
                record.get("status").and_then(Value::as_str).unwrap_or(""),
                "running" | "recovery_required"
            )),
            "cancellable={cancellable}: final expiry exposed unlaunchable state: {listed:?}"
        );
        let preparations = spawn_preparation_directory_path(root.path());
        assert!(
            !preparations.exists()
                || std::fs::read_dir(&preparations)
                    .expect("read final-expiry preparations")
                    .next()
                    .is_none(),
            "cancellable={cancellable}: restart did not retire intent last"
        );
    }
}

#[tokio::test]
async fn manager_rollback_failure_preserves_intent_until_restart_compensates() {
    for cancellable in [false, true] {
        let root = tempfile::tempdir().expect("manager rollback project");
        initialize_spawn_test_repository(root.path());
        let operation_timeout = if cfg!(windows) {
            Duration::from_secs(15)
        } else {
            Duration::from_secs(5)
        };
        let expiry_delay = operation_timeout + Duration::from_millis(100);
        let cancellation_timeout = if cfg!(windows) {
            Duration::from_secs(15)
        } else {
            Duration::from_secs(2)
        };
        let _timeout = SpawnPreparationTimeoutGuard::set(operation_timeout);
        let _cancellation_timeout = SubagentCancellationTimeoutGuard::set(cancellation_timeout);
        let _hook = SpawnHandoffPhaseHookGuard::install(move |phase| {
            if phase == "manager_registered" {
                std::thread::sleep(expiry_delay);
            }
        });
        crate::daemons::task::inject_rollback_unattached_failures(1);
        let args = json!({"prompt": "expire after manager registration"});
        let error = if cancellable {
            spawn_subagent_cancellable(&args, root.path(), None)
                .await
                .expect_err("async manager rollback failure must fail closed")
        } else {
            spawn_subagent(&args, root.path())
                .expect_err("sync manager rollback failure must fail closed")
        };
        assert!(
            error.contains("task registration rollback failed"),
            "{error}"
        );
        drop(_hook);
        drop(_timeout);
        crate::daemons::task::inject_rollback_unattached_failures(0);
        let preparation_dir = spawn_preparation_directory_path(root.path());
        let intent_path = std::fs::read_dir(&preparation_dir)
            .expect("retained manager-failure intent directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
            .expect("retained manager-failure intent");
        let intent: SpawnPreparationIntentData = serde_json::from_slice(
            &std::fs::read(&intent_path).expect("read manager-failure intent"),
        )
        .expect("decode manager-failure intent");
        assert_eq!(
            crate::daemons::task::TASK_MANAGER.get_status(&intent.subagent_id),
            Some("running".to_string()),
            "failed rollback did not retain the unattached manager entry"
        );

        let listed = list_subagents(root.path()).unwrap_or_else(|reconcile| {
            panic!("cancellable={cancellable}: rollback restart: {reconcile}")
        });
        assert!(
            listed.is_empty(),
            "rollback restart retained workload: {listed:?}"
        );
        assert_eq!(
            crate::daemons::task::TASK_MANAGER.get_status(&intent.subagent_id),
            None
        );
    }
}

#[tokio::test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
async fn expired_spawn_preparation_retains_intent_until_fresh_restart_cleanup() {
    const OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
    const EXPIRY_DELAY: Duration = Duration::from_millis(5_100);
    // The assertion is that a fresh, bounded restart authority completes
    // exact cleanup after the deliberately expired spawn authority. The
    // production five-second outer/three-second worktree budgets are not
    // under test and are too tight for repeated Git cleanup on a loaded
    // Windows runner.
    let _reconciliation_timeout = SpawnReconciliationTimeoutGuard::set(Duration::from_secs(15));

    for cancellable in [false, true] {
        for phase in ["worktree_reservation", "session_temp", "session_canonical"] {
            let root = tempfile::tempdir().expect("git project");
            initialize_spawn_test_repository(root.path());
            ensure_records_directory(root.path()).expect("authorized records namespace");
            let primed_owner = SubagentOwnerLease::create(root.path()).expect("prime owner");
            primed_owner.remove().expect("remove primed owner");
            let primed_worktree =
                crate::sandbox::worktree::Worktree::create(root.path(), "deadline-prime")
                    .expect("prime worktree");
            crate::sandbox::worktree::Worktree::remove(root.path(), &primed_worktree.id)
                .expect("remove primed worktree");
            let sessions = crate::session::SessionStore::for_project(root.path())
                .expect("prime fallback session namespace")
                .sessions_dir()
                .to_path_buf();
            let records = records_dir(root.path());
            let ownership = root.path().join(".nib/worktree-ownership");
            let ownership_before = std::fs::read_dir(&ownership)
                .expect("read primed worktree ownership")
                .map(|entry| entry.expect("worktree ownership entry").file_name())
                .collect::<std::collections::BTreeSet<_>>();
            let before = subagent_namespace_snapshot(root.path());
            let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let fired_for_hook = fired.clone();
            let records_for_hook = records.clone();
            let sessions_for_hook = sessions.clone();
            let ownership_for_hook = ownership.clone();
            let ownership_before_for_hook = ownership_before.clone();
            let hook = std::sync::Arc::new(move |observed_records: &Path| {
                if !crate::fs_security::canonical_paths_match(observed_records, &records_for_hook)
                    || fired_for_hook.load(std::sync::atomic::Ordering::Acquire)
                {
                    return Ok(());
                }
                let boundary_reached = match phase {
                    "worktree_reservation" => std::fs::read_dir(&ownership_for_hook)
                        .map(|entries| {
                            entries.filter_map(Result::ok).any(|entry| {
                                !ownership_before_for_hook.contains(&entry.file_name())
                            })
                        })
                        .unwrap_or(false),
                    "session_temp" => std::fs::read_dir(&sessions_for_hook)
                        .map(|entries| {
                            entries.filter_map(Result::ok).any(|entry| {
                                let name = entry.file_name();
                                let name = name.to_string_lossy();
                                name.starts_with(".nib-session-")
                                    && name.ends_with(".tmp")
                                    && std::fs::metadata(entry.path())
                                        .is_ok_and(|metadata| metadata.len() > 0)
                            })
                        })
                        .unwrap_or(false),
                    "session_canonical" => std::fs::read_dir(&sessions_for_hook)
                        .map(|entries| {
                            entries
                                .filter_map(Result::ok)
                                .any(|entry| entry.file_name().to_string_lossy().ends_with(".json"))
                        })
                        .unwrap_or(false),
                    _ => unreachable!(),
                };
                if boundary_reached
                    && fired_for_hook
                        .compare_exchange(
                            false,
                            true,
                            std::sync::atomic::Ordering::AcqRel,
                            std::sync::atomic::Ordering::Acquire,
                        )
                        .is_ok()
                {
                    std::thread::sleep(EXPIRY_DELAY);
                }
                Ok(())
            });
            let timeout_guard = SpawnPreparationTimeoutGuard::set(OPERATION_TIMEOUT);
            let hook_guard = SpawnAuthorityVerifyHookGuard::install(hook);
            let args = json!({"prompt": format!("expire after {phase}")});
            let result = if cancellable {
                spawn_subagent_cancellable(&args, root.path(), None).await
            } else {
                spawn_subagent(&args, root.path())
            };
            drop(hook_guard);
            drop(timeout_guard);
            let error = result.expect_err("expired spawn preparation must fail closed");
            assert!(
                error.contains("deadline") || error.contains("timed out"),
                "{phase} cancellable={cancellable}: {error}"
            );
            assert!(
                fired.load(std::sync::atomic::Ordering::Acquire),
                "{phase} cancellable={cancellable} never reached the durable boundary"
            );
            let preparations = spawn_preparation_directory_path(root.path());
            assert!(
                preparations.exists()
                    && std::fs::read_dir(&preparations)
                        .expect("read retained deadline preparation")
                        .next()
                        .is_some(),
                "{phase} cancellable={cancellable} retired an expired preparation"
            );

            assert!(list_subagents(root.path())
                .expect("fresh-deadline restart reconciliation")
                .is_empty());
            assert!(
                std::fs::read_dir(&preparations)
                    .expect("read reconciled deadline preparations")
                    .next()
                    .is_none(),
                "{phase} cancellable={cancellable} did not retire intent last"
            );
            let after = subagent_namespace_snapshot(root.path());
            assert_spawn_cleanup_snapshot(
                &before,
                &after,
                &format!("{phase} cancellable={cancellable} restart changed namespace entries"),
            );
        }
    }
}

#[tokio::test]
async fn parent_only_internal_audit_argument_is_rejected_before_legacy_session_migration() {
    let root = tempfile::tempdir().expect("project root");
    let legacy = crate::session::SessionStore::new(root.path());
    legacy
        .try_create_session_with_id("legacy-parent")
        .expect("legacy parent session");
    let before = subagent_namespace_snapshot(root.path());

    let args = json!({
        "prompt": "must reject incomplete reserved authority",
        "_parent_session_id": "legacy-parent",
    });
    let sync_error = spawn_subagent(&args, root.path())
        .expect_err("sync parent-only authority must be rejected");
    assert!(sync_error.contains("must be supplied together"));
    let cancellable_error = spawn_subagent_cancellable(&args, root.path(), None)
        .await
        .expect_err("cancellable parent-only authority must be rejected");
    assert!(cancellable_error.contains("must be supplied together"));
    assert_eq!(subagent_namespace_snapshot(root.path()), before);

    let migrated = crate::session::SessionStore::for_project(root.path())
        .expect("normal profile migration remains available");
    assert!(migrated
        .load_result("legacy-parent")
        .expect("load migrated parent")
        .is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn direct_spawn_rejects_selected_state_replacement_after_audit_preflight_without_mutation() {
    let root = tempfile::tempdir().expect("project root");
    let state = root.path().join(".nib/selected-state");
    let displaced = root.path().join(".nib/selected-state.displaced");
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
    let store = crate::session::SessionStore::for_project(root.path()).expect("session store");
    store
        .try_create_session_with_id("existing-audit")
        .expect("existing audit session");
    let original_snapshot = directory_tree_snapshot(&state);

    let state_for_hook = state.clone();
    let displaced_for_hook = displaced.clone();
    AFTER_SUBAGENT_AUDIT_PREFLIGHT_HOOK.with(|slot| {
        assert!(slot
            .borrow_mut()
            .replace(Box::new(move || {
                std::fs::rename(&state_for_hook, &displaced_for_hook)
                    .expect("displace selected state");
                std::fs::create_dir(&state_for_hook).expect("replacement selected state");
                std::fs::write(state_for_hook.join("sentinel"), b"replacement")
                    .expect("replacement sentinel");
            }))
            .is_none());
    });

    let error = spawn_subagent(
        &json!({"prompt": "must not mutate replacement"}),
        root.path(),
    )
    .expect_err("selected state replacement must fail closed");
    assert!(
        error.contains("identity changed") || error.contains("changed while"),
        "unexpected replacement error: {error}"
    );
    assert_eq!(directory_tree_snapshot(&displaced), original_snapshot);
    assert_eq!(
        directory_tree_snapshot(&state),
        vec![(PathBuf::from("sentinel"), b"replacement".to_vec())]
    );
    for path in [
        root.path().join(".nib/subagents"),
        root.path().join(".nib/subagent-owner-leases"),
        root.path().join(".nib/worktrees/subagents"),
    ] {
        assert!(
            !path.exists(),
            "replacement failure created {}",
            path.display()
        );
    }

    std::fs::remove_dir_all(&state).expect("remove replacement state");
    std::fs::rename(&displaced, &state).expect("restore selected state");
    prepare_subagent_audit_target(&json!({}), root.path(), "fresh-state-retry")
        .expect("fresh retry binds the restored state capability");
    assert!(state.join("sessions/fresh-state-retry.json").is_file());
}

#[cfg(unix)]
#[tokio::test]
async fn cancellable_spawn_rejects_sessions_replacement_after_audit_preflight_without_mutation() {
    let root = tempfile::tempdir().expect("project root");
    let state = root.path().join(".nib/selected-state");
    let sessions = state.join("sessions");
    let displaced = state.join("sessions.displaced");
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
    let store = crate::session::SessionStore::for_project(root.path()).expect("session store");
    store
        .try_create_session_with_id("existing-audit")
        .expect("existing audit session");
    let original_snapshot = directory_tree_snapshot(&sessions);

    let sessions_for_hook = sessions.clone();
    let displaced_for_hook = displaced.clone();
    AFTER_SUBAGENT_AUDIT_PREFLIGHT_HOOK.with(|slot| {
        assert!(slot
            .borrow_mut()
            .replace(Box::new(move || {
                std::fs::rename(&sessions_for_hook, &displaced_for_hook)
                    .expect("displace sessions directory");
                std::fs::create_dir(&sessions_for_hook).expect("replacement sessions");
                std::fs::write(sessions_for_hook.join("sentinel"), b"replacement")
                    .expect("replacement sentinel");
            }))
            .is_none());
    });

    let error = spawn_subagent_cancellable(
        &json!({"prompt": "must not mutate replacement"}),
        root.path(),
        None,
    )
    .await
    .expect_err("sessions replacement must fail closed");
    assert!(
        error.contains("identity changed") || error.contains("changed while"),
        "unexpected replacement error: {error}"
    );
    assert_eq!(directory_tree_snapshot(&displaced), original_snapshot);
    assert_eq!(
        directory_tree_snapshot(&sessions),
        vec![(PathBuf::from("sentinel"), b"replacement".to_vec())]
    );
    for path in [
        root.path().join(".nib/subagents"),
        root.path().join(".nib/subagent-owner-leases"),
        root.path().join(".nib/worktrees/subagents"),
    ] {
        assert!(
            !path.exists(),
            "replacement failure created {}",
            path.display()
        );
    }

    std::fs::remove_dir_all(&sessions).expect("remove replacement sessions");
    std::fs::rename(&displaced, &sessions).expect("restore sessions directory");
    prepare_subagent_audit_target(&json!({}), root.path(), "fresh-sessions-retry")
        .expect("fresh retry binds the restored sessions capability");
    assert!(sessions.join("fresh-sessions-retry.json").is_file());
}

#[tokio::test]
async fn direct_spawn_rejects_missing_profile_parent_appearance_after_audit_preflight() {
    let root = tempfile::tempdir().expect("project root");
    let mut config = crate::config::NibConfig::default();
    crate::config::save_nib_config_full(root.path(), &mut config).expect("default config");
    let profiles = root.path().join(".nib/profiles");
    assert!(!profiles.exists(), "fixture profile parent starts absent");

    let profiles_for_hook = profiles.clone();
    AFTER_SUBAGENT_AUDIT_PREFLIGHT_HOOK.with(|slot| {
        assert!(slot
            .borrow_mut()
            .replace(Box::new(move || {
                std::fs::create_dir(&profiles_for_hook).expect("appearing profile parent");
                std::fs::write(profiles_for_hook.join("sentinel"), b"appeared")
                    .expect("appearing parent sentinel");
            }))
            .is_none());
    });

    let error = spawn_subagent(
        &json!({"prompt": "must preserve appearing parent"}),
        root.path(),
    )
    .expect_err("post-preflight profile parent must fail closed");
    assert!(
        error.contains("appeared after its absence was proven"),
        "unexpected appearing-parent error: {error}"
    );
    assert_eq!(
        std::fs::read(profiles.join("sentinel")).expect("preserved sentinel"),
        b"appeared"
    );
    assert_eq!(
        std::fs::read_dir(&profiles)
            .expect("preserved profile parent")
            .count(),
        1,
        "failed preflight must not add profile descendants"
    );
    for path in [
        root.path().join(".nib/subagents"),
        root.path().join(".nib/subagent-owner-leases"),
        root.path().join(".nib/worktrees/subagents"),
    ] {
        assert!(
            !path.exists(),
            "appearing parent created {}",
            path.display()
        );
    }

    std::fs::remove_dir_all(&profiles).expect("remove appearing profile parent");
    prepare_subagent_audit_target(&json!({}), root.path(), "fresh-missing-parent-retry")
        .expect("fresh retry creates the preflighted profile hierarchy");
    assert!(root
        .path()
        .join(".nib/profiles/default/sessions/fresh-missing-parent-retry.json")
        .is_file());
}

#[test]
fn direct_list_projects_running_authority_and_preserves_terminal_and_merge_results() {
    let root = tempfile::tempdir().expect("root");
    let owner_lease = SubagentOwnerLease::create(root.path()).expect("live owner lease");
    let mut running = record_fixture(root.path(), "sub-public-running", "running");
    attach_execution_ownership(&mut running, &owner_lease);
    let private_path = root.path().join("private-audit-sessions");
    running.result = Some(json!({
        "_ownership_audit_target": {
            "sessions_dir": private_path,
            "directory_identity": "private-file-identity",
        }
    }));
    write_subagent_record(root.path(), &running).expect("running record");

    let listed = list_subagents(root.path()).expect("public subagent list");
    assert_eq!(listed.len(), 1);
    let public = &listed[0];
    assert_eq!(public["id"], running.id);
    assert!(public["result"].is_null());
    assert!(public.get("execution_generation").is_none());
    assert!(public.get("owner_lease").is_none());
    let encoded = serde_json::to_string(public).expect("public record");
    assert!(!encoded.contains(OWNERSHIP_AUDIT_TARGET_KEY));
    assert!(!encoded.contains("private-audit-sessions"));
    assert!(!encoded.contains("private-file-identity"));

    let persisted =
        get_subagent_record_unreconciled(root.path(), &running.id).expect("internal record");
    assert!(persisted.execution_generation.is_some());
    assert!(persisted.owner_lease.is_some());
    assert!(subagent_audit_target(&persisted).is_err());

    let terminal = project_public_subagent_result(json!({
        "outcome": "completed",
        "summary": "public result",
        "cleanup_verified": true,
        "cleanup_proof": {"private": "authority"},
        "_ownership_audit_target": {"private": "target"},
    }))
    .expect("terminal public result");
    assert_eq!(
        terminal,
        json!({"outcome": "completed", "summary": "public result"})
    );
    let manager_id = format!("sub-public-manager-{}", uuid::Uuid::new_v4());
    crate::daemons::task::TASK_MANAGER
        .register_task(manager_id.clone(), "subagent")
        .expect("manager record");
    let mut terminal_record = record_fixture(root.path(), &manager_id, "completed");
    terminal_record.result = Some(json!({
        "outcome": "completed",
        "summary": "public result",
        "cleanup_verified": true,
        "cleanup_proof": {"private": "authority"},
        "_ownership_audit_target": {"private": "target"},
    }));
    sync_subagent_task_manager(&terminal_record);
    let manager_public = crate::daemons::task::TASK_MANAGER
        .get_task(&manager_id)
        .expect("public manager record");
    assert_eq!(manager_public["result"], terminal);

    let merged = project_public_subagent_result(json!({
        "subagent_result": {
            "summary": "done",
            "ownership_reconciliation": {"private": "authority"},
            "_ownership_audit_target": {"private": "target"},
        },
        "merge_commit": "abc123",
        "merge_stdout": "merged",
    }))
    .expect("merge public result");
    assert_eq!(merged["subagent_result"], json!({"summary": "done"}));
    assert_eq!(merged["merge_commit"], "abc123");
    assert_eq!(merged["merge_stdout"], "merged");
}

#[test]
fn terminal_scope_retirement_requires_a_locked_full_record_and_exact_authority() {
    let root = tempfile::tempdir().expect("root");
    let execution_generation = 7001;
    let id = "sub-terminal-retirement";
    let proof = install_completed_process_scope(root.path(), id, execution_generation);
    let owner_lease = uuid::Uuid::new_v4().to_string();
    let record =
        terminal_record_fixture(root.path(), id, execution_generation, &owner_lease, &proof);
    let _records = ensure_records_directory(root.path()).expect("records directory");
    let path = record_path(root.path(), id).expect("record path");
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({
            "id": id,
            "status": "completed",
            "result": record.result.clone(),
        }))
        .expect("partial record"),
    )
    .expect("write partial record");
    assert!(retire_terminal_process_scope(root.path(), &record).is_err());
    assert!(root
        .path()
        .join(".nib/process-scopes")
        .join(format!("{id}.json"))
        .exists());
    std::fs::remove_file(&path).expect("remove partial record");
    write_subagent_record(root.path(), &record).expect("full terminal record");
    assert!(retire_terminal_process_scope(root.path(), &record).expect("retire exact scope"));
    assert!(!root
        .path()
        .join(".nib/process-scopes")
        .join(format!("{id}.json"))
        .exists());
}

#[test]
fn delegated_provider_failure_preserves_typed_private_terminal_evidence() {
    let root = tempfile::tempdir().expect("root");
    let id = "sub-typed-provider-failure";
    let execution_generation = 81_337;
    let lease_id = uuid::Uuid::new_v4().to_string();
    let mut record = record_fixture(root.path(), id, "running");
    record.execution_generation = Some(execution_generation);
    record.owner_lease = Some(lease_id.clone());
    write_subagent_record(root.path(), &record).expect("running subagent record");
    crate::daemons::task::TASK_MANAGER
        .register_task(id.to_string(), "subagent")
        .expect("register delegated task");

    let secret = "delegated-provider-private-secret".to_string();
    let sensitive_values = vec![secret.clone()];
    let failure = crate::llm::LlmError::new(
        crate::llm::LlmErrorClass::Authentication,
        crate::llm::LlmErrorPhase::HttpResponse,
        crate::llm::RetryDisposition::NotRetryable,
        crate::llm::LlmErrorMetadata::new(
            "openai",
            "responses",
            Some("fixture-model"),
            Some(401),
            &sensitive_values,
        ),
        format!("provider rejected private credential {secret}"),
    );
    let summary = crate::agent::AgentRunSummary {
        session_id: format!("child-{id}"),
        run_id: "0123456789abcdef0123456789abcdef".to_string(),
        steps_taken: 1,
        last_message: None,
        tool_call_count: 0,
        final_state: crate::agent::state::AgentState::Done,
        outcome: "llm_stream_failed".to_string(),
        failure: Some(failure),
        bound_reached: false,
        trace: vec!["reconciliation".to_string(), "done".to_string()],
    };

    persist_subagent_outcome(
        root.path(),
        id,
        execution_generation,
        &lease_id,
        Ok(summary),
    )
    .expect("persist delegated provider failure");

    let persisted =
        get_subagent_record_unreconciled(root.path(), id).expect("persisted delegated record");
    assert_eq!(persisted.status, "failed");
    let result = persisted.result.expect("delegated failure result");
    assert_eq!(result["outcome"], "llm_stream_failed");
    assert!(result["last_message"].is_null());
    assert_eq!(result["failure"]["class"], "authentication");
    assert_eq!(result["failure"]["incident_code"], "LLM-AUTH");
    assert_eq!(result["failure"]["provider"], "openai");
    assert_eq!(result["failure"]["transport"], "responses");
    assert_eq!(result["failure"]["http_status"], 401);

    let observed = crate::daemons::task::TASK_MANAGER
        .get_task(id)
        .expect("delegated task observer");
    assert_eq!(observed["status"], "failed");
    assert_eq!(
        observed["result"]["failure"]["class"],
        result["failure"]["class"]
    );
    assert_eq!(
        observed["result"]["failure"]["incident_code"],
        result["failure"]["incident_code"]
    );
    let encoded = format!(
        "{}\n{}",
        serde_json::to_string(&result).unwrap(),
        serde_json::to_string(&observed).unwrap()
    );
    assert!(!encoded.contains(&secret));
    assert!(!encoded.contains("provider rejected private credential"));
}

#[test]
fn terminal_scope_retirement_rejects_mismatched_ownership_and_stale_snapshots() {
    let root = tempfile::tempdir().expect("root");
    let execution_generation = 7002;
    let id = "sub-terminal-stale";
    let proof = install_completed_process_scope(root.path(), id, execution_generation);
    let owner_lease = uuid::Uuid::new_v4().to_string();
    let mut record =
        terminal_record_fixture(root.path(), id, execution_generation, &owner_lease, &proof);

    let mut mismatched = record.clone();
    mismatched.execution_generation = Some(execution_generation + 1);
    assert!(terminal_process_scope_authority(&mismatched).is_err());
    mismatched = record.clone();
    mismatched.result = Some(json!({
        "outcome": "interrupted",
        "ownership_reconciliation": {
            "subagent_id": id,
            "execution_generation": execution_generation,
            "owner_lease": uuid::Uuid::new_v4().to_string(),
            "terminal_status": "completed",
            "cleanup_verified": true,
            "cleanup_proof": proof,
        }
    }));
    assert!(terminal_process_scope_authority(&mismatched).is_err());

    write_subagent_record(root.path(), &record).expect("terminal record");
    let stale = record.clone();
    record.error = Some("replacement revision".to_string());
    record.updated_at = Utc::now() + chrono::Duration::milliseconds(1);
    update_subagent_record(root.path(), id, |current| {
        *current = record.clone();
        Ok(())
    })
    .expect("replacement record");
    assert!(retire_terminal_process_scope(root.path(), &stale).is_err());
    assert!(root
        .path()
        .join(".nib/process-scopes")
        .join(format!("{id}.json"))
        .exists());
}

#[test]
fn terminal_scope_retirement_retries_after_merge_status_advances() {
    let root = tempfile::tempdir().expect("root");
    let execution_generation = 7003;
    let id = "sub-terminal-retirement-retry";
    let (proof, cleanup_lease) =
        install_completed_process_scope_with_live_lease(root.path(), id, execution_generation);
    let owner_lease = uuid::Uuid::new_v4().to_string();
    let mut record =
        terminal_record_fixture(root.path(), id, execution_generation, &owner_lease, &proof);
    write_subagent_record(root.path(), &record).expect("terminal record");

    let error = retire_terminal_process_scope(root.path(), &record)
        .expect_err("live cleanup lease must defer retirement");
    assert!(error.contains("cleanup lease exists"), "{error}");

    let subagent_result = record.result.take();
    record.status = MERGE_PENDING_STATUS.to_string();
    record.result = Some(json!({
        "subagent_result": subagent_result,
        "verification_command": "task check",
        "merge_commit": "a".repeat(40),
        "parent_head_before": "b".repeat(40),
        "active_merge_base": Value::Null,
        "merge_stdout": Value::Null,
    }));
    record.updated_at = Utc::now() + chrono::Duration::milliseconds(1);
    update_subagent_record(root.path(), id, |current| {
        *current = record.clone();
        Ok(())
    })
    .expect("advance terminal record to merge pending");
    cleanup_lease
        .release_after_proof(&proof)
        .expect("release transient cleanup lease");

    let advanced =
        get_subagent_record_unreconciled(root.path(), id).expect("advanced terminal record");
    assert!(retire_terminal_process_scope(root.path(), &advanced)
        .expect("retry retirement from merge-pending authority"));
    assert!(!root
        .path()
        .join(".nib/process-scopes")
        .join(format!("{id}.json"))
        .exists());
}

#[test]
fn terminal_scope_retirement_authority_survives_every_supported_status_shape() {
    let root = tempfile::tempdir().expect("root");
    let execution_generation = 7004;
    let owner_lease = uuid::Uuid::new_v4().to_string();
    let proof = install_completed_process_scope(
        root.path(),
        "sub-terminal-authority-statuses",
        execution_generation,
    );
    let base = terminal_record_fixture(
        root.path(),
        "sub-terminal-authority-statuses",
        execution_generation,
        &owner_lease,
        &proof,
    );

    for status in [
        "completed",
        "failed",
        "cancelled",
        "verification_failed",
        MERGE_FAILED_STATUS,
    ] {
        let mut record = base.clone();
        record.status = status.to_string();
        let authority = terminal_process_scope_authority(&record)
            .expect("direct authority")
            .expect("direct proof");
        assert!(matches!(
            authority.1,
            TerminalProcessScopeAuthority::Cleanup(ref observed) if observed == &proof
        ));
    }

    for status in [MERGE_PENDING_STATUS, "merged"] {
        let mut record = base.clone();
        record.status = status.to_string();
        record.result = Some(json!({
            "subagent_result": record.result.take(),
        }));
        let authority = terminal_process_scope_authority(&record)
            .expect("nested authority")
            .expect("nested proof");
        assert!(matches!(
            authority.1,
            TerminalProcessScopeAuthority::Cleanup(ref observed) if observed == &proof
        ));
    }
}

#[test]
fn terminal_scope_retirement_accepts_exact_launch_abort_authority() {
    let root = tempfile::tempdir().expect("root");
    let execution_generation = 7005;
    let id = "sub-terminal-launch-abort";
    let owner_lease = uuid::Uuid::new_v4().to_string();
    let proof = install_completed_launch_abort_scope(root.path(), id, execution_generation);
    let base = launch_abort_terminal_record_fixture(
        root.path(),
        id,
        execution_generation,
        &owner_lease,
        &proof,
    );

    for status in [
        "completed",
        "failed",
        "cancelled",
        "verification_failed",
        MERGE_FAILED_STATUS,
    ] {
        let mut record = base.clone();
        record.status = status.to_string();
        let authority = terminal_process_scope_authority(&record)
            .expect("direct launch-abort authority")
            .expect("direct launch-abort proof");
        assert!(matches!(
            authority.1,
            TerminalProcessScopeAuthority::LaunchAbort(ref observed) if observed == &proof
        ));
    }

    for status in [MERGE_PENDING_STATUS, "merged"] {
        let mut record = base.clone();
        record.status = status.to_string();
        record.result = Some(json!({
            "subagent_result": record.result.take(),
        }));
        let authority = terminal_process_scope_authority(&record)
            .expect("nested launch-abort authority")
            .expect("nested launch-abort proof");
        assert!(matches!(
            authority.1,
            TerminalProcessScopeAuthority::LaunchAbort(ref observed) if observed == &proof
        ));
    }

    write_subagent_record(root.path(), &base).expect("terminal launch-abort record");
    assert!(retire_terminal_process_scope(root.path(), &base).expect("retire launch-aborted scope"));
    assert!(!root
        .path()
        .join(".nib/process-scopes")
        .join(format!("{id}.json"))
        .exists());
}

#[test]
fn terminal_scope_retirement_rejects_forged_launch_abort_authority() {
    let root = tempfile::tempdir().expect("root");
    let execution_generation = 7006;
    let id = "sub-terminal-launch-abort-forgery";
    let owner_lease = uuid::Uuid::new_v4().to_string();
    let proof = install_completed_launch_abort_scope(root.path(), id, execution_generation);
    let base = launch_abort_terminal_record_fixture(
        root.path(),
        id,
        execution_generation,
        &owner_lease,
        &proof,
    );

    let mut forged = base.clone();
    forged.result.as_mut().expect("result")["launch_abort_proof"]["execution_generation"] =
        json!(execution_generation + 1);
    let error = terminal_process_scope_authority(&forged)
        .expect_err("mismatched launch-abort generation must be rejected");
    assert!(error.contains("does not own its execution"), "{error}");

    forged = base.clone();
    forged.result.as_mut().expect("result")["workload_never_launched"] = Value::Bool(false);
    let error = terminal_process_scope_authority(&forged)
        .expect_err("unverified workload launch state must be rejected");
    assert!(error.contains("incomplete proof evidence"), "{error}");

    forged = base.clone();
    forged.result.as_mut().expect("result")["launch_abort_verified"] = Value::Bool(false);
    let error = terminal_process_scope_authority(&forged)
        .expect_err("proof without verified launch abort must be rejected");
    assert!(error.contains("without verified launch abort"), "{error}");

    let cleanup_proof = crate::sandbox::process::CleanupProof {
        execution_generation,
        cleanup_lease_id: proof.cleanup_lease_id.clone(),
        backend: proof.backend,
        direct_child: proof.supervisor.clone(),
        outcome: "forged_cleanup".to_string(),
        descendants_reaped: true,
        completed_at: Utc::now(),
    };
    forged = base.clone();
    let result = forged
        .result
        .as_mut()
        .and_then(Value::as_object_mut)
        .expect("result object");
    result.insert("cleanup_verified".to_string(), Value::Bool(true));
    result.insert(
        "cleanup_proof".to_string(),
        serde_json::to_value(cleanup_proof).expect("cleanup proof"),
    );
    let error = terminal_process_scope_authority(&forged)
        .expect_err("cleanup and launch-abort authority must be mutually exclusive");
    assert!(
        error.contains("conflicts with cleanup verification"),
        "{error}"
    );

    for (field, value) in [
        (
            "owner_lease",
            Value::String(uuid::Uuid::new_v4().to_string()),
        ),
        ("terminal_status", Value::String("completed".to_string())),
    ] {
        forged = base.clone();
        forged.result = Some(json!({
            "ownership_reconciliation": {
                "subagent_id": id,
                "execution_generation": execution_generation,
                "owner_lease": owner_lease,
                "terminal_status": "failed",
                "cleanup_verified": false,
                "launch_abort_verified": true,
                "workload_never_launched": true,
                "launch_abort_proof": proof,
            }
        }));
        forged.result.as_mut().expect("result")["ownership_reconciliation"][field] = value;
        let error = terminal_process_scope_authority(&forged)
            .expect_err("mismatched reconciliation ownership must be rejected");
        assert!(
            error.contains("does not match subagent execution ownership"),
            "{error}"
        );
    }

    assert!(root
        .path()
        .join(".nib/process-scopes")
        .join(format!("{id}.json"))
        .exists());
}

#[test]
fn subagent_owner_process_loss_child() {
    let Some(project_root) = std::env::var_os(OWNER_LOSS_CHILD_PROJECT_ROOT) else {
        return;
    };
    let ready = PathBuf::from(
        std::env::var_os(OWNER_LOSS_CHILD_READY).expect("owner-loss child ready path"),
    );
    let project_root = PathBuf::from(project_root);
    let owner_lease =
        SubagentOwnerLease::create(&project_root).expect("child owner lease is acquired");
    let mut record = record_fixture(&project_root, OWNER_LOSS_SUBAGENT_ID, "running");
    record.parent_session_id = Some("parent-owner-process-loss".to_string());
    attach_execution_ownership(&mut record, &owner_lease);
    write_subagent_record(&project_root, &record).expect("child running record is durable");
    std::fs::write(ready, b"ready").expect("child readiness is published");
    let _owner_lease = owner_lease;
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[test]
fn legacy_open_before_lock_child_process() {
    let Some(project_root) = std::env::var_os(LEGACY_OPEN_CHILD_PROJECT_ROOT) else {
        return;
    };
    let project_root = PathBuf::from(project_root);
    let ready =
        PathBuf::from(std::env::var_os(LEGACY_OPEN_CHILD_READY).expect("legacy child ready path"));
    let resume = PathBuf::from(
        std::env::var_os(LEGACY_OPEN_CHILD_RESUME).expect("legacy child resume path"),
    );
    let locked = PathBuf::from(
        std::env::var_os(LEGACY_OPEN_CHILD_LOCKED).expect("legacy child locked path"),
    );
    let release = PathBuf::from(
        std::env::var_os(LEGACY_OPEN_CHILD_RELEASE).expect("legacy child release path"),
    );
    let visible = records_dir(&project_root)
        .join(".locks")
        .join(format!("{LEGACY_OPEN_CHILD_ID}.lock"));
    let anchor =
        crate::daemons::state::daemon_lock_anchor_path(&visible).expect("legacy child anchor path");
    let owner = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&anchor)
        .expect("legacy child opens the exact anchor before locking");
    std::fs::write(&ready, b"opened").expect("publish opened readiness");
    let started = Instant::now();
    while !resume.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "legacy child was not resumed after opening"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    owner
        .try_lock()
        .expect("the exact opened legacy anchor remains lockable");
    std::fs::write(&locked, b"locked").expect("publish legacy lock acquisition");
    let started = Instant::now();
    while !release.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "legacy child was not released"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(any(unix, windows))]
#[test]
fn killed_owner_without_process_scope_requires_recovery_and_preserves_leases() {
    let root = tempfile::tempdir().expect("root");
    let store = crate::session::SessionStore::for_project(root.path()).expect("session store");
    store.create_session_with_id("parent-owner-process-loss");
    let ready = root.path().join("owner-loss.ready");
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "tools::delegation::tests::test_part_1::subagent_owner_process_loss_child",
            "--nocapture",
        ])
        .env(OWNER_LOSS_CHILD_PROJECT_ROOT, root.path())
        .env(OWNER_LOSS_CHILD_READY, &ready)
        .spawn()
        .expect("spawn owner process");
    let started = Instant::now();
    while !ready.exists() {
        if let Some(status) = child.try_wait().expect("inspect owner process") {
            panic!("owner process exited before readiness: {status}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "owner process did not publish readiness"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let live = get_subagent_record_internal(root.path(), OWNER_LOSS_SUBAGENT_ID)
        .expect("cross-process live owner remains running");
    assert_eq!(live.status, "running");
    assert!(
        live.result.is_none(),
        "live owner probing must not publish missing-scope recovery evidence"
    );
    let lease_id = live.owner_lease.clone().expect("owner lease id");
    child.kill().expect("kill owner process");
    child.wait().expect("reap owner process");

    let reconciled = get_subagent_record_internal(root.path(), OWNER_LOSS_SUBAGENT_ID)
        .expect("process loss requires recovery");
    assert_eq!(reconciled.status, "running");
    let result = reconciled.result.expect("recovery evidence");
    assert_eq!(result["outcome"], "recovery_required");
    assert_eq!(result["process_scope"]["status"], "missing");
    assert_eq!(result["cleanup_verified"], false);
    assert!(reconciled.error.is_none());
    let session = store
        .load_result("parent-owner-process-loss")
        .expect("load audit session")
        .expect("audit session");
    assert!(!session
        .events
        .iter()
        .any(|event| event.kind == "subagent_execution_reconciled"));
    assert!(owner_lease_path(root.path(), &lease_id)
        .expect("visible lease path")
        .exists());
    assert!(owner_lease_anchor_path(root.path(), &lease_id)
        .expect("lease anchor path")
        .exists());
}

#[test]
fn expired_legacy_audit_resolution_does_not_migrate_or_create_profile_state() {
    let root = tempfile::tempdir().expect("root");
    let nib = root.path().join(".nib");
    std::fs::create_dir(&nib).expect("legacy nib directory");
    std::fs::write(
        nib.join("config.json"),
        serde_json::to_vec_pretty(&crate::config::LlmConfig::default()).expect("legacy config"),
    )
    .expect("write legacy config");
    std::fs::create_dir(nib.join("sessions")).expect("legacy session source");
    let record = record_fixture(root.path(), "sub-expired-audit-target", "running");

    let error = resolve_legacy_subagent_audit_target(
        root.path(),
        &record,
        Some(Instant::now() - Duration::from_millis(1)),
    )
    .expect_err("expired audit setup must fail closed");

    assert!(error.contains("deadline elapsed"), "{error}");
    assert!(nib.join("config.json").is_file());
    assert!(!nib.join("config.toml").exists());
    assert!(!nib.join("config.json.bak").exists());
    assert!(!nib.join("profiles").exists());
    assert!(!nib.join(".legacy-state-migration-v1.json").exists());
}

#[cfg(unix)]
#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn legacy_audit_target_waits_for_evacuated_config_before_terminal_cas() {
    let root = tempfile::tempdir().expect("root");
    let nib = root.path().join(".nib");
    let wrong_sessions = nib.join("profiles/default/sessions");
    let selected_sessions = nib.join("profiles/selected/sessions");
    let wrong_store = crate::session::SessionStore::at_dir(wrong_sessions.clone());
    wrong_store.create_session_with_id("parent");
    let selected_store = crate::session::SessionStore::at_dir(selected_sessions.clone());
    selected_store.create_session_with_id("parent");

    let mut initial = crate::config::NibConfig::default();
    crate::config::save_nib_config_full(root.path(), &mut initial)
        .expect("initial TOML configuration");
    std::fs::write(
        nib.join("config.json"),
        serde_json::to_vec_pretty(&crate::config::LlmConfig::default())
            .expect("legacy JSON configuration"),
    )
    .expect("legacy JSON fallback");
    let wrong_namespace = directory_tree_snapshot(&wrong_sessions);

    let mut replacement = initial.clone();
    replacement.revision = replacement
        .revision
        .checked_add(1)
        .expect("replacement revision");
    replacement.profiles = crate::config::ProfilesConfig {
        default: "selected".to_string(),
        active: vec![crate::config::ProfileConfig {
            id: "selected".to_string(),
            root: PathBuf::from("."),
            state_dir: Some(PathBuf::from(".nib/profiles/selected")),
            ..crate::config::ProfileConfig::default()
        }],
    };
    let replacement_bytes = toml::to_string_pretty(&replacement)
        .expect("encode replacement configuration")
        .into_bytes();

    let owner_lease = SubagentOwnerLease::create(root.path()).expect("owner lease");
    let execution_generation = owner_lease.execution_generation;
    let id = "sub-config-evacuation-audit";
    let mut record = record_fixture(root.path(), id, "running");
    attach_execution_ownership(&mut record, &owner_lease);
    install_completed_process_scope(root.path(), id, execution_generation);
    write_subagent_record(root.path(), &record).expect("legacy running record");
    drop(owner_lease);

    let (evacuated_tx, evacuated_rx) = std::sync::mpsc::sync_channel(1);
    let (publish_tx, publish_rx) = std::sync::mpsc::sync_channel(1);
    let writer_nib = nib.clone();
    let config_path = nib.join("config.toml");
    let writer_config_path = config_path.clone();
    let writer = std::thread::spawn(move || {
        let directory = crate::daemons::state::StableDirectory::open(&writer_nib)
            .expect("stable config directory");
        let previous = directory
            .open_read(&writer_config_path)
            .expect("opened prior config");
        directory
            .save_bytes_atomically_expected_with_after_evacuation_hook(
                &writer_config_path,
                &replacement_bytes,
                ".config.toml.tmp-",
                crate::daemons::state::FileExpectation::Present(&previous),
                || {
                    evacuated_tx.send(()).expect("publish evacuation pause");
                    publish_rx.recv().expect("resume config publication");
                },
            )
            .expect("publish replacement config")
    });
    evacuated_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("config writer evacuated its target");
    assert!(
        !config_path.exists(),
        "config target is intentionally evacuated"
    );
    assert!(
        nib.join("config.json").is_file(),
        "wrong legacy fallback exists"
    );

    let reconcile_root = root.path().to_path_buf();
    let reconciler = std::thread::spawn(move || {
        reconcile_subagent_ownership_until(
            &reconcile_root,
            id,
            Instant::now() + Duration::from_secs(2),
        )
    });
    std::thread::sleep(Duration::from_millis(75));
    let still_running =
        get_subagent_record_unreconciled(root.path(), id).expect("record during evacuation");
    assert_eq!(still_running.status, "running");
    assert!(
        subagent_audit_target(&still_running)
            .expect("valid running metadata")
            .is_none(),
        "an evacuated config must not pin the legacy/default target"
    );
    publish_tx.send(()).expect("resume config writer");
    writer.join().expect("config writer");
    let terminal = reconciler
        .join()
        .expect("reconciliation worker")
        .expect("terminal reconciliation");

    let target = subagent_audit_target(&terminal)
        .expect("valid terminal target")
        .expect("pinned terminal target");
    assert_eq!(
        target.sessions_dir,
        selected_sessions.canonicalize().expect("selected sessions")
    );
    assert_ne!(
        target.sessions_dir,
        wrong_sessions.canonicalize().expect("wrong sessions")
    );
    assert_eq!(
        directory_tree_snapshot(&wrong_sessions),
        wrong_namespace,
        "coherent resolution must not read, pin, or mutate the legacy/default store"
    );
    assert!(nib.join("config.json").is_file());
    assert!(!nib.join("config.json.bak").exists());
    assert!(!nib.join(".legacy-state-migration-v1.json").exists());
    let audit = selected_store
        .load_result("parent")
        .expect("load selected audit session")
        .expect("selected audit session");
    assert_eq!(
        audit
            .events
            .iter()
            .filter(|event| event.kind == "subagent_execution_reconciled")
            .count(),
        1
    );
}
