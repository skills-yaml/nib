//! TUI internals split for T043 module size.

use super::*;

pub(crate) struct TuiAgentWorker {
    pub(crate) run_id: String,
    pub(crate) mode: InteractiveAgentMode,
    pub(crate) cancellation: CancellationSignal,
    pub(crate) steering: Option<crate::agent::ExactRunSteeringHandle>,
    pub(crate) handle: Option<JoinHandle<()>>,
}

#[derive(Clone)]
pub(crate) struct TuiAgentProfileScope {
    pub(crate) project_root: std::path::PathBuf,
    pub(crate) profile_id: String,
    pub(crate) sessions_dir: std::path::PathBuf,
}

pub(crate) type TuiAgentStart = (
    String,
    InteractiveAgentMode,
    String,
    Option<crate::agent::ExactRunSteeringReceiver>,
    Option<String>,
    Option<crate::tools::ToolInvocationId>,
);

pub(crate) struct PreparedTuiAgentWorker {
    pub(crate) cancellation: Option<CancellationSignal>,
    pub(crate) start_tx: Option<mpsc::SyncSender<TuiAgentStart>>,
    pub(crate) session_store: SessionStore,
    pub(crate) session_id: String,
    pub(crate) handle: Option<JoinHandle<()>>,
}

#[derive(Debug)]
pub(crate) struct SessionStreamEvent {
    pub(crate) session_id: String,
    pub(crate) run_id: String,
    pub(crate) event: StreamEvent,
}

impl TuiAgentWorker {
    pub(crate) fn request_cancellation(&self) {
        self.cancellation.cancel();
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub(crate) fn submit_steering(&self, text: &str) -> Result<usize, String> {
        self.steering
            .as_ref()
            .ok_or_else(|| "this active operation does not accept exact-run steering".to_string())?
            .submit(text)
    }

    pub(crate) fn fail_unaccounted_steering(&self, reason: &str) -> Result<(), String> {
        self.steering
            .as_ref()
            .map_or(Ok(()), |steering| steering.fail_unaccounted(reason))
    }

    pub(crate) fn join(&mut self) -> io::Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        handle
            .join()
            .map_err(|_| io::Error::other("TUI agent worker panicked"))
    }
}

pub(crate) fn submit_tui_steering_draft(
    worker: Option<&TuiAgentWorker>,
    composer: &mut Composer,
) -> Result<(String, usize), String> {
    let reduction = reduce_interaction(
        &InteractionState {
            run: tui_run_state(worker.is_some(), None, None),
            ..InteractionState::default()
        },
        InteractionInput::SteerCurrent(&composer.input),
    );
    let text = match reduction {
        InteractionReduction::SteerCurrent(text) => text,
        InteractionReduction::Error { message, .. } => return Err(message),
        _ => return Err("steering input had no valid consumer".to_string()),
    };
    let sequence = worker
        .ok_or_else(|| "exact active run is unavailable".to_string())?
        .submit_steering(&text)?;
    composer.remember_submission(&text);
    composer.set_text(String::new());
    Ok((text, sequence))
}

impl PreparedTuiAgentWorker {
    pub(crate) fn start(
        self,
        goal: String,
        mode: InteractiveAgentMode,
    ) -> io::Result<TuiAgentWorker> {
        self.start_with_continuation(goal, mode, None)
    }

    pub(crate) fn start_with_continuation(
        self,
        goal: String,
        mode: InteractiveAgentMode,
        continuation_plan_id: Option<String>,
    ) -> io::Result<TuiAgentWorker> {
        self.start_with_recovery(goal, mode, continuation_plan_id, None)
    }

    pub(crate) fn start_with_recovery(
        mut self,
        goal: String,
        mode: InteractiveAgentMode,
        continuation_plan_id: Option<String>,
        discussion_invocation_id: Option<crate::tools::ToolInvocationId>,
    ) -> io::Result<TuiAgentWorker> {
        let start_tx = self
            .start_tx
            .take()
            .ok_or_else(|| io::Error::other("prepared TUI worker has no start channel"))?;
        let run_id = uuid::Uuid::new_v4().simple().to_string();
        let (steering, steering_receiver) = if mode == InteractiveAgentMode::Compact {
            (None, None)
        } else {
            let (steering, receiver) = crate::agent::exact_run_steering_channel(
                self.session_store.clone(),
                self.session_id.clone(),
                run_id.clone(),
                "tui",
            )
            .map_err(io::Error::other)?;
            (Some(steering), Some(receiver))
        };
        start_tx
            .send((
                goal,
                mode,
                run_id.clone(),
                steering_receiver,
                continuation_plan_id,
                discussion_invocation_id,
            ))
            .map_err(|_| io::Error::other("prepared TUI worker stopped before activation"))?;
        Ok(TuiAgentWorker {
            run_id,
            mode,
            cancellation: self.cancellation.take().ok_or_else(|| {
                io::Error::other("prepared TUI worker has no cancellation signal")
            })?,
            steering,
            handle: self.handle.take(),
        })
    }
}

impl Drop for PreparedTuiAgentWorker {
    fn drop(&mut self) {
        self.start_tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn prepare_tui_agent_worker(
    profile_scope: TuiAgentProfileScope,
    session_id: String,
    approval_tx: mpsc::Sender<TuiApprovalRequest>,
    question_tx: mpsc::Sender<TuiQuestionRequest>,
    stream_tx: tokio::sync::mpsc::Sender<SessionStreamEvent>,
) -> io::Result<PreparedTuiAgentWorker> {
    let TuiAgentProfileScope {
        project_root,
        profile_id,
        sessions_dir,
    } = profile_scope;
    let cancellation = CancellationSignal::new();
    let run_cancellation = cancellation.clone();
    let session_store = SessionStore::at_dir(sessions_dir.clone());
    let prepared_session_id = session_id.clone();
    let (start_tx, start_rx) = mpsc::sync_channel::<TuiAgentStart>(0);
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), String>>(0);
    let handle = std::thread::Builder::new()
        .name("nib-tui-agent".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = ready_tx.send(Err(format!(
                        "failed to initialize the async runtime: {error}"
                    )));
                    return;
                }
            };
            if ready_tx.send(Ok(())).is_err() {
                return;
            }
            let Ok((goal, mode, run_id, steering, continuation_plan_id, discussion_invocation_id)) =
                start_rx.recv()
            else {
                return;
            };

            runtime.block_on(async move {
                let (agent_stream_tx, mut agent_stream_rx) =
                    tokio::sync::mpsc::channel::<StreamEvent>(100);
                let forwarding_session_id = session_id.clone();
                let forwarding_run_id = run_id.clone();
                let forwarding_tx = stream_tx.clone();
                let forwarder = tokio::spawn(async move {
                    while let Some(event) = agent_stream_rx.recv().await {
                        if forwarding_tx
                            .send(SessionStreamEvent {
                                session_id: forwarding_session_id.clone(),
                                run_id: forwarding_run_id.clone(),
                                event,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
                let loop_cfg = crate::agent::AgentLoopConfig {
                    max_steps: 0,
                    mode: mode.as_str().to_string(),
                    interactive_request: mode == InteractiveAgentMode::Execute,
                    approval_handler: Some(std::sync::Arc::new(TuiApprovalHandler {
                        tx: approval_tx,
                    })),
                    question_handler: Some(std::sync::Arc::new(TuiQuestionHandler {
                        tx: question_tx,
                    })),
                    stream_tx: Some(agent_stream_tx.clone()),
                    cancellation: Some(run_cancellation),
                    run_id: Some(run_id),
                    steering,
                    continuation_plan_id,
                    discussion_invocation_id,
                    ..Default::default()
                };

                let run_result = crate::agent::run_agent_loop_for_profile(
                    project_root,
                    &profile_id,
                    &sessions_dir,
                    &session_id,
                    &goal,
                    loop_cfg,
                )
                .await;
                match run_result {
                    Ok(summary) => {
                        let _ = agent_stream_tx
                            .send(StreamEvent::End(summary.outcome))
                            .await;
                    }
                    Err(error) => {
                        let _ = agent_stream_tx
                            .send(safe_agent_error_stream_event(&error))
                            .await;
                    }
                }
                drop(agent_stream_tx);
                let _ = forwarder.await;
            });
        })?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(PreparedTuiAgentWorker {
            cancellation: Some(cancellation),
            start_tx: Some(start_tx),
            session_store,
            session_id: prepared_session_id,
            handle: Some(handle),
        }),
        Ok(Err(error)) => {
            let _ = handle.join();
            Err(io::Error::other(error))
        }
        Err(_) => {
            let _ = handle.join();
            Err(io::Error::other(
                "TUI agent worker stopped before reporting startup readiness",
            ))
        }
    }
}

pub(crate) fn safe_agent_error_stream_event(_error: &str) -> StreamEvent {
    StreamEvent::End("local_error".to_string())
}

pub(crate) fn assign_session_title_from_goal(
    store: &SessionStore,
    session_id: &str,
    goal: &str,
    chrome_generation: &mut u64,
) {
    if let Ok(Some(_)) = maybe_assign_session_display_name(store, session_id, goal) {
        *chrome_generation = chrome_generation.saturating_add(1);
    }
}

pub(crate) fn spawn_tui_agent_worker(
    profile_scope: TuiAgentProfileScope,
    session_id: String,
    goal: String,
    mode: InteractiveAgentMode,
    approval_tx: mpsc::Sender<TuiApprovalRequest>,
    question_tx: mpsc::Sender<TuiQuestionRequest>,
    stream_tx: tokio::sync::mpsc::Sender<SessionStreamEvent>,
) -> io::Result<TuiAgentWorker> {
    prepare_tui_agent_worker(
        profile_scope,
        session_id,
        approval_tx,
        question_tx,
        stream_tx,
    )?
    .start(goal, mode)
}

pub(crate) fn cancel_pending_interactions(
    pending_approval: &mut Option<TuiApprovalRequest>,
    pending_question: &mut Option<PendingQuestion>,
    approval_rx: &mpsc::Receiver<TuiApprovalRequest>,
    question_rx: &mpsc::Receiver<TuiQuestionRequest>,
) {
    if let Some(request) = pending_approval.take() {
        let _ = request.reply.send(ApprovalDecision::denied());
    }
    if let Some(question) = pending_question.take() {
        let _ = question
            .request
            .reply
            .send(crate::interactive::QuestionFormOutcome::Cancelled);
    }
    while let Ok(request) = approval_rx.try_recv() {
        let _ = request.reply.send(ApprovalDecision::denied());
    }
    while let Ok(request) = question_rx.try_recv() {
        let _ = request
            .reply
            .send(crate::interactive::QuestionFormOutcome::Cancelled);
    }
}

pub(crate) fn refresh_pending_interactions(
    pending_approval: &mut Option<TuiApprovalRequest>,
    pending_question: &mut Option<PendingQuestion>,
    approval_rx: &mpsc::Receiver<TuiApprovalRequest>,
    question_rx: &mpsc::Receiver<TuiQuestionRequest>,
) {
    if pending_approval.is_none() {
        if let Ok(request) = approval_rx.try_recv() {
            *pending_approval = Some(request);
        }
    }
    if pending_question.is_none() {
        if let Ok(request) = question_rx.try_recv() {
            *pending_question = Some(PendingQuestion::new(request));
        }
    }
}

pub(crate) fn drain_stream_events(
    stream_rx: &mut tokio::sync::mpsc::Receiver<SessionStreamEvent>,
    timeline: &mut ActiveTimeline,
) {
    let _ = drain_stream_events_bounded(stream_rx, timeline, usize::MAX);
}

pub(crate) fn drain_stream_events_bounded(
    stream_rx: &mut tokio::sync::mpsc::Receiver<SessionStreamEvent>,
    timeline: &mut ActiveTimeline,
    limit: usize,
) -> usize {
    let mut drained = 0;
    while drained < limit {
        let Ok(event) = stream_rx.try_recv() else {
            break;
        };
        drained += 1;
        let reduction = reduce_interaction(
            &InteractionState::default(),
            InteractionInput::SessionRunEvent {
                active_session_id: &timeline.session_id,
                active_run_id: timeline.active_run_id.as_deref(),
                event_session_id: &event.session_id,
                event_run_id: &event.run_id,
            },
        );
        if reduction == InteractionReduction::Consumed(InteractionConsumer::Timeline) {
            timeline.apply_event(event.event);
        }
    }
    drained
}

pub(crate) fn shutdown_agent_worker(
    worker: &mut Option<TuiAgentWorker>,
    pending_approval: &mut Option<TuiApprovalRequest>,
    pending_question: &mut Option<PendingQuestion>,
    approval_rx: &mpsc::Receiver<TuiApprovalRequest>,
    question_rx: &mpsc::Receiver<TuiQuestionRequest>,
    stream_rx: &mut tokio::sync::mpsc::Receiver<SessionStreamEvent>,
    timeline: &mut ActiveTimeline,
) -> io::Result<()> {
    shutdown_agent_worker_with_timeout(
        worker,
        pending_approval,
        pending_question,
        approval_rx,
        question_rx,
        stream_rx,
        timeline,
        AGENT_SHUTDOWN_TIMEOUT,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn shutdown_agent_worker_with_timeout(
    worker: &mut Option<TuiAgentWorker>,
    pending_approval: &mut Option<TuiApprovalRequest>,
    pending_question: &mut Option<PendingQuestion>,
    approval_rx: &mpsc::Receiver<TuiApprovalRequest>,
    question_rx: &mpsc::Receiver<TuiQuestionRequest>,
    stream_rx: &mut tokio::sync::mpsc::Receiver<SessionStreamEvent>,
    timeline: &mut ActiveTimeline,
    shutdown_timeout: std::time::Duration,
) -> io::Result<()> {
    let Some(active_worker) = worker.as_ref() else {
        cancel_pending_interactions(pending_approval, pending_question, approval_rx, question_rx);
        timeline.active_run_id = None;
        drain_stream_events(stream_rx, timeline);
        return Ok(());
    };
    active_worker.request_cancellation();
    // Approval and question handlers are explicit worker dependencies. Resolve them
    // before waiting so cancellation cannot deadlock behind a modal response.
    cancel_pending_interactions(pending_approval, pending_question, approval_rx, question_rx);
    let shutdown_started = std::time::Instant::now();
    while worker.as_ref().is_some_and(|worker| !worker.is_finished()) {
        drain_stream_events(stream_rx, timeline);
        if shutdown_started.elapsed() >= shutdown_timeout {
            // Rust threads cannot be killed safely. Drop the join handle and fail the
            // TUI closed so its outer restoration guard can restore the terminal and
            // process shutdown can terminate the unresponsive worker.
            let compensation = worker
                .as_ref()
                .expect("active worker exists while shutting down")
                .fail_unaccounted_steering("unresponsive_worker_shutdown");
            *worker = None;
            timeline.active_run_id = None;
            drain_stream_events(stream_rx, timeline);
            timeline.apply_event(StreamEvent::End("unresponsive_worker_shutdown".to_string()));
            if let Err(error) = compensation {
                return Err(io::Error::other(format!(
                    "TUI agent worker did not stop within the cancellation deadline; failed to reconcile accepted steering: {error}"
                )));
            }
            let message =
                crate::interactive::terminal_outcome_message("unresponsive_worker_shutdown");
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{}. {}", message.title, message.detail),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    if let Some(active_worker) = worker.as_mut() {
        active_worker.join()?;
    }
    // The worker can publish a modal request after the early cleanup while
    // cancellation is settling. Joining closes that producer before the final drain.
    cancel_pending_interactions(pending_approval, pending_question, approval_rx, question_rx);
    drain_stream_events(stream_rx, timeline);
    timeline.active_run_id = None;
    *worker = None;
    Ok(())
}

pub(crate) fn reap_finished_worker(
    worker: &mut Option<TuiAgentWorker>,
    pending_approval: &mut Option<TuiApprovalRequest>,
    pending_question: &mut Option<PendingQuestion>,
    approval_rx: &mpsc::Receiver<TuiApprovalRequest>,
    question_rx: &mpsc::Receiver<TuiQuestionRequest>,
    stream_rx: &mut tokio::sync::mpsc::Receiver<SessionStreamEvent>,
    timeline: &mut ActiveTimeline,
) -> io::Result<()> {
    if worker.as_ref().is_some_and(TuiAgentWorker::is_finished) {
        cancel_pending_interactions(pending_approval, pending_question, approval_rx, question_rx);
        if let Some(worker) = worker.as_mut() {
            worker.join()?;
        }
        *worker = None;
        drain_stream_events(stream_rx, timeline);
        timeline.active_run_id = None;
    }
    Ok(())
}
