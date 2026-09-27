//! T043 split.

use super::*;

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
    let (result, quit_requested) = prepared.runtime.block_on(async {
        let mut agent = Box::pin(nib::agent::run_agent_loop_for_profile(
            scope.project.to_path_buf(),
            scope.profile_id,
            scope.session_store.sessions_dir(),
            session_id,
            goal,
            loop_cfg,
        ));
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
                    if let Some(prompt) = pending_approval.take() {
                        let _ = prompt.reply.send(nib::tools::models::ApprovalDecision::denied());
                    }
                    if let Some(prompt) = pending_question.take() {
                        let _ = prompt.reply.send(nib::agent::QuestionOutcome::Cancelled);
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
                        println!("\nQuestion: {}", prompt.question);
                        if let Some(proposal) = prompt.proposed_answer.as_deref() {
                            println!("Proposed answer: {proposal}");
                            println!("1. Approve proposed answer\n2. Reject and leave unanswered\n3. Instruct otherwise");
                        }
                        if prompt.proposed_answer.is_none() {
                            for (index, option) in prompt.options.iter().enumerate() {
                                println!("  {}. {}", index + 1, option);
                            }
                        }
                        if prompt.proposed_answer.is_some() {
                            print!("Decision or answer: ");
                        } else if prompt.options.is_empty() {
                            print!("Answer: ");
                        } else {
                            print!("Answer (number or text): ");
                        }
                        let _ = io::stdout().flush();
                        pending_question = Some(prompt);
                        if let Some(line) = buffered_modal_line.take() {
                            if let Some(prompt) = pending_question.as_ref() {
                                match interpret_plain_question_line(&line, &prompt.options, prompt.proposed_answer.as_deref()) {
                                    PlainQuestionLine::Outcome(outcome) => {
                                        let prompt = pending_question.take().expect("question");
                                        pending_modal_response = Some(PendingPlainModalResponse::Question {
                                            outcome,
                                            reply: prompt.reply,
                                        });
                                        request_plain_modal_frame_delimiter();
                                    }
                                    PlainQuestionLine::Retry(message) => println!("{message}"),
                                    PlainQuestionLine::Command(_) => println!(
                                        "buffered command was not applied; enter it after the prompt is shown"
                                    ),
                                }
                            }
                        }
                    } else {
                        modal_state.clear();
                        let _ = prompt.reply.send(nib::agent::QuestionOutcome::InputClosed);
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
                                let _ = prompt.reply.send(nib::agent::QuestionOutcome::InputClosed);
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
                        let prompt = pending_question.as_ref().expect("question");
                        let options = prompt.options.clone();
                        let proposed_answer = prompt.proposed_answer.clone();
                        match interpret_plain_question_line(&line, &options, proposed_answer.as_deref()) {
                            PlainQuestionLine::Outcome(outcome) => {
                                let prompt = pending_question.take().expect("question");
                                pending_modal_response = Some(PendingPlainModalResponse::Question {
                                    outcome,
                                    reply: prompt.reply,
                                });
                                request_plain_modal_frame_delimiter();
                            }
                            PlainQuestionLine::Retry(message) => {
                                println!("{message}");
                                if proposed_answer.is_some() {
                                    print!("Decision or answer: ");
                                } else if options.is_empty() {
                                    print!("Answer: ");
                                } else {
                                    print!("Answer (number or text): ");
                                }
                                let _ = io::stdout().flush();
                            }
                            PlainQuestionLine::Command(command) => {
                                match execute_interactive_command_in_state(
                                    command,
                                    scope.project,
                                    scope.profile_id,
                                    scope.session_store,
                                    session_id,
                                    "running",
                                ) {
                                    Ok(InteractiveEffect::Output(output)) => {
                                        println!("{output}")
                                    }
                                    Ok(_) => println!(
                                        "command completed without changing the pending question"
                                    ),
                                    Err(error) => println!("{error}"),
                                }
                                print!("Decision or answer: ");
                                let _ = io::stdout().flush();
                            }
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
