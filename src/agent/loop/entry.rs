//! Agent loop internals.

use super::*;

pub async fn run_agent_loop(
    project_root: PathBuf,
    session_id: &str,
    goal: &str,
    cfg: AgentLoopConfig,
) -> Result<AgentRunSummary, String> {
    let runtime = prepare_agent_loop_runtime(
        &project_root,
        None,
        None,
        cfg.provider.as_deref(),
        cfg.model.as_deref(),
    )?;
    runtime.nib_cfg.validate_public_session_id(session_id)?;
    let run_lease = runtime
        .session_store
        .try_acquire_run_lease(session_id)
        .map_err(|error| error.to_string())?;
    run_agent_loop_with_runtime(runtime, session_id, goal, cfg, run_lease).await
}

/// Run against the profile and session directory captured when an interactive UI
/// started. Reloaded configuration may tighten or remove that profile, but it may
/// never redirect the turn into another profile with a coincident session ID.
pub async fn run_agent_loop_for_profile(
    project_root: PathBuf,
    profile_id: &str,
    sessions_dir: &Path,
    session_id: &str,
    goal: &str,
    cfg: AgentLoopConfig,
) -> Result<AgentRunSummary, String> {
    crate::config::load_nib_config_full(&project_root)
        .map_err(|error| error.to_string())?
        .validate_public_session_id(session_id)?;
    let store = SessionStore::at_dir(sessions_dir.to_path_buf());
    let run_lease = store
        .try_acquire_run_lease(session_id)
        .map_err(|error| error.to_string())?;
    run_agent_loop_for_profile_with_lease(
        project_root,
        profile_id,
        sessions_dir,
        session_id,
        goal,
        cfg,
        run_lease,
    )
    .await
}

pub(crate) async fn run_agent_loop_for_profile_with_lease(
    project_root: PathBuf,
    profile_id: &str,
    sessions_dir: &Path,
    session_id: &str,
    goal: &str,
    cfg: AgentLoopConfig,
    run_lease: SessionRunLease,
) -> Result<AgentRunSummary, String> {
    let runtime = prepare_agent_loop_runtime(
        &project_root,
        Some(profile_id),
        Some(sessions_dir),
        cfg.provider.as_deref(),
        cfg.model.as_deref(),
    )?;
    runtime.nib_cfg.validate_public_session_id(session_id)?;
    run_agent_loop_with_runtime(runtime, session_id, goal, cfg, run_lease).await
}

pub(crate) struct AgentLoopRuntime {
    pub(crate) nib_cfg: crate::config::NibConfig,
    pub(crate) profile: crate::profile::Profile,
    pub(crate) session_store: SessionStore,
}

pub(crate) fn prepare_agent_loop_runtime(
    project_root: &Path,
    profile_id: Option<&str>,
    expected_sessions_dir: Option<&Path>,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<AgentLoopRuntime, String> {
    let mut nib_cfg =
        crate::config::load_nib_config_full(project_root).map_err(|error| error.to_string())?;
    apply_model_override(&mut nib_cfg, provider, model)?;
    let profiles = crate::profile::ProfileRegistry::load(project_root, &nib_cfg.profiles)
        .map_err(|error| error.to_string())?;
    let profile = match profile_id {
        Some(profile_id) => profiles
            .get(profile_id)
            .ok_or_else(|| format!("agent profile no longer exists: {profile_id}"))?,
        None => profiles
            .for_workspace(project_root)
            .unwrap_or_else(|| profiles.default_profile()),
    }
    .clone();
    profile
        .ensure_state_dirs()
        .map_err(|error| error.to_string())?;
    let session_store = SessionStore::at_dir(profile.sessions_dir().to_path_buf());
    if let Some(expected_sessions_dir) = expected_sessions_dir {
        let expected_sessions_dir = expected_sessions_dir
            .canonicalize()
            .map_err(|error| format!("failed to resolve agent session scope: {error}"))?;
        if !crate::fs_security::canonical_paths_match(
            session_store.sessions_dir(),
            &expected_sessions_dir,
        ) {
            return Err(format!(
                "agent profile session scope changed: expected {}, got {}",
                expected_sessions_dir.display(),
                session_store.sessions_dir().display()
            ));
        }
    }
    Ok(AgentLoopRuntime {
        nib_cfg,
        profile,
        session_store,
    })
}

pub(crate) async fn run_agent_loop_with_runtime(
    runtime: AgentLoopRuntime,
    session_id: &str,
    goal: &str,
    cfg: AgentLoopConfig,
    run_lease: SessionRunLease,
) -> Result<AgentRunSummary, String> {
    run_agent_loop_with_runtime_and_recovery(
        runtime,
        session_id,
        goal,
        cfg,
        run_lease,
        reconcile_interrupted_provider_continuation,
    )
    .await
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) async fn run_agent_loop_with_runtime_and_recovery(
    runtime: AgentLoopRuntime,
    session_id: &str,
    goal: &str,
    mut cfg: AgentLoopConfig,
    run_lease: SessionRunLease,
    recover_interrupted_continuation: fn(&SessionStore, &str) -> Result<bool, String>,
) -> Result<AgentRunSummary, String> {
    let sessions_dir = runtime.session_store.sessions_dir().to_path_buf();
    run_lease
        .verify_for(session_id, &sessions_dir)
        .map_err(|error| error.to_string())?;
    let run_id = resolve_agent_run_id(cfg.run_id.clone())?;
    if runtime
        .session_store
        .load_result(session_id)
        .map_err(|error| error.to_string())?
        .is_none()
    {
        runtime
            .session_store
            .try_create_session_with_id(session_id.to_string())
            .map_err(|error| error.to_string())?;
    }
    let resources = AgentResourceTracker::from_session(
        &runtime
            .session_store
            .load_result(session_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "session disappeared before run resource setup".to_string())?,
    );
    let compaction_request = cfg.mode == "compact";
    runtime
        .session_store
        .update_session(session_id, |session| {
            if session.events.iter().any(|event| {
                event.kind == "run_started"
                    && event.details.get("run_id").and_then(Value::as_str) == Some(&run_id)
            }) {
                return Err(crate::session::SessionError::InvalidMutation(
                    "duplicate or replayed run_id".to_string(),
                ));
            }
            append_session_event(session, "run_started", json!({"run_id": run_id.clone()}));
            // Plans interrupted before T081 cleared them at termination are
            // cleared now, so they cannot trap this request.
            if !compaction_request {
                clear_legacy_interrupted_plan(session);
            }
            Ok(())
        })
        .map_err(|error| error.to_string())?;
    let unsupported_steering_mode = cfg.steering.is_some()
        && !matches!(cfg.mode.as_str(), "execute" | "plan")
        && cfg.steering.take().is_some();
    let steering_binding = if unsupported_steering_mode {
        Err(format!(
            "exact-run steering is not supported for agent mode: {}",
            cfg.mode
        ))
    } else {
        cfg.steering.as_ref().map_or(Ok(()), |steering| {
            bind_exact_run_steering_receiver(&runtime.session_store, session_id, &run_id, steering)
        })
    };
    // Explicit compression is a local maintenance operation, not a chat turn. It
    // must not reconcile an older provider continuation because that recovery may
    // append an assistant boundary. The next ordinary run remains responsible for
    // that pre-existing continuation.
    let recovery_result = match steering_binding {
        Err(error) => Err(error),
        Ok(()) if cfg.mode == "compact" => Ok(false),
        Ok(()) => recover_interrupted_continuation(&runtime.session_store, session_id),
    }
    .and_then(|recovered| {
        if let Some(plan_id) = cfg.continuation_plan_id.as_deref() {
            if let Some(invocation_id) = cfg.discussion_invocation_id {
                validate_discussion_admission(
                    &runtime.session_store,
                    session_id,
                    plan_id,
                    goal,
                    &run_id,
                    invocation_id,
                )?;
            } else {
                validate_continue_admission(
                    &runtime.session_store,
                    session_id,
                    plan_id,
                    goal,
                    &run_id,
                )?;
            }
        }
        if cfg.discussion_invocation_id.is_some() && cfg.continuation_plan_id.is_none() {
            return Err("discussion continuation requires its exact persisted plan".to_string());
        }
        Ok(recovered)
    });
    cfg.run_id = Some(run_id.clone());
    let explicit_compaction = cfg.mode == "compact";
    let cancellation = cfg.cancellation.clone();
    let stream_tx = cfg.stream_tx.clone();
    let stream_sensitive_values = runtime.nib_cfg.public_session_sensitive_values();
    let cancellation_store = runtime.session_store.clone();
    // Requests rejected at admission never ran, so they interrupt no plan.
    let admitted = recovery_result.is_ok();
    let run_result = match recovery_result {
        Err(error) => Err(error),
        Ok(_) => {
            if let Some(cancellation) = cancellation {
                if cancellation.is_cancelled() {
                    reconcile_cancelled_run(&cancellation_store, session_id, &stream_tx).await
                } else {
                    let mut running = Box::pin(run_agent_operation(
                        runtime,
                        session_id,
                        goal,
                        cfg,
                        resources.clone(),
                    ));
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => {
                            if explicit_compaction
                                && explicit_compaction_terminal_committed(
                                    &cancellation_store,
                                    session_id,
                                    &run_id,
                                )?
                            {
                                running.await
                            } else {
                                drop(running);
                                reconcile_cancelled_run(&cancellation_store, session_id, &stream_tx).await
                            }
                        },
                        result = &mut running => result,
                    }
                }
            } else {
                Box::pin(run_agent_operation(
                    runtime,
                    session_id,
                    goal,
                    cfg,
                    resources.clone(),
                ))
                .await
            }
        }
    };
    let result = match (run_result, run_lease.verify_for(session_id, &sessions_dir)) {
        (Ok(mut summary), Ok(())) => {
            summary.run_id = run_id.clone();
            let outcome = summary.outcome.clone();
            persist_resource_evidence(
                &cancellation_store,
                session_id,
                &run_id,
                &outcome,
                &resources,
            )?;
            runtime_terminal_event(&cancellation_store, session_id, &run_id, &outcome)?;
            if !explicit_compaction {
                clear_after_run_logged(&cancellation_store, session_id, &run_id, &outcome);
            }
            Ok(summary)
        }
        (Err(error), Ok(())) => {
            persist_resource_evidence(
                &cancellation_store,
                session_id,
                &run_id,
                "local_error",
                &resources,
            )?;
            runtime_terminal_event(&cancellation_store, session_id, &run_id, "local_error")?;
            if admitted && !explicit_compaction {
                clear_after_run_logged(&cancellation_store, session_id, &run_id, "local_error");
            }
            Err(error)
        }
        (Ok(_), Err(error)) => Err(error.to_string()),
        (Err(run_error), Err(lease_error)) => Err(format!(
            "{run_error}; active run lease verification failed: {lease_error}"
        )),
    };
    if let Ok(summary) = &result {
        if !explicit_compaction && summary.steps_taken == 0 {
            if let Ok(Some(session)) = cancellation_store.load_result(session_id) {
                if let Some(plan) = session.plan.as_ref().filter(|plan| plan.steps.len() > 1) {
                    emit_progress_nonblocking(
                        &stream_tx,
                        StreamEvent::PlanProgress(crate::interactive::plan_progress_from_plan(
                            plan,
                            &stream_sensitive_values,
                        )),
                    );
                }
            }
        }
        if let Some(failure) = &summary.failure {
            emit_terminal_bounded(
                &stream_tx,
                StreamEvent::Failure {
                    failure: failure.clone(),
                    session_id: Some(summary.session_id.clone()),
                },
            )
            .await;
        }
        emit_terminal_bounded(&stream_tx, StreamEvent::End(summary.outcome.clone())).await;
    }
    result
}

/// Clears an interrupted plan after the terminal record is written. A failure
/// here must not discard the run's own result, so it is logged and audited.
fn clear_after_run_logged(store: &SessionStore, session_id: &str, run_id: &str, outcome: &str) {
    if let Err(error) = clear_interrupted_plan_after_run(store, session_id, run_id, outcome) {
        tracing::warn!(session_id, run_id, %error, "interrupted plan was not cleared");
        let _ = store.record_event(
            session_id,
            "plan_clear_failed",
            json!({"run_id": run_id, "outcome": outcome}),
        );
    }
}

pub(crate) async fn emit_terminal_bounded(
    stream_tx: &Option<Sender<StreamEvent>>,
    event: StreamEvent,
) {
    if let Some(sender) = stream_tx {
        match tokio::time::timeout(std::time::Duration::from_millis(250), sender.reserve()).await {
            Ok(Ok(permit)) => {
                permit.send(event);
            }
            Err(_) => {
                let sender = sender.clone();
                tokio::spawn(async move {
                    let _ = sender.send(event).await;
                });
            }
            Ok(Err(_)) => {}
        }
    }
}

pub(crate) fn explicit_compaction_terminal_committed(
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
) -> Result<bool, String> {
    let session = store
        .load_result(session_id)
        .map_err(|error| format!("failed to inspect explicit compression terminal: {error}"))?
        .ok_or_else(|| "session disappeared while inspecting explicit compression".to_string())?;
    Ok(session.events.iter().any(|event| {
        event.kind == "compression_request_terminal" && event.details["run_id"] == run_id
    }))
}

pub(crate) async fn run_agent_operation(
    runtime: AgentLoopRuntime,
    session_id: &str,
    goal: &str,
    cfg: AgentLoopConfig,
    resources: AgentResourceTracker,
) -> Result<AgentRunSummary, String> {
    if cfg.mode == "compact" {
        Box::pin(run_explicit_compaction(runtime, session_id, cfg, resources)).await
    } else {
        Box::pin(run_agent_loop_inner(
            runtime, session_id, goal, cfg, resources,
        ))
        .await
    }
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) async fn run_explicit_compaction(
    runtime: AgentLoopRuntime,
    session_id: &str,
    cfg: AgentLoopConfig,
    resources: AgentResourceTracker,
) -> Result<AgentRunSummary, String> {
    let AgentLoopRuntime {
        nib_cfg,
        session_store: store,
        ..
    } = runtime;
    let run_id = cfg
        .run_id
        .as_deref()
        .ok_or_else(|| "agent run identity was not initialized".to_string())?;
    store
        .record_event(
            session_id,
            "compression_requested",
            json!({"run_id": run_id, "source": "interactive"}),
        )
        .map_err(|error| error.to_string())?;
    emit(
        &cfg.stream_tx,
        StreamEvent::StateTransition {
            state: AgentState::Compression.as_str().to_string(),
        },
    )
    .await;

    if !nib_cfg.compression.enabled {
        return finish_explicit_compaction(
            &store,
            session_id,
            run_id,
            &cfg.stream_tx,
            "compression_disabled",
            0,
        );
    }
    let session = store
        .load_result(session_id)
        .map_err(|error| format!("failed to load session for explicit compression: {error}"))?
        .ok_or_else(|| "session disappeared before explicit compression".to_string())?;
    if session.summary_index >= session.messages.len() {
        return finish_explicit_compaction(
            &store,
            session_id,
            run_id,
            &cfg.stream_tx,
            "context_unchanged",
            0,
        );
    }

    let sensitive_values = nib_cfg.sensitive_values();
    let untracked_llm: Arc<dyn LlmClient> =
        match crate::llm::factory::create_client_with_sensitive_values(
            &nib_cfg.llm,
            cfg.provider.as_deref(),
            &sensitive_values,
        ) {
            Ok(llm) => llm,
            Err(error) => {
                let failure = llm_configuration_failure(
                    &nib_cfg,
                    cfg.provider.as_deref(),
                    &error,
                    &sensitive_values,
                );
                return reconcile_explicit_compression_failure(
                    &store,
                    session_id,
                    &cfg.stream_tx,
                    failure,
                    "configuration_failed",
                );
            }
        };
    let llm: Arc<dyn LlmClient> = Arc::new(ResourceTrackingLlm {
        inner: untracked_llm,
        resources: resources.clone(),
    });
    let generation_requests_before = resources.generation_requests();
    match crate::context::compression::explicitly_compress_session(
        &store, session_id, &llm, &nib_cfg,
    )
    .await
    {
        Ok(Some(report)) => {
            resources.record_compression_requests_since(generation_requests_before);
            emit_nonblocking(
                &cfg.stream_tx,
                StreamEvent::Compression {
                    before_tokens: report.before_tokens,
                    after_tokens: report.after_tokens,
                    summarized_through: report.summarized_through,
                },
            );
            finish_explicit_compaction(
                &store,
                session_id,
                run_id,
                &cfg.stream_tx,
                "context_compacted",
                1,
            )
        }
        Ok(None) => {
            resources.record_compression_requests_since(generation_requests_before);
            finish_explicit_compaction(
                &store,
                session_id,
                run_id,
                &cfg.stream_tx,
                "context_unchanged",
                0,
            )
        }
        Err(error) => {
            resources.record_compression_requests_since(generation_requests_before);
            let failure = redact_provider_failure(&nib_cfg, error);
            reconcile_explicit_compression_failure(
                &store,
                session_id,
                &cfg.stream_tx,
                failure,
                "compression_failed",
            )
        }
    }
}

pub(crate) fn finish_explicit_compaction(
    store: &SessionStore,
    session_id: &str,
    run_id: &str,
    stream_tx: &Option<Sender<StreamEvent>>,
    outcome: &str,
    steps_taken: u32,
) -> Result<AgentRunSummary, String> {
    let tool_call_count = store
        .update_session(session_id, |session| {
            append_session_event(
                session,
                "compression_request_terminal",
                json!({"run_id": run_id, "outcome": outcome}),
            );
            Ok(session.tool_calls.len())
        })
        .map_err(|error| format!("failed to reconcile explicit compression: {error}"))?;
    emit_nonblocking(
        stream_tx,
        StreamEvent::Reconciled {
            outcome: outcome.to_string(),
        },
    );
    emit_nonblocking(
        stream_tx,
        StreamEvent::StateTransition {
            state: AgentState::Done.as_str().to_string(),
        },
    );
    Ok(AgentRunSummary {
        session_id: session_id.to_string(),
        run_id: run_id.to_string(),
        steps_taken,
        last_message: None,
        tool_call_count,
        final_state: AgentState::Done,
        outcome: outcome.to_string(),
        failure: None,
        bound_reached: false,
        trace: vec![
            AgentState::Compression.as_str().to_string(),
            AgentState::Done.as_str().to_string(),
        ],
    })
}

pub(crate) fn reconcile_explicit_compression_failure(
    store: &SessionStore,
    session_id: &str,
    stream_tx: &Option<Sender<StreamEvent>>,
    failure: LlmError,
    outcome: &str,
) -> Result<AgentRunSummary, String> {
    let persisted_failure = failure.clone();
    let tool_call_count = store
        .update_session(session_id, |session| {
            append_session_event(
                session,
                "reconciliation",
                json!({
                    "outcome": outcome,
                    "continue": false,
                    "failure": persisted_failure,
                }),
            );
            Ok(session.tool_calls.len())
        })
        .map_err(|error| format!("failed to reconcile explicit compression failure: {error}"))?;
    emit_nonblocking(
        stream_tx,
        StreamEvent::Reconciled {
            outcome: outcome.to_string(),
        },
    );
    emit_nonblocking(
        stream_tx,
        StreamEvent::StateTransition {
            state: AgentState::Done.as_str().to_string(),
        },
    );
    Ok(AgentRunSummary {
        session_id: session_id.to_string(),
        run_id: String::new(),
        steps_taken: 0,
        last_message: None,
        tool_call_count,
        final_state: AgentState::Done,
        outcome: outcome.to_string(),
        failure: Some(failure),
        bound_reached: false,
        trace: vec![
            AgentState::Compression.as_str().to_string(),
            AgentState::Done.as_str().to_string(),
        ],
    })
}

pub(crate) fn resolve_agent_run_id(run_id: Option<String>) -> Result<String, String> {
    let run_id = run_id.unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    if run_id.len() != 32
        || !run_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err("agent run_id must be exactly 32 lowercase hexadecimal characters".to_string());
    }
    Ok(run_id)
}

pub(crate) fn request_scope_for_run(
    session_id: &str,
    run_id: &str,
) -> Result<LlmRequestScope, String> {
    LlmRequestScope::new(session_id, run_id)
}
