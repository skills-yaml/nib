//! Split for T043 C02.

use super::*;

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub fn spawn_subagent_cancellable<'a>(
    args: &'a Value,
    project_root: &'a Path,
    cancellation: Option<&'a crate::agent::CancellationSignal>,
) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>> {
    Box::pin(async move {
        if std::env::var_os("NIB_MANAGED_PROCESS_SCOPE").is_some() {
            return Err(
                "nested subagents are not supported inside a foreground managed-process scope"
                    .to_string(),
            );
        }
        tokio::runtime::Handle::try_current()
            .map_err(|_| "spawn_subagent requires an active Tokio runtime".to_string())?;
        let project_root = canonical_project_root(project_root)?;
        #[cfg(not(test))]
        crate::sandbox::process::ProcessScopeBackend::production()?;
        let subagent_id = format!("sub-{}", uuid::Uuid::new_v4());
        let prompt = args
            .get("prompt")
            .and_then(|value| value.as_str())
            .filter(|prompt| !prompt.trim().is_empty())
            .ok_or("missing prompt")?
            .to_string();
        let max_steps = args
            .get("max_steps")
            .and_then(|value| value.as_u64())
            .map(|value| value.clamp(1, 100) as u32)
            .unwrap_or(0);
        let parent_session_id = validate_subagent_audit_argument_pair(args)?;
        let audit_plan = preflight_subagent_audit_target(args, &project_root)?;
        audit_plan.verify_continuity()?;
        let runtime_config = audit_plan.runtime_config().clone();
        if cancellation.is_some_and(crate::agent::CancellationSignal::is_cancelled) {
            return Err("subagent spawn cancelled before mutation".to_string());
        }
        let worktree_plan =
            crate::sandbox::worktree::Worktree::plan_preparation_authority_cancellable(
                &project_root,
                &subagent_id,
                cancellation,
            )
            .await?;
        let records = ensure_records_directory_capability_until(&project_root, None)?;
        reconcile_spawn_preparations(&project_root, &records)?;
        let audit_namespace_plan =
            audit_plan.fallback_namespace_plan_after_records(&subagent_id, &records)?;
        let owner_plan = SubagentOwnerLease::plan();
        let audit_session_id = parent_session_id.as_deref().unwrap_or(&subagent_id);
        let (audit_sessions_dir, initial_audit_target) = audit_plan.durable_audit_destination();
        let mut preparation_intent = Some(SpawnPreparationIntent::create(
            &records,
            &subagent_id,
            owner_plan.clone(),
            worktree_plan.clone(),
            audit_session_id,
            audit_sessions_dir,
            audit_namespace_plan.clone(),
            initial_audit_target,
        )?);
        let preparation_authority = preparation_intent
            .as_ref()
            .ok_or_else(|| "spawn preparation authority was not retained".to_string())?
            .authority
            .clone();
        pause_after_spawn_preparation_intent(&subagent_id)?;
        run_spawn_forward_mutation_hook("worktree");
        let worktree_guard = {
            let authority = preparation_authority.clone();
            let deadline = authority.operation_deadline();
            std::sync::Arc::new(move || authority.verify_until(deadline))
                as std::sync::Arc<dyn Fn() -> Result<(), String> + Send + Sync>
        };
        let worktree =
            match crate::sandbox::worktree::Worktree::create_cancellable_from_preparation_authority_with_guard(
                &project_root,
                &worktree_plan,
                cancellation,
                worktree_guard,
            )
            .await
            {
                Ok(worktree) => worktree,
                Err(error) => {
                    // See the synchronous path: a constructor error can own
                    // partial durable worktree state and therefore retains the
                    // preparation intent for restart reconciliation.
                    drop(preparation_intent.take());
                    return Err(error);
                }
            };
        run_spawn_forward_mutation_hook("child_config");
        if let Err(error) = prepare_child_runtime_config_with_authority(
            &runtime_config,
            &worktree.path,
            &preparation_authority,
        ) {
            let worktree_cleanup = cleanup_precommit_worktree_with_authority(
                &project_root,
                &worktree,
                preparation_authority.clone(),
            )
            .await;
            let cleanup = worktree_cleanup
                .as_ref()
                .err()
                .map(|cleanup| format!("; worktree cleanup failed: {cleanup}"))
                .unwrap_or_default();
            let intent_cleanup = if worktree_cleanup.is_ok() {
                preparation_intent
                    .take()
                    .map(SpawnPreparationIntent::cleanup)
                    .transpose()
                    .err()
                    .map(|value| format!("; preparation intent cleanup failed: {value}"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            return Err(format!(
                "failed to prepare subagent runtime: {error}{cleanup}{intent_cleanup}"
            ));
        }
        if cancellation.is_some_and(crate::agent::CancellationSignal::is_cancelled) {
            cleanup_precommit_worktree_with_authority(
                &project_root,
                &worktree,
                preparation_authority.clone(),
            )
            .await?;
            if let Some(intent) = preparation_intent.take() {
                intent.cleanup()?;
            }
            return Err("subagent spawn cancelled before commit".to_string());
        }
        run_spawn_forward_mutation_hook("owner");
        let owner_lease =
            match create_spawn_owner_lease(&project_root, &owner_plan, &preparation_authority) {
                Ok(owner_lease) => owner_lease,
                Err(error) => {
                    let worktree_cleanup = cleanup_precommit_worktree_with_authority(
                        &project_root,
                        &worktree,
                        preparation_authority.clone(),
                    )
                    .await;
                    let cleanup = worktree_cleanup
                        .as_ref()
                        .err()
                        .map(|cleanup| format!("; worktree cleanup failed: {cleanup}"))
                        .unwrap_or_default();
                    let intent_cleanup = if !error.mutation_indeterminate
                        && worktree_cleanup.is_ok()
                    {
                        preparation_intent
                            .take()
                            .map(SpawnPreparationIntent::cleanup)
                            .transpose()
                            .err()
                            .map(|value| format!("; preparation intent cleanup failed: {value}"))
                            .unwrap_or_default()
                    } else {
                        drop(preparation_intent.take());
                        String::new()
                    };
                    return Err(format!(
                    "failed to establish subagent execution ownership: {}{cleanup}{intent_cleanup}",
                    error.message
                ));
                }
            };
        if let Some(intent) = preparation_intent.as_mut() {
            if let Err(error) =
                intent.revise(SpawnPreparationPhase::ResourcesPrepared, None, None, None)
            {
                let worktree_cleanup = cleanup_precommit_worktree_with_authority(
                    &project_root,
                    &worktree,
                    preparation_authority.clone(),
                )
                .await
                .err()
                .map(|value| format!("; worktree cleanup failed: {value}"))
                .unwrap_or_default();
                let owner_cleanup = compensate_owner_lease_with_authority(
                    owner_lease,
                    OwnerLeaseCompensation::Remove,
                    &preparation_authority,
                )
                .err()
                .map(|value| format!("; owner cleanup failed: {value}"))
                .unwrap_or_default();
                return Err(format!("{error}{worktree_cleanup}{owner_cleanup}"));
            }
        }
        run_spawn_forward_mutation_hook("audit");
        let prepared_audit = match commit_subagent_audit_target(
            audit_plan,
            audit_session_id,
            Some(&worktree),
            preparation_intent.as_mut(),
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                let cleanup = cleanup_precommit_worktree_with_authority(
                    &project_root,
                    &worktree,
                    preparation_authority.clone(),
                )
                .await
                .err()
                .map(|cleanup| format!("; worktree cleanup failed: {cleanup}"))
                .unwrap_or_default();
                let owner_cleanup = compensate_owner_lease_with_authority(
                    owner_lease,
                    OwnerLeaseCompensation::Remove,
                    &preparation_authority,
                )
                .err()
                .map(|cleanup| format!("; owner lease cleanup failed: {cleanup}"))
                .unwrap_or_default();
                // Audit initialization can fail after an indeterminate session
                // cleanup. Retain the intent regardless of the other exact
                // cleanup outcomes.
                drop(preparation_intent);
                return Err(format!("{error}{cleanup}{owner_cleanup}"));
            }
        };
        if let Some(intent) = preparation_intent.as_mut() {
            if intent.data.phase == SpawnPreparationPhase::ResourcesPrepared {
                if let Err(error) =
                    intent.revise(SpawnPreparationPhase::AuditPublished, None, None, None)
                {
                    let cleanup_errors = collect_spawn_compensation_async_with_audit(
                        || Ok(()),
                        cleanup_precommit_worktree_with_authority(
                            &project_root,
                            &worktree,
                            preparation_authority.clone(),
                        ),
                        |action| {
                            compensate_owner_lease_with_authority(
                                owner_lease,
                                action,
                                &preparation_authority,
                            )
                        },
                        prepared_audit,
                        &preparation_authority,
                    )
                    .await;
                    // The failed revision can have recoverable publication
                    // artifacts, so durable restart authority must remain.
                    drop(preparation_intent);
                    return if cleanup_errors.is_empty() {
                        Err(error)
                    } else {
                        Err(format!("{error}; {}", cleanup_errors.join("; ")))
                    };
                }
            }
        }
        if let Err(error) = pause_after_subagent_audit_preparation(&subagent_id) {
            let mut cleanup_errors = collect_spawn_compensation_async_with_audit(
                || Ok(()),
                cleanup_precommit_worktree_with_authority(
                    &project_root,
                    &worktree,
                    preparation_authority.clone(),
                ),
                |action| {
                    compensate_owner_lease_with_authority(
                        owner_lease,
                        action,
                        &preparation_authority,
                    )
                },
                prepared_audit,
                &preparation_authority,
            )
            .await;
            if cleanup_errors.is_empty() {
                if let Some(intent) = preparation_intent {
                    if let Err(cleanup) = intent.cleanup() {
                        cleanup_errors
                            .push(format!("preparation intent cleanup failed: {cleanup}"));
                    }
                }
            } else {
                drop(preparation_intent);
            }
            return if cleanup_errors.is_empty() {
                Err(error)
            } else {
                Err(format!("{error}; {}", cleanup_errors.join("; ")))
            };
        }
        let audit_target = prepared_audit.encoded.clone();
        #[cfg(test)]
        if consume_spawn_failure(&SPAWN_POST_AUDIT_CANCELLATIONS) {
            if let Some(cancellation) = cancellation {
                cancellation.cancel();
            }
        }
        if cancellation.is_some_and(crate::agent::CancellationSignal::is_cancelled) {
            let mut cleanup_errors = Vec::new();
            if let Err(cleanup) = prepared_audit.cleanup_with_authority(&preparation_authority) {
                cleanup_errors.push(format!("audit cleanup failed: {cleanup}"));
            }
            if let Err(cleanup) = cleanup_precommit_worktree_with_authority(
                &project_root,
                &worktree,
                preparation_authority.clone(),
            )
            .await
            {
                cleanup_errors.push(format!("worktree cleanup failed: {cleanup}"));
            }
            if let Err(cleanup) = compensate_owner_lease_with_authority(
                owner_lease,
                OwnerLeaseCompensation::Remove,
                &preparation_authority,
            ) {
                cleanup_errors.push(format!("owner lease cleanup failed: {cleanup}"));
            }
            if cleanup_errors.is_empty() {
                if let Some(intent) = preparation_intent {
                    if let Err(cleanup) = intent.cleanup() {
                        cleanup_errors
                            .push(format!("preparation intent cleanup failed: {cleanup}"));
                    }
                }
            } else {
                drop(preparation_intent);
            }
            return if cleanup_errors.is_empty() {
                Err("subagent spawn cancelled before commit".to_string())
            } else {
                Err(format!(
                    "subagent spawn cancelled before commit; {}",
                    cleanup_errors.join("; ")
                ))
            };
        }
        let handoff_evidence = preparation_intent
            .as_ref()
            .map(|intent| spawn_handoff_evidence(&intent.data, "pending"))
            .ok_or("spawn preparation authority was lost before record construction")?;
        let mut record = SubagentRecord {
            id: subagent_id.clone(),
            parent_session_id: parent_session_id.clone(),
            child_session_id: subagent_id.clone(),
            prompt: prompt.clone(),
            status: "running".to_string(),
            execution_generation: Some(owner_lease.execution_generation),
            owner_lease: Some(owner_lease.lease_id.clone()),
            worktree_path: worktree.path.clone(),
            branch: worktree.branch.clone(),
            branch_oid: Some(worktree.branch_oid.clone()),
            result: Some(json!({
                "_ownership_audit_target": audit_target,
                "_worktree_ownership_receipt": worktree.preparation_authority().ownership_receipt_id,
                "_spawn_handoff": handoff_evidence,
            })),
            error: None,
            verification: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let publication_result = match preparation_intent.as_ref() {
            Some(intent) => {
                write_spawn_subagent_record_locked(&project_root, &record, &intent.authority)
            }
            None => Err(InitialSubagentRecordPublicationError {
                message: "spawn preparation authority was lost before record publication"
                    .to_string(),
                receipt: None,
                publication_attempted: false,
            }),
        };
        let mut publication = match publication_result {
            Ok(publication) => publication,
            Err(error) => {
                let message = error.message.clone();
                let compensation_errors = collect_spawn_compensation_async_with_audit(
                    || {
                        cleanup_record_after_publication_failure_locked(
                            &project_root,
                            &record,
                            &error,
                            &preparation_authority,
                        )
                    },
                    cleanup_precommit_worktree_with_authority(
                        &project_root,
                        &worktree,
                        preparation_authority.clone(),
                    ),
                    |action| {
                        compensate_owner_lease_with_authority(
                            owner_lease,
                            action,
                            &preparation_authority,
                        )
                    },
                    prepared_audit,
                    &preparation_authority,
                )
                .await;
                let mut compensation_errors = compensation_errors;
                if compensation_errors.is_empty() {
                    if let Some(intent) = preparation_intent {
                        if let Err(cleanup) = intent.cleanup() {
                            compensation_errors
                                .push(format!("preparation intent compensation failed: {cleanup}"));
                        }
                    }
                }
                return if compensation_errors.is_empty() {
                    Err(message)
                } else {
                    Err(format!("{message}; {}", compensation_errors.join("; ")))
                };
            }
        };
        run_spawn_handoff_phase_hook("record_published");
        if let Some(intent) = preparation_intent.as_mut() {
            if let Err(error) =
                intent.revise(SpawnPreparationPhase::RecordPublished, None, None, None)
            {
                let compensation_errors = collect_spawn_compensation_async_with_audit(
                    || {
                        cleanup_record_after_registration_failure_locked(
                            &project_root,
                            &record,
                            &publication,
                            &preparation_authority,
                        )
                    },
                    cleanup_precommit_worktree_with_authority(
                        &project_root,
                        &worktree,
                        preparation_authority.clone(),
                    ),
                    |action| {
                        compensate_owner_lease_with_authority(
                            owner_lease,
                            action,
                            &preparation_authority,
                        )
                    },
                    prepared_audit,
                    &preparation_authority,
                )
                .await;
                drop(preparation_intent);
                return if compensation_errors.is_empty() {
                    Err(error)
                } else {
                    Err(format!("{error}; {}", compensation_errors.join("; ")))
                };
            }
        }
        let start_gate = match crate::daemons::task::TASK_MANAGER
            .register_paused_task(subagent_id.clone(), "subagent")
        {
            Ok(start_gate) => start_gate,
            Err(error) => {
                let compensation_errors = collect_spawn_compensation_async_with_audit(
                    || {
                        cleanup_record_after_registration_failure_locked(
                            &project_root,
                            &record,
                            &publication,
                            &preparation_authority,
                        )
                    },
                    cleanup_precommit_worktree_with_authority(
                        &project_root,
                        &worktree,
                        preparation_authority.clone(),
                    ),
                    |action| {
                        compensate_owner_lease_with_authority(
                            owner_lease,
                            action,
                            &preparation_authority,
                        )
                    },
                    prepared_audit,
                    &preparation_authority,
                )
                .await;
                let mut compensation_errors = compensation_errors;
                if compensation_errors.is_empty() {
                    if let Some(intent) = preparation_intent {
                        if let Err(cleanup) = intent.cleanup() {
                            compensation_errors
                                .push(format!("preparation intent compensation failed: {cleanup}"));
                        }
                    }
                }
                return if compensation_errors.is_empty() {
                    Err(error)
                } else {
                    Err(format!("{error}; {}", compensation_errors.join("; ")))
                };
            }
        };
        run_spawn_handoff_phase_hook("manager_registered");
        if let Some(intent) = preparation_intent.as_mut() {
            if let Err(error) =
                intent.revise(SpawnPreparationPhase::ManagerRegistered, None, None, None)
            {
                let mut compensation_errors = Vec::new();
                if let Err(rollback) =
                    crate::daemons::task::TASK_MANAGER.rollback_unattached_task(&subagent_id)
                {
                    compensation_errors
                        .push(format!("task registration rollback failed: {rollback}"));
                }
                compensation_errors.extend(
                    collect_spawn_compensation_async_with_audit(
                        || {
                            cleanup_record_after_registration_failure_locked(
                                &project_root,
                                &record,
                                &publication,
                                &preparation_authority,
                            )
                        },
                        cleanup_precommit_worktree_with_authority(
                            &project_root,
                            &worktree,
                            preparation_authority.clone(),
                        ),
                        |action| {
                            compensate_owner_lease_with_authority(
                                owner_lease,
                                action,
                                &preparation_authority,
                            )
                        },
                        prepared_audit,
                        &preparation_authority,
                    )
                    .await,
                );
                drop(preparation_intent);
                return if compensation_errors.is_empty() {
                    Err(error)
                } else {
                    Err(format!("{error}; {}", compensation_errors.join("; ")))
                };
            }
        }
        prepared_audit.disarm();
        let launch = launch_subagent_task(
            project_root.clone(),
            subagent_id.clone(),
            prompt,
            max_steps,
            parent_session_id,
            PreparedSubagentTask {
                record: record.clone(),
                worktree,
                owner_lease,
            },
            start_gate,
            &preparation_authority,
            preparation_intent
                .as_ref()
                .and_then(|intent| intent.data.process_scope_plan.clone()),
        );
        let mut launched = match launch {
            Ok(launched) => launched,
            Err(error) => {
                let deadline = preparation_authority.operation_deadline();
                let execution_generation = record.execution_generation;
                let owner_lease = record.owner_lease.clone();
                let rollback = crate::daemons::task::TASK_MANAGER
                    .rollback_unattached_task(&subagent_id)
                    .err()
                    .map(|value| format!("; task registration rollback failed: {value}"))
                    .unwrap_or_default();
                drop(preparation_intent);
                drop(publication);
                drop(preparation_authority);
                let persistence = persist_unstarted_after_preparation_unlock(
                    &project_root,
                    &subagent_id,
                    execution_generation,
                    owner_lease.as_deref(),
                    &error,
                    deadline,
                )
                .err()
                .map(|value| format!("; unstarted outcome persistence failed: {value}"))
                .unwrap_or_default();
                return Err(format!("{error}{rollback}{persistence}"));
            }
        };
        let response = launched.response.clone();
        run_spawn_handoff_phase_hook("handoff_established");
        if let Err(error) = pause_after_supervisor_ready_before_commit(&subagent_id) {
            let cancellation = crate::daemons::task::TASK_MANAGER
                .cancel(&subagent_id)
                .err()
                .map(|value| format!("; gated task cancellation failed: {value}"))
                .unwrap_or_default();
            drop(preparation_intent);
            drop(publication);
            drop(preparation_authority);
            return Err(format!(
                "subagent supervisor READY barrier failed: {error}{cancellation}"
            ));
        }
        let intent = preparation_intent
            .as_mut()
            .ok_or("spawn preparation authority was lost before handoff commit")?;
        if let Err(error) = intent.revise(
            SpawnPreparationPhase::HandoffProven,
            None,
            None,
            launched.precommit_process_scope(),
        ) {
            let cancellation = crate::daemons::task::TASK_MANAGER
                .cancel(&subagent_id)
                .err()
                .map(|value| format!("; launched task cancellation failed: {value}"))
                .unwrap_or_default();
            drop(preparation_intent);
            drop(publication);
            drop(preparation_authority);
            return Err(format!(
                "failed to persist subagent handoff: {error}{cancellation}"
            ));
        }
        run_spawn_handoff_phase_hook("handoff_proven");
        if let Err(error) =
            commit_spawn_handoff_record_locked(&project_root, &mut record, &mut publication, intent)
        {
            let cancellation = crate::daemons::task::TASK_MANAGER
                .cancel(&subagent_id)
                .err()
                .map(|value| format!("; launched task cancellation failed: {value}"))
                .unwrap_or_default();
            drop(preparation_intent);
            drop(publication);
            drop(preparation_authority);
            return Err(format!(
                "failed to commit subagent handoff: {error}{cancellation}"
            ));
        }
        run_spawn_handoff_phase_hook("handoff_committed");
        if let Err(error) = launched.commit_supervisor_handoff(&preparation_authority) {
            let cancellation = crate::daemons::task::TASK_MANAGER
                .cancel(&subagent_id)
                .err()
                .map(|value| format!("; handed-off task cancellation failed: {value}"))
                .unwrap_or_default();
            drop(publication);
            drop(preparation_authority);
            return Err(format!(
                "failed to acknowledge committed subagent supervisor handoff: {error}{cancellation}"
            ));
        }
        run_spawn_handoff_phase_hook("supervisor_started");
        if let Err(error) =
            preparation_authority.verify_until(preparation_authority.operation_deadline())
        {
            let cancellation = crate::daemons::task::TASK_MANAGER
                .cancel(&subagent_id)
                .err()
                .map(|value| format!("; handed-off task cancellation failed: {value}"))
                .unwrap_or_default();
            drop(publication);
            drop(preparation_authority);
            return Err(format!(
                "subagent handoff deadline expired before start-gate release: {error}{cancellation}"
            ));
        }
        if let Err(error) = crate::daemons::task::TASK_MANAGER.start_task(&subagent_id) {
            let cancellation = crate::daemons::task::TASK_MANAGER
                .cancel(&subagent_id)
                .err()
                .map(|value| format!("; handed-off task cancellation failed: {value}"))
                .unwrap_or_default();
            drop(publication);
            drop(preparation_authority);
            return Err(format!(
                "failed to release committed subagent start gate: {error}{cancellation}"
            ));
        }
        run_spawn_handoff_phase_hook("launch_released");
        run_spawn_handoff_phase_hook("before_intent_retirement");
        let intent = preparation_intent
            .take()
            .ok_or("spawn preparation authority was lost before final retirement")?;
        if let Err(error) = intent.cleanup() {
            let cancellation = crate::daemons::task::TASK_MANAGER
                .cancel(&subagent_id)
                .err()
                .map(|value| format!("; launched task cancellation failed: {value}"))
                .unwrap_or_default();
            drop(publication);
            drop(preparation_authority);
            return Err(format!(
                "failed to retire authoritative spawn preparation after handoff: {error}{cancellation}"
            ));
        }
        run_spawn_handoff_phase_hook("intent_retired");
        drop(publication);
        drop(preparation_authority);
        Ok(response)
    })
}

#[cfg(test)]
// The test launcher mirrors the production lifecycle boundary, whose request,
// durable authorities, and start gate must remain independently inspectable.
#[allow(clippy::too_many_arguments)]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn launch_subagent_task(
    project_root: PathBuf,
    subagent_id: String,
    prompt: String,
    max_steps: u32,
    parent_session_id: Option<String>,
    prepared: PreparedSubagentTask,
    start_gate: tokio::sync::oneshot::Receiver<()>,
    authority: &SpawnPreparationAuthority,
    process_scope_plan: Option<SubagentProcessScopePlan>,
) -> Result<LaunchedSubagentTask, String> {
    let deadline = authority.operation_deadline();
    authority.verify_until(deadline)?;
    let process_scope_plan = process_scope_plan
        .ok_or_else(|| "test subagent launch lacks preplanned process authority".to_string())?;
    let PreparedSubagentTask {
        record,
        worktree,
        owner_lease,
    } = prepared;
    let record_id = subagent_id.clone();
    let child_session_id = subagent_id.clone();
    let worktree_path = worktree.path;
    let record_root = project_root.clone();
    let execution_generation = owner_lease.execution_generation;
    let lease_id = owner_lease.lease_id.clone();
    let task_lease_id = lease_id.clone();
    let process_identity = crate::sandbox::process::ProcessIdentity::current()?;
    #[cfg(target_os = "linux")]
    let process_backend = crate::sandbox::process::ProcessScopeBackend::LinuxPidNamespace;
    #[cfg(windows)]
    let process_backend = crate::sandbox::process::ProcessScopeBackend::WindowsJobObject;
    #[cfg(target_os = "macos")]
    let process_backend = crate::sandbox::process::ProcessScopeBackend::MacosProcessGroup;
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    let process_backend =
        return Err("test subagent process scope is unsupported on this platform".to_string());
    let now = Utc::now();
    let precommit_process_scope = crate::sandbox::process::ProcessScopeRecord {
        version: 2,
        scope_id: subagent_id.clone(),
        workload_kind: "subagent".to_string(),
        execution_generation,
        cleanup_lease_id: process_scope_plan.cleanup_lease_id,
        supervisor_registration_nonce: Some(process_scope_plan.supervisor_registration_nonce),
        owner: process_identity.clone(),
        backend: process_backend,
        status: crate::sandbox::process::ProcessScopeStatus::Running,
        launch_committed: Some(false),
        supervisor: Some(process_identity.clone()),
        direct_child: Some(process_identity),
        cleanup_reason: None,
        cleanup_proof: None,
        launch_abort_proof: None,
        created_at: now,
        updated_at: now,
    };
    let guard = SubagentRunGuard::new(record_root.clone(), record_id.clone(), owner_lease);
    let session_lock_policy = crate::session::SessionStore::current_lock_policy();
    let handle = tokio::spawn(crate::session::SessionStore::with_optional_lock_policy(
        session_lock_policy,
        async move {
            let mut guard = guard;
            if start_gate.await.is_err() {
                guard.reason =
                    "subagent execution handoff was cancelled before durable commit".to_string();
                return;
            }
            let config = crate::agent::AgentLoopConfig {
                max_steps,
                auto_approve: false,
                approval_handler: Some(Arc::new(NonInteractiveSubagentApproval)),
                ..Default::default()
            };
            let outcome =
                crate::agent::run_agent_loop(worktree_path, &child_session_id, &prompt, config)
                    .await;
            match persist_subagent_outcome(
                &record_root,
                &record_id,
                execution_generation,
                &task_lease_id,
                outcome,
            ) {
                Ok(()) => guard.disarm(),
                Err(error) => {
                    guard.reason = format!("{INTERRUPTED_ERROR}: {error}");
                }
            }
        },
    ));
    if let Err(error) =
        crate::daemons::task::TASK_MANAGER.attach_abort_handle(&subagent_id, handle.abort_handle())
    {
        handle.abort();
        persist_interrupted_subagent(
            &project_root,
            &subagent_id,
            execution_generation,
            &lease_id,
            &error,
        );
        return Err(error);
    }
    if let Err(error) = authority.verify_until(deadline) {
        handle.abort();
        return Err(format!(
            "subagent execution handoff exceeded its preparation deadline: {error}"
        ));
    }

    Ok(LaunchedSubagentTask {
        response: public_subagent_start_response(&record, parent_session_id),
        precommit_process_scope: Some(precommit_process_scope),
    })
}

pub(crate) fn public_subagent_start_response(
    record: &SubagentRecord,
    parent_session_id: Option<String>,
) -> Value {
    json!({
        "status": "started",
        "subagent_id": record.id,
        "parent_session_id": parent_session_id,
        "child_session_id": record.child_session_id,
    })
}

#[cfg(not(test))]
pub(crate) fn monitor_subagent_supervisor_exit(
    mut child: std::process::Child,
    owner_lease: SubagentOwnerLease,
    project_root: PathBuf,
    subagent_id: String,
    execution_generation: u64,
    lease_id: String,
    wait_tx: tokio::sync::oneshot::Sender<Result<std::process::ExitStatus, String>>,
) {
    let status = child.wait().map_err(|error| error.to_string());
    let initial_record = get_subagent_record_unreconciled(&project_root, &subagent_id).ok();
    let completion_verified = initial_record.as_ref().is_some_and(|record| {
        record.status != "running"
            && process_scope_retirement_result(record)
                .is_none_or(|result| !result.contains_key("ownership_reconciliation"))
            && has_direct_terminal_process_scope_authority(record)
            && matches!(terminal_process_scope_authority(record), Ok(Some(_)))
            && retire_terminal_process_scope(&project_root, record).unwrap_or(false)
    });
    let (lease_cleanup, record) = if completion_verified {
        (owner_lease.remove(), initial_record)
    } else {
        let cleanup = owner_lease.release_for_reconciliation();
        let reconciled = match reconcile_subagent_ownership_with_owner_state(
            &project_root,
            &subagent_id,
            true,
        ) {
            Ok(record) => Some(record),
            Err(_)
                if initial_record.as_ref().is_some_and(|record| {
                    process_scope_retirement_result(record)
                        .is_some_and(|result| result.contains_key("ownership_reconciliation"))
                }) =>
            {
                None
            }
            Err(_) => initial_record,
        };
        (cleanup, reconciled)
    };
    let lease_cleanup_succeeded = lease_cleanup.is_ok();
    if let Err(error) = lease_cleanup {
        persist_owner_lease_cleanup_error(
            &project_root,
            &subagent_id,
            execution_generation,
            &lease_id,
            &error,
        );
    }
    if let Some(record) = record {
        if lease_cleanup_succeeded {
            let public_result = record
                .result
                .clone()
                .and_then(project_public_subagent_result);
            match record.status.as_str() {
                "completed" => {
                    crate::daemons::task::TASK_MANAGER.complete(&subagent_id, public_result)
                }
                "failed" => crate::daemons::task::TASK_MANAGER.fail(
                    &subagent_id,
                    record
                        .error
                        .clone()
                        .unwrap_or_else(|| "subagent supervisor failed".to_string()),
                    public_result,
                ),
                _ => {}
            }
        }
    }
    let _ = wait_tx.send(status);
}

pub(crate) fn retire_terminal_process_scope(
    project_root: &Path,
    expected: &SubagentRecord,
) -> Result<bool, String> {
    retire_terminal_process_scope_until(project_root, expected, None)
}

pub(crate) fn retire_terminal_process_scope_until(
    project_root: &Path,
    expected: &SubagentRecord,
    deadline: Option<Instant>,
) -> Result<bool, String> {
    if !status_retains_process_scope_retirement_authority(&expected.status) {
        return Ok(false);
    }
    ensure_subagent_reconciliation_deadline(deadline)?;
    let project_root = canonical_project_root(project_root)?;
    let path = record_path(&project_root, &expected.id)?;
    let records_directory = ensure_records_directory_until(&project_root, deadline)?;
    with_subagent_reconciliation_lock_in(
        &project_root,
        &expected.id,
        &records_directory,
        deadline,
        |directory, deadline| {
            ensure_subagent_reconciliation_deadline(deadline)?;
            let opened = read_opened_subagent_record_in(directory, &path)?;
            validate_reopened_subagent_record(expected, &opened.record)?;
            let Some((execution_generation, authority)) =
                terminal_process_scope_authority(&opened.record)?
            else {
                return Ok(false);
            };
            directory.verify_file_identity(&path, &opened.file)?;
            ensure_subagent_reconciliation_deadline(deadline)?;
            let retired = retire_process_scope_authority_in_locked_records_until(
                &project_root,
                directory,
                &opened.record.id,
                execution_generation,
                &authority,
                deadline.ok_or_else(subagent_reconciliation_deadline_elapsed)?,
            )?;
            ensure_subagent_reconciliation_deadline(deadline)?;
            directory.verify_file_identity(&path, &opened.file)?;
            Ok(retired)
        },
    )
}

pub(crate) fn retire_terminal_process_scope_in_locked_records_until(
    project_root: &Path,
    records: &crate::daemons::state::StableDirectory,
    record: &SubagentRecord,
    deadline: Instant,
) -> Result<(), String> {
    let Some((generation, authority)) = terminal_process_scope_authority(record)? else {
        return Err(
            "terminal subagent has no exact process-scope retirement authority".to_string(),
        );
    };
    let _retired = retire_process_scope_authority_in_locked_records_until(
        project_root,
        records,
        &record.id,
        generation,
        &authority,
        deadline,
    )?;
    Ok(())
}

pub(crate) fn retire_process_scope_authority_in_locked_records_until(
    project_root: &Path,
    records: &crate::daemons::state::StableDirectory,
    scope_id: &str,
    execution_generation: u64,
    authority: &TerminalProcessScopeAuthority,
    deadline: Instant,
) -> Result<bool, String> {
    let Some(store) = crate::sandbox::process::ProcessScopeStore::open_existing_bound_to_records(
        project_root,
        records,
        deadline,
    )?
    else {
        return Ok(false);
    };
    match authority {
        TerminalProcessScopeAuthority::Cleanup(proof) => {
            store.retire_complete(scope_id, execution_generation, proof)
        }
        TerminalProcessScopeAuthority::LaunchAbort(proof) => {
            store.retire_launch_abort(scope_id, execution_generation, proof)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TerminalProcessScopeAuthority {
    Cleanup(crate::sandbox::process::CleanupProof),
    LaunchAbort(crate::sandbox::process::LaunchAbortProof),
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn terminal_process_scope_authority(
    record: &SubagentRecord,
) -> Result<Option<(u64, TerminalProcessScopeAuthority)>, String> {
    if !status_retains_process_scope_retirement_authority(&record.status) {
        return Ok(None);
    }
    let execution_generation = record.execution_generation.ok_or_else(|| {
        format!(
            "terminal subagent {} has no execution generation for scope retirement",
            record.id
        )
    })?;
    let owner_lease = record.owner_lease.as_deref().ok_or_else(|| {
        format!(
            "terminal subagent {} has no owner lease for scope retirement",
            record.id
        )
    })?;
    validate_execution_ownership(execution_generation, owner_lease)?;
    let Some(result) = process_scope_retirement_result(record) else {
        return Ok(None);
    };

    let direct_cleanup_proof = match (
        result.get("cleanup_verified").and_then(Value::as_bool),
        result.get("cleanup_proof"),
    ) {
        (Some(true), Some(proof)) => Some(parse_terminal_cleanup_proof(proof)?),
        (Some(true), None) => {
            return Err("verified terminal subagent result has no cleanup proof".to_string());
        }
        (_, Some(_)) => {
            return Err(
                "terminal subagent result has a cleanup proof without verified cleanup".to_string(),
            );
        }
        _ => None,
    };
    let direct_launch_abort_proof = parse_terminal_launch_abort_authority(result)?;

    let (reconciled_cleanup_proof, reconciled_launch_abort_proof) =
        match result.get("ownership_reconciliation") {
            None => None,
            Some(evidence) => {
                let evidence = evidence
                    .as_object()
                    .ok_or("terminal ownership reconciliation is not an object")?;
                if evidence.get("subagent_id").and_then(Value::as_str) != Some(record.id.as_str())
                    || evidence.get("execution_generation").and_then(Value::as_u64)
                        != Some(execution_generation)
                    || evidence.get("owner_lease").and_then(Value::as_str) != Some(owner_lease)
                    || !retirement_terminal_status_matches(
                        &record.status,
                        evidence.get("terminal_status").and_then(Value::as_str),
                    )
                {
                    return Err(
                        "terminal ownership reconciliation does not match subagent execution ownership"
                            .to_string(),
                    );
                }
                let expected_reconciliation_id = subagent_reconciliation_id(
                    &record.id,
                    execution_generation,
                    owner_lease,
                )?;
                if evidence
                    .get("reconciliation_id")
                    .is_some_and(|observed| {
                        observed.as_str() != Some(expected_reconciliation_id.as_str())
                    })
                {
                    return Err(
                        "terminal ownership reconciliation has an invalid reconciliation identity"
                            .to_string(),
                    );
                }
                let cleanup_proof = match (
                    evidence.get("cleanup_verified").and_then(Value::as_bool),
                    evidence.get("cleanup_proof"),
                ) {
                    (Some(true), Some(proof)) => Some(parse_terminal_cleanup_proof(proof)?),
                    (Some(true), None) => {
                        return Err(
                            "verified terminal ownership reconciliation has no cleanup proof"
                                .to_string(),
                        );
                    }
                    (_, Some(_)) => {
                        return Err(
                            "terminal ownership reconciliation has a cleanup proof without verified cleanup"
                                .to_string(),
                        );
                    }
                    _ => None,
                };
                let launch_abort_proof = parse_terminal_launch_abort_authority(evidence)?;
                Some((cleanup_proof, launch_abort_proof))
            }
        }
        .unwrap_or((None, None));

    let cleanup_proof = match (direct_cleanup_proof, reconciled_cleanup_proof) {
        (Some(direct), Some(reconciled)) if direct != reconciled => {
            return Err("terminal subagent cleanup proofs conflict".to_string());
        }
        (Some(proof), _) | (_, Some(proof)) => Some(proof),
        (None, None) => None,
    };
    let launch_abort_proof = match (direct_launch_abort_proof, reconciled_launch_abort_proof) {
        (Some(direct), Some(reconciled)) if direct != reconciled => {
            return Err("terminal subagent launch-abort proofs conflict".to_string());
        }
        (Some(proof), _) | (_, Some(proof)) => Some(proof),
        (None, None) => None,
    };
    match (cleanup_proof, launch_abort_proof) {
        (Some(_), Some(_)) => {
            Err("terminal subagent carries conflicting managed-process authorities".to_string())
        }
        (Some(proof), None) => {
            if proof.execution_generation != execution_generation || !proof.descendants_reaped {
                return Err(
                    "terminal subagent cleanup proof does not own its execution".to_string()
                );
            }
            Ok(Some((
                execution_generation,
                TerminalProcessScopeAuthority::Cleanup(proof),
            )))
        }
        (None, Some(proof)) => {
            if proof.execution_generation != execution_generation || !proof.workload_never_launched
            {
                return Err(
                    "terminal subagent launch-abort proof does not own its execution".to_string(),
                );
            }
            Ok(Some((
                execution_generation,
                TerminalProcessScopeAuthority::LaunchAbort(proof),
            )))
        }
        (None, None) => Ok(None),
    }
}

pub(crate) fn has_direct_terminal_process_scope_authority(record: &SubagentRecord) -> bool {
    process_scope_retirement_result(record).is_some_and(|result| {
        result.contains_key("cleanup_proof") || result.contains_key("launch_abort_proof")
    })
}

pub(crate) fn parse_terminal_launch_abort_authority(
    result: &serde_json::Map<String, Value>,
) -> Result<Option<crate::sandbox::process::LaunchAbortProof>, String> {
    match (
        result.get("launch_abort_verified").and_then(Value::as_bool),
        result
            .get("workload_never_launched")
            .and_then(Value::as_bool),
        result.get("launch_abort_proof"),
    ) {
        (Some(true), Some(true), Some(proof)) => {
            if result.get("cleanup_verified").and_then(Value::as_bool) == Some(true)
                || result.get("cleanup_proof").is_some()
            {
                return Err(
                    "terminal launch-abort authority conflicts with cleanup verification"
                        .to_string(),
                );
            }
            serde_json::from_value(proof.clone())
                .map(Some)
                .map_err(|error| {
                    format!("invalid terminal managed-process launch-abort proof: {error}")
                })
        }
        (Some(true), _, _) => {
            Err("verified terminal launch abort has incomplete proof evidence".to_string())
        }
        (_, _, Some(_)) => {
            Err("terminal launch-abort proof is present without verified launch abort".to_string())
        }
        _ => Ok(None),
    }
}

pub(crate) fn status_retains_process_scope_retirement_authority(status: &str) -> bool {
    matches!(
        status,
        "completed"
            | "failed"
            | "cancelled"
            | "verification_failed"
            | MERGE_FAILED_STATUS
            | MERGE_PENDING_STATUS
            | "merged"
    )
}

pub(crate) fn process_scope_retirement_result(
    record: &SubagentRecord,
) -> Option<&serde_json::Map<String, Value>> {
    let result = record.result.as_ref()?.as_object()?;
    if matches!(record.status.as_str(), MERGE_PENDING_STATUS | "merged") {
        result.get("subagent_result")?.as_object()
    } else {
        Some(result)
    }
}

pub(crate) fn retirement_terminal_status_matches(
    record_status: &str,
    terminal_status: Option<&str>,
) -> bool {
    if matches!(
        record_status,
        "verification_failed" | MERGE_FAILED_STATUS | MERGE_PENDING_STATUS | "merged"
    ) {
        terminal_status == Some("completed")
    } else {
        terminal_status == Some(record_status)
    }
}

pub(crate) fn parse_terminal_cleanup_proof(
    proof: &Value,
) -> Result<crate::sandbox::process::CleanupProof, String> {
    serde_json::from_value(proof.clone())
        .map_err(|error| format!("invalid terminal managed-process cleanup proof: {error}"))
}
