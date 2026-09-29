//! Split for T043 C02.

use super::*;

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub fn spawn_subagent(args: &Value, project_root: &Path) -> Result<Value, String> {
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
    let worktree_plan = crate::sandbox::worktree::Worktree::plan_preparation_authority(
        &project_root,
        &subagent_id,
    )?;
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
        match crate::sandbox::worktree::Worktree::create_from_preparation_authority_with_guard(
            &project_root,
            &worktree_plan,
            worktree_guard,
        ) {
            Ok(worktree) => worktree,
            Err(error) => {
                // The constructor may already have published a durable
                // reservation, branch, path, or Git registration. Its exact
                // extent is authoritative only in the retained intent, so a
                // constructor error never retires that intent in-process.
                drop(preparation_intent);
                return Err(error);
            }
        };
    run_spawn_forward_mutation_hook("child_config");
    if let Err(error) = prepare_child_runtime_config_with_authority(
        &runtime_config,
        &worktree.path,
        &preparation_authority,
    ) {
        let worktree_cleanup = cleanup_precommit_worktree_sync_with_authority(
            &project_root,
            &worktree,
            &preparation_authority,
        );
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
    run_spawn_forward_mutation_hook("owner");
    let owner_lease =
        match create_spawn_owner_lease(&project_root, &owner_plan, &preparation_authority) {
            Ok(owner) => owner,
            Err(error) => {
                let worktree_cleanup = cleanup_precommit_worktree_sync_with_authority(
                    &project_root,
                    &worktree,
                    &preparation_authority,
                );
                let cleanup = worktree_cleanup
                    .as_ref()
                    .err()
                    .map(|value| format!("; worktree cleanup failed: {value}"))
                    .unwrap_or_default();
                let intent_cleanup = if !error.mutation_indeterminate && worktree_cleanup.is_ok() {
                    preparation_intent
                        .take()
                        .map(SpawnPreparationIntent::cleanup)
                        .transpose()
                        .err()
                        .map(|value| format!("; preparation intent cleanup failed: {value}"))
                        .unwrap_or_default()
                } else {
                    // Owner creation can fail after publishing either half of its
                    // exact pair. Preserve the durable intent until restart can
                    // classify and remove the planned owner.
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
            let worktree_cleanup = cleanup_precommit_worktree_sync_with_authority(
                &project_root,
                &worktree,
                &preparation_authority,
            )
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
            let mut cleanup = Vec::new();
            if let Err(cleanup_error) = cleanup_precommit_worktree_sync_with_authority(
                &project_root,
                &worktree,
                &preparation_authority,
            ) {
                cleanup.push(format!("worktree cleanup failed: {cleanup_error}"));
            }
            if let Err(cleanup_error) = compensate_owner_lease_with_authority(
                owner_lease,
                OwnerLeaseCompensation::Remove,
                &preparation_authority,
            ) {
                cleanup.push(format!("owner lease cleanup failed: {cleanup_error}"));
            }
            if let Some(intent) = preparation_intent {
                // Audit initialization reports a single error that can include
                // an indeterminate session cleanup. Preserve the intent for
                // bounded restart reconciliation instead of guessing that the
                // audit namespace is clean.
                drop(intent);
            }
            return if cleanup.is_empty() {
                Err(error)
            } else {
                Err(format!("{error}; {}", cleanup.join("; ")))
            };
        }
    };
    if let Some(intent) = preparation_intent.as_mut() {
        if intent.data.phase == SpawnPreparationPhase::ResourcesPrepared {
            if let Err(error) =
                intent.revise(SpawnPreparationPhase::AuditPublished, None, None, None)
            {
                let cleanup_errors = collect_spawn_compensation_sync_with_audit(
                    || Ok(()),
                    || {
                        cleanup_precommit_worktree_sync_with_authority(
                            &project_root,
                            &worktree,
                            &preparation_authority,
                        )
                    },
                    |action| {
                        compensate_owner_lease_with_authority(
                            owner_lease,
                            action,
                            &preparation_authority,
                        )
                    },
                    prepared_audit,
                    &preparation_authority,
                );
                // The intent revision itself is indeterminate.  Its durable
                // transaction remains the restart authority even when all
                // exact external cleanup happened to finish.
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
        let mut cleanup_errors = collect_spawn_compensation_sync_with_audit(
            || Ok(()),
            || {
                cleanup_precommit_worktree_sync_with_authority(
                    &project_root,
                    &worktree,
                    &preparation_authority,
                )
            },
            |action| {
                compensate_owner_lease_with_authority(owner_lease, action, &preparation_authority)
            },
            prepared_audit,
            &preparation_authority,
        );
        if cleanup_errors.is_empty() {
            if let Some(intent) = preparation_intent {
                if let Err(cleanup) = intent.cleanup() {
                    cleanup_errors.push(format!("preparation intent cleanup failed: {cleanup}"));
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
            message: "spawn preparation authority was lost before record publication".to_string(),
            receipt: None,
            publication_attempted: false,
        }),
    };
    let mut publication = match publication_result {
        Ok(publication) => publication,
        Err(error) => {
            let message = error.message.clone();
            let compensation_errors = collect_spawn_compensation_sync_with_audit(
                || {
                    cleanup_record_after_publication_failure_locked(
                        &project_root,
                        &record,
                        &error,
                        &preparation_authority,
                    )
                },
                || {
                    cleanup_precommit_worktree_sync_with_authority(
                        &project_root,
                        &worktree,
                        &preparation_authority,
                    )
                },
                |action| {
                    compensate_owner_lease_with_authority(
                        owner_lease,
                        action,
                        &preparation_authority,
                    )
                },
                prepared_audit,
                &preparation_authority,
            );
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
        if let Err(error) = intent.revise(SpawnPreparationPhase::RecordPublished, None, None, None)
        {
            let compensation_errors = collect_spawn_compensation_sync_with_audit(
                || {
                    cleanup_record_after_registration_failure_locked(
                        &project_root,
                        &record,
                        &publication,
                        &preparation_authority,
                    )
                },
                || {
                    cleanup_precommit_worktree_sync_with_authority(
                        &project_root,
                        &worktree,
                        &preparation_authority,
                    )
                },
                |action| {
                    compensate_owner_lease_with_authority(
                        owner_lease,
                        action,
                        &preparation_authority,
                    )
                },
                prepared_audit,
                &preparation_authority,
            );
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
            let compensation_errors = collect_spawn_compensation_sync_with_audit(
                || {
                    cleanup_record_after_registration_failure_locked(
                        &project_root,
                        &record,
                        &publication,
                        &preparation_authority,
                    )
                },
                || {
                    cleanup_precommit_worktree_sync_with_authority(
                        &project_root,
                        &worktree,
                        &preparation_authority,
                    )
                },
                |action| {
                    compensate_owner_lease_with_authority(
                        owner_lease,
                        action,
                        &preparation_authority,
                    )
                },
                prepared_audit,
                &preparation_authority,
            );
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
                compensation_errors.push(format!("task registration rollback failed: {rollback}"));
            }
            compensation_errors.extend(collect_spawn_compensation_sync_with_audit(
                || {
                    cleanup_record_after_registration_failure_locked(
                        &project_root,
                        &record,
                        &publication,
                        &preparation_authority,
                    )
                },
                || {
                    cleanup_precommit_worktree_sync_with_authority(
                        &project_root,
                        &worktree,
                        &preparation_authority,
                    )
                },
                |action| {
                    compensate_owner_lease_with_authority(
                        owner_lease,
                        action,
                        &preparation_authority,
                    )
                },
                prepared_audit,
                &preparation_authority,
            ));
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
}
