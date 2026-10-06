//! T043 split.

use super::*;

struct PlainAgentAbortGuard(tokio::task::AbortHandle);

impl PlainAgentAbortGuard {
    fn new<T>(task: &tokio::task::JoinHandle<T>) -> Self {
        Self(task.abort_handle())
    }
}

impl Drop for PlainAgentAbortGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn plain_agent_join_result(
    result: Result<Result<nib::agent::AgentRunSummary, String>, tokio::task::JoinError>,
) -> Result<nib::agent::AgentRunSummary, String> {
    result.unwrap_or_else(|error| {
        Err(if error.is_cancelled() {
            "plain agent runtime worker was cancelled".to_string()
        } else {
            "plain agent runtime worker panicked".to_string()
        })
    })
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn execute_prepared_agent_step(
    mut prepared: PreparedPlainAgentStep,
    scope: &PlainAgentScope<'_>,
    session_id: &str,
    goal: &str,
    input: &ConsoleInput,
    modal_state: PlainModalState,
) -> Result<PlainAgentDisposition, String> {
    let cancellation = prepared.cancellation.clone();
    let steering = prepared.steering.take();
    let mut approval_rx = prepared
        .approval_rx
        .take()
        .ok_or_else(|| "prepared plain agent has no approval channel".to_string())?;
    let mut question_rx = prepared
        .question_rx
        .take()
        .ok_or_else(|| "prepared plain agent has no question channel".to_string())?;
    let loop_cfg = prepared
        .loop_cfg
        .take()
        .ok_or_else(|| "prepared plain agent has no runtime configuration".to_string())?;

    let _signal_registration = scope.signal_owner.register(cancellation.clone());
    if steering.is_some() {
        println!("Run active: Enter queues; use steer: <text> for the exact active run.");
    } else {
        println!("Maintenance active: exact-run steering is unavailable; Enter queues.");
    }
    let worker_project = scope.project.to_path_buf();
    let worker_profile = scope.profile_id.to_string();
    let worker_sessions = scope.session_store.sessions_dir().to_path_buf();
    let worker_session = session_id.to_string();
    let worker_goal = goal.to_string();
    let (first_poll_tx, first_poll_rx) = tokio::sync::oneshot::channel();
    // Construct and poll the agent on the configured runtime worker, keeping
    // the caller's small main stack limited to input routing and joining.
    let mut agent = prepared.runtime.spawn(async move {
        let mut running = Box::pin(nib::agent::run_agent_loop_for_profile(
            worker_project,
            &worker_profile,
            &worker_sessions,
            &worker_session,
            &worker_goal,
            loop_cfg,
        ));
        let mut first_poll_tx = Some(first_poll_tx);
        std::future::poll_fn(|context| {
            let result = std::future::Future::poll(running.as_mut(), context);
            if let Some(sender) = first_poll_tx.take() {
                let _ = sender.send(result.is_ready());
            }
            result
        })
        .await
    });
    // This owner drops before prepared's renderer join if routing unwinds.
    let _agent_owner = PlainAgentAbortGuard::new(&agent);
    let (result, quit_requested) = prepared.runtime.block_on(async {
        // Match the old biased inline poll: route input only after a Pending
        // first poll. Ready or a panicked first poll must join before any input.
        if !matches!(first_poll_rx.await, Ok(false)) {
            let result = plain_agent_join_result((&mut agent).await);
            modal_state.clear();
            return (result, false);
        }
        let mut pending_approval: Option<PlainApprovalPrompt> = None;
        let mut pending_question: Option<PlainQuestionPrompt> = None;
        let mut pending_modal_response: Option<PendingPlainModalResponse> = None;
        let mut buffered_modal_line: Option<String> = None;
        let mut input_open = true;
        let mut quit_requested = false;
        let mut awaiting_command_reason = false;

        loop {
            tokio::select! {
                biased;
                result = &mut agent => {
                    let result = plain_agent_join_result(result);
                    if let Some(prompt) = pending_approval.take() {
                        let _ = prompt.reply.send(nib::tools::models::ApprovalDecision::denied());
                    }
                    if let Some(prompt) = pending_question.take() {
                        let _ = prompt.reply.send(nib::interactive::QuestionFormOutcome::Cancelled);
                    }
                    if let Some(response) = pending_modal_response.take() {
                        response.fail_closed();
                    }
                    modal_state.clear();
                    break (result, quit_requested);
                }
                Some(prompt) = approval_rx.recv(), if pending_approval.is_none() => {
                    if input_open {
                        if prompt.context.shown_command.is_some() {
                            let environment = if prompt.context.command_environment.is_empty() {
                                "local"
                            } else {
                                prompt.context.command_environment.as_str()
                            };
                            let card = nib::interaction_card::command_approval_card(
                                environment,
                                &prompt.context.reason,
                                prompt.context.shown_command.as_deref().unwrap_or_default(),
                                &prompt.context.command_extras,
                                prompt.context.remember_exact.as_deref(),
                                0,
                                false,
                            );
                            eprintln!("\n{}", card.text);
                            if let Some(error) = &prompt.context.input_error {
                                eprintln!("{error}");
                            }
                            eprint!("> ");
                        } else {
                            eprintln!("\nApproval required\n{}", prompt.context.render());
                            eprint!("Approve? [y/N]: ");
                        }
                        let _ = io::stderr().flush();
                        pending_approval = Some(prompt);
                        if let Some(line) = buffered_modal_line.take() {
                            if let Some(prompt) = pending_approval.as_ref() {
                                match complete_plain_approval_line(&line, &prompt.context) {
                                    Ok(Some(decision)) => {
                                        let prompt = pending_approval.take().expect("approval");
                                        pending_modal_response = Some(PendingPlainModalResponse::Approval {
                                            decision,
                                            reply: prompt.reply,
                                        });
                                        request_plain_modal_frame_delimiter();
                                    }
                                    Ok(None) => {}
                                    Err(message) if message == "REASON" => {
                                        awaiting_command_reason = true;
                                        eprint!("Reason to record: ");
                                        let _ = io::stderr().flush();
                                    }
                                    Err(message) => eprintln!("{message}"),
                                }
                            }
                        }
                    } else {
                        modal_state.clear();
                        let _ = prompt.reply.send(nib::tools::models::ApprovalDecision::denied());
                    }
                }
                Some(prompt) = question_rx.recv(), if pending_question.is_none() => {
                    if input_open {
                        pending_question = Some(prompt);
                        if let Some(prompt) = pending_question.as_ref() {
                            print!("{}", prompt.form.render(&prompt.sensitive_values));
                            let _ = io::stdout().flush();
                        }
                        if let Some(line) = buffered_modal_line.take() {
                            if line.trim_start().starts_with(":command") {
                                println!("buffered command was not applied; enter it after the prompt is shown");
                            } else if let Some(outcome) = pending_question.as_mut().and_then(|prompt| prompt.form.submit_line(&line)) {
                                let prompt = pending_question.take().expect("question");
                                pending_modal_response = complete_plain_question_prompt(prompt, outcome);
                            } else if let Some(prompt) = pending_question.as_ref() {
                                print!("{}", prompt.form.render(&prompt.sensitive_values));
                                let _ = io::stdout().flush();
                            }
                        }
                    } else {
                        modal_state.clear();
                        let _ = prompt.reply.send(nib::interactive::QuestionFormOutcome::InputClosed);
                    }
                }
                line = input.read_line_async(), if input_open && buffered_modal_line.is_none() => {
                    let line = match line {
                        Ok(line) => line,
                        Err(_) => {
                            input_open = false;
                            if let Some(response) = pending_modal_response.take() {
                                response.fail_closed();
                                modal_state.clear();
                            }
                            if let Some(prompt) = pending_approval.take() {
                                let _ = prompt.reply.send(nib::tools::models::ApprovalDecision::denied_input_closed());
                            }
                            if let Some(prompt) = pending_question.take() {
                                let _ = prompt.reply.send(nib::interactive::QuestionFormOutcome::InputClosed);
                            }
                            continue;
                        }
                    };
                    if pending_modal_response.is_some() {
                        if line.trim().is_empty() {
                            let response = pending_modal_response
                                .take()
                                .expect("checked pending modal response");
                            modal_state.clear();
                            response.deliver();
                        } else if line.trim() == "esc" && matches!(pending_modal_response, Some(PendingPlainModalResponse::Question { .. })) {
                            if let Some(PendingPlainModalResponse::Question { reply, .. }) = pending_modal_response.take() {
                                let _ = reply.send(nib::interactive::QuestionFormOutcome::LeftUnanswered);
                            }
                        } else {
                            println!(
                                "[input rejected] surplus modal line was not applied; press Enter on an empty line to return input ownership"
                            );
                        }
                        continue;
                    }
                    if pending_approval.is_some() {
                        if awaiting_command_reason {
                            let reason = line.trim();
                            if reason.len() > 240 {
                                eprintln!("Input error: the reason is too long");
                                eprint!("Reason to record: ");
                                let _ = io::stderr().flush();
                                continue;
                            }
                            awaiting_command_reason = false;
                            let decision = if reason.is_empty() {
                                nib::tools::models::ApprovalDecision::denied()
                            } else {
                                nib::tools::models::ApprovalDecision::denied_with_reason(
                                    reason.to_string(),
                                )
                            };
                            let prompt = pending_approval.take().expect("approval");
                            pending_modal_response = Some(PendingPlainModalResponse::Approval {
                                decision,
                                reply: prompt.reply,
                            });
                            request_plain_modal_frame_delimiter();
                            continue;
                        }
                        let context = pending_approval
                            .as_ref()
                            .expect("approval")
                            .context
                            .clone();
                        match complete_plain_approval_line(&line, &context) {
                            Ok(Some(decision)) => {
                                let prompt = pending_approval.take().expect("approval");
                                pending_modal_response = Some(PendingPlainModalResponse::Approval {
                                    decision,
                                    reply: prompt.reply,
                                });
                                request_plain_modal_frame_delimiter();
                            }
                            Ok(None) => {}
                            Err(message) if message == "REASON" => {
                                awaiting_command_reason = true;
                                eprint!("Reason to record: ");
                                let _ = io::stderr().flush();
                            }
                            Err(message) => eprintln!("{message}"),
                        }
                        continue;
                    }
                    if pending_question.is_some() {
                        if inspect_plain_question_command(&line, scope, session_id, "running") {
                            // Inspection keeps the same question responder and drafts.
                        } else if let Some(outcome) = pending_question.as_mut().and_then(|prompt| prompt.form.submit_line(&line)) {
                            let prompt = pending_question.take().expect("question");
                            pending_modal_response = complete_plain_question_prompt(prompt, outcome);
                            continue;
                        }
                        if let Some(prompt) = pending_question.as_ref() {
                            print!("{}", prompt.form.render(&prompt.sensitive_values));
                            let _ = io::stdout().flush();
                        }
                        continue;
                    }
                    if modal_state.is_pending() {
                        buffered_modal_line = Some(line);
                        continue;
                    }

                    let state = InteractionState {
                        run: InteractionRunState::Running,
                        ..InteractionState::default()
                    };
                    match reduce_interaction(&state, InteractionInput::SubmittedLine(&line)) {
                        InteractionReduction::SteerCurrent(text) => match submit_plain_steering(
                            steering.as_ref(),
                            &text,
                        ) {
                            Ok(sequence) => println!(
                                "steering accepted for the next safe boundary (#{sequence})"
                            ),
                            Err(error) => println!("steering was not accepted: {error}"),
                        },
                        InteractionReduction::QueueNext(text) => {
                            match persist_queued_follow_up(
                                scope.session_store,
                                session_id,
                                &text,
                                "plain_active_run",
                            ) {
                                Ok(_) => println!("queued follow-up retained on session {session_id}"),
                                Err(error) => println!("{error}"),
                            }
                        }
                        InteractionReduction::Command(nib::interactive::InteractiveCommand::Quit) => {
                            quit_requested = true;
                            cancellation.cancel();
                            println!("quit requested; reconciling the active run first");
                        }
                        InteractionReduction::Command(command) => {
                            match execute_interactive_command_in_state(
                                command,
                                scope.project,
                                scope.profile_id,
                                scope.session_store,
                                session_id,
                                "running",
                            ) {
                                Ok(InteractiveEffect::Output(output)) => println!("{output}"),
                                Ok(_) => println!("command completed"),
                                Err(error) => println!("{error}"),
                            }
                        }
                        InteractionReduction::Error { message, .. } => println!("{message}"),
                        InteractionReduction::NoOp(_) => {}
                        _ => println!("input was not applicable while the run was active"),
                    }
                }
            }
        }
    });

    prepared
        .renderer
        .take()
        .ok_or_else(|| "prepared plain agent has no stream renderer".to_string())?
        .join()
        .map_err(|_| "plain stream renderer panicked".to_string())?;

    result.and_then(|summary| {
        let terminal = match reduce_interaction(
            &InteractionState::default(),
            InteractionInput::ReconciledOutcome {
                outcome: &summary.outcome,
                failure: summary.is_failure(),
            },
        ) {
            InteractionReduction::Reconciled { terminal, .. } => terminal,
            _ => unreachable!("reconciliation input always has a terminal reduction"),
        };
        if quit_requested {
            Ok(PlainAgentDisposition::QuitRequested(terminal))
        } else if terminal == InteractionTerminalOutcome::WaitingForInput {
            Err(format!(
                "question input was unavailable; session {session_id} was reconciled without continuing"
            ))
        } else if terminal == InteractionTerminalOutcome::Cancelled {
            Ok(PlainAgentDisposition::Cancelled)
        } else if terminal == InteractionTerminalOutcome::Failed && summary.failure.is_some() {
            // The agent emits its structured failure on the lifecycle stream after
            // reconciliation, so plain mode has already rendered the single safe report.
            Ok(PlainAgentDisposition::Failed)
        } else if terminal == InteractionTerminalOutcome::Failed {
            Err(summary.user_failure_report().unwrap_or_else(|| {
                nib::interactive::user_visible_stop_report(&summary.outcome, session_id)
            }))
        } else {
            Ok(PlainAgentDisposition::Completed)
        }
    })
}

fn complete_plain_question_prompt(
    prompt: PlainQuestionPrompt,
    outcome: nib::interactive::QuestionFormOutcome,
) -> Option<PendingPlainModalResponse> {
    if outcome.is_success() {
        request_plain_modal_frame_delimiter();
        Some(PendingPlainModalResponse::Question {
            outcome,
            reply: prompt.reply,
        })
    } else {
        // Interrupts reach the worker immediately, even while stdin remains open.
        let _ = prompt.reply.send(outcome);
        None
    }
}

pub(crate) fn execute_plain_continuation(
    scope: &PlainAgentScope<'_>,
    session_id: &str,
    goal: &str,
    plan_id: &str,
    input: &ConsoleInput,
    modal_state: PlainModalState,
) -> Result<PlainAgentDisposition, String> {
    let mut prepared = PreparedPlainAgentStep::prepare(
        scope,
        session_id,
        InteractiveAgentMode::Execute,
        modal_state.clone(),
    )?;
    if let Some(cfg) = prepared.loop_cfg.as_mut() {
        cfg.continuation_plan_id = Some(plan_id.to_string());
    }
    execute_prepared_agent_step(prepared, scope, session_id, goal, input, modal_state)
}

pub(crate) fn execute_plain_question_recovery(
    scope: &PlainAgentScope<'_>,
    session_id: &str,
    mut effect: nib::interactive::QuestionRecoveryEffect,
    input: &ConsoleInput,
    modal_state: PlainModalState,
) -> Result<PlainAgentDisposition, String> {
    use nib::interactive::QuestionRecoveryEffect;
    let sensitive_values = nib::config::load_nib_config_full(scope.project)
        .map_err(|error| error.to_string())?
        .public_session_sensitive_values();
    loop {
        match effect {
            QuestionRecoveryEffect::Output(output) => {
                println!(
                    "{}",
                    nib::interactive::bounded_public_text(
                        &output,
                        &sensitive_values,
                        64 * 1024,
                        true
                    )
                );
                return Ok(PlainAgentDisposition::Completed);
            }
            QuestionRecoveryEffect::OpenForm(persisted) => {
                effect = complete_plain_recovery_form(
                    scope,
                    session_id,
                    persisted,
                    None,
                    input,
                    &sensitive_values,
                )?;
            }
            QuestionRecoveryEffect::OpenEditor {
                form,
                question_index,
            } => {
                effect = complete_plain_recovery_form(
                    scope,
                    session_id,
                    form,
                    Some(question_index),
                    input,
                    &sensitive_values,
                )?;
            }
            QuestionRecoveryEffect::ContinuePlan { plan_id, goal } => {
                return execute_plain_continuation(
                    scope,
                    session_id,
                    &goal,
                    &plan_id,
                    input,
                    modal_state,
                );
            }
            QuestionRecoveryEffect::ContinueDiscussion {
                plan_id,
                goal,
                invocation_id,
            } => {
                let mut prepared = PreparedPlainAgentStep::prepare(
                    scope,
                    session_id,
                    InteractiveAgentMode::Execute,
                    modal_state.clone(),
                )?;
                if let Some(cfg) = prepared.loop_cfg.as_mut() {
                    cfg.continuation_plan_id = Some(plan_id);
                    cfg.discussion_invocation_id = Some(invocation_id);
                }
                return execute_prepared_agent_step(
                    prepared,
                    scope,
                    session_id,
                    &goal,
                    input,
                    modal_state,
                );
            }
        }
    }
}

fn complete_plain_recovery_form(
    scope: &PlainAgentScope<'_>,
    session_id: &str,
    persisted: nib::session::PersistedQuestionForm,
    editor: Option<Option<usize>>,
    input: &ConsoleInput,
    sensitive_values: &[String],
) -> Result<nib::interactive::QuestionRecoveryEffect, String> {
    let mut form = crate::console::LineQuestionForm::new(persisted.form, persisted.initial_answers);
    if let Some(index) = editor {
        form.enter_editor(index)?;
    }
    let outcome = loop {
        print!("{}", form.render(sensitive_values));
        io::stdout().flush().map_err(|error| error.to_string())?;
        let line = match input.read_line_blocking() {
            Ok(line) => line,
            Err(_) => break nib::interactive::QuestionFormOutcome::InputClosed,
        };
        if inspect_plain_question_command(&line, scope, session_id, "waiting_for_user_input") {
            continue;
        }
        if let Some(outcome) = form.submit_line(&line) {
            break outcome;
        }
    };
    let outcome = frame_plain_recovery_outcome(input, outcome);
    nib::interactive::complete_question_recovery(
        scope.session_store,
        session_id,
        persisted.invocation_id,
        outcome,
    )
}

fn inspect_plain_question_command(
    line: &str,
    scope: &PlainAgentScope<'_>,
    session_id: &str,
    state: &str,
) -> bool {
    if !line.trim_start().starts_with(":command") {
        return false;
    }
    match parse_plain_question_answer(line, &[]) {
        InteractionReduction::ModalCommand(command) => {
            match execute_interactive_command_in_state(
                command,
                scope.project,
                scope.profile_id,
                scope.session_store,
                session_id,
                state,
            ) {
                Ok(InteractiveEffect::Output(output)) => println!("{output}"),
                Ok(_) => println!("command completed without changing the pending question"),
                Err(error) => println!("{error}"),
            }
        }
        InteractionReduction::Error { message, .. } => println!("{message}"),
        _ => println!("invalid prompt-local command"),
    }
    true
}

pub(crate) fn frame_plain_recovery_outcome(
    input: &ConsoleInput,
    outcome: nib::interactive::QuestionFormOutcome,
) -> nib::interactive::QuestionFormOutcome {
    use nib::interactive::QuestionFormOutcome;
    if !outcome.is_success() {
        return outcome;
    }
    request_plain_modal_frame_delimiter();
    loop {
        match input.read_line_blocking() {
            Ok(line) if line.trim().is_empty() => return outcome,
            Ok(line) if line.trim() == "esc" => return QuestionFormOutcome::LeftUnanswered,
            Ok(_) => println!("[input rejected] surplus modal line was not applied; press Enter on an empty line to return input ownership"),
            Err(_) => return QuestionFormOutcome::InputClosed,
        }
    }
}

pub(crate) fn execute_plain_turn_and_queued_follow_ups(
    scope: &PlainAgentScope<'_>,
    session_id: &str,
    goal: &str,
    mode: InteractiveAgentMode,
    input: &ConsoleInput,
    modal_state: PlainModalState,
) -> Result<PlainAgentDisposition, String> {
    match execute_agent_step_with_modal_state(
        scope,
        session_id,
        goal,
        mode,
        input,
        modal_state.clone(),
    )? {
        PlainAgentDisposition::Completed => {
            drain_plain_queued_follow_ups(scope, session_id, input, modal_state)
        }
        disposition => Ok(disposition),
    }
}

pub(crate) fn drain_plain_queued_follow_ups(
    scope: &PlainAgentScope<'_>,
    session_id: &str,
    input: &ConsoleInput,
    modal_state: PlainModalState,
) -> Result<PlainAgentDisposition, String> {
    loop {
        let Some((queued, prepared)) =
            claim_next_queued_follow_up_after_startup(scope.session_store, session_id, |_| {
                PreparedPlainAgentStep::prepare(
                    scope,
                    session_id,
                    InteractiveAgentMode::Execute,
                    modal_state.clone(),
                )
            })?
        else {
            return Ok(PlainAgentDisposition::Completed);
        };
        println!("Starting queued follow-up.");
        match execute_prepared_agent_step(
            prepared,
            scope,
            session_id,
            &queued.text,
            input,
            modal_state.clone(),
        )? {
            PlainAgentDisposition::Completed => {}
            disposition => return Ok(disposition),
        }
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;

    #[test]
    fn router_unwind_aborts_pending_worker_and_closes_its_stream_without_continuing() {
        struct PendingResources {
            _stream: tokio::sync::mpsc::Sender<()>,
            dropped: std::sync::mpsc::Sender<()>,
        }
        impl Drop for PendingResources {
            fn drop(&mut self) {
                let _ = self.dropped.send(());
            }
        }
        let runtime = nib::agent::build_agent_runtime("test runtime").unwrap();
        let modal = PlainModalState::default();
        let pending_modal = modal.clone();
        let forbidden = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let continued = forbidden.clone();
        let (stream, mut events) = tokio::sync::mpsc::channel(1);
        let (dropped, destroyed) = std::sync::mpsc::channel();
        let (ready, waiting) = std::sync::mpsc::channel();
        let (release, resume) = tokio::sync::oneshot::channel::<()>();
        let mut task = runtime.spawn(async move {
            let mut pending = Box::pin(async move {
                let resources = PendingResources {
                    _stream: stream,
                    dropped,
                };
                assert!(pending_modal.claim(PLAIN_MODAL_QUESTION));
                if resume.await.is_ok() {
                    continued.store(true, Ordering::SeqCst);
                }
                drop(resources);
            });
            let mut ready = Some(ready);
            std::future::poll_fn(|context| {
                let result = std::future::Future::poll(pending.as_mut(), context);
                if let Some(sender) = ready.take() {
                    sender.send(result.is_pending()).unwrap();
                }
                result
            })
            .await
        });
        assert!(waiting
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap());
        assert!(modal.is_pending());
        let unwind = std::panic::catch_unwind(|| {
            let _owner = PlainAgentAbortGuard::new(&task);
            panic!("plain router unwind");
        });
        assert!(unwind.is_err());
        // A detached worker would accept this continuation and run forbidden work.
        let _ = release.send(());
        let joined = runtime
            .block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(5), &mut task).await
            })
            .expect("worker teardown deadline");
        assert!(joined.unwrap_err().is_cancelled());
        destroyed
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("worker resources dropped");
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));
        assert!(!forbidden.load(Ordering::SeqCst));
    }
}
