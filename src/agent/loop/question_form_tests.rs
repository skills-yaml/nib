use super::*;
use crate::interactive::{
    parse_question_editor_input, parse_question_form, parse_question_line,
    parse_question_submit_line, public_question_form, QuestionAnswer, QuestionAnswerSource,
    QuestionEditorInput, QuestionForm, QuestionFormOutcome, QuestionLineInput, QuestionSubmitInput,
};

fn form(value: Value) -> QuestionForm {
    parse_question_form(&value).expect("question form")
}
fn text(value: &str) -> QuestionAnswer {
    QuestionAnswer {
        answer: value.to_string(),
        source: QuestionAnswerSource::Text,
    }
}
fn option(value: &str) -> QuestionAnswer {
    QuestionAnswer {
        answer: value.to_string(),
        source: QuestionAnswerSource::Option,
    }
}
fn form_store() -> (tempfile::TempDir, SessionStore, String, String) {
    let dir = tempdir().expect("form store");
    let store = SessionStore::at_dir(dir.path().join("sessions"));
    let mut session = store.create_session_with_id("form-session");
    let mut plan = pending_plan("finish work", "finish the approved step");
    plan.approve();
    let plan_id = plan.id.clone();
    session.plan = Some(plan);
    store.save(&mut session).expect("plan");
    (dir, store, session.id, plan_id)
}
fn register(
    store: &SessionStore,
    id: &str,
    plan: &str,
    run: &str,
    form: &QuestionForm,
    discussion: Option<ToolInvocationId>,
) -> (ToolInvocationId, Vec<Option<QuestionAnswer>>) {
    let invocation = ToolInvocationId::new();
    let initial = prepare_question_form(
        QuestionFormBinding {
            store,
            session_id: id,
            plan_id: Some(plan),
            run_id: run,
            invocation_id: invocation,
            dependent_paths: &["src".to_string()],
            discussion_invocation_id: discussion,
        },
        form,
    )
    .expect("register form");
    (invocation, initial)
}
fn apply(
    store: &SessionStore,
    id: &str,
    invocation: ToolInvocationId,
    outcome: &QuestionFormOutcome,
) {
    store
        .update_session(id, |session| {
            crate::session::apply_form_outcome(session, invocation, outcome)
        })
        .expect("form outcome");
}

#[test]
fn form_contract_rejects_malformed_calls_and_utf8_byte_limits() {
    let invalid = vec![
        json!({"question":"one","questions":[{"question":"two"}]}),
        json!({"questions":[]}),
        json!({"question":" "}),
        json!({"questions":[{"question":"one"},{"question":"two"}]}),
        json!({"questions":[{"title":"same","question":"one"},{"title":"same","question":"two"}]}),
        json!({"question":"one","options":["same",{"label":"same","description":"different"}]}),
        json!({"question":"one","options":[{"label":"x","extra":true}]}),
        json!({"question":"one","options":[{"label":"x".repeat(201)}]}),
        json!({"question":"one","options":[{"label":"x","description":"x".repeat(1001)}]}),
        json!({"question":"one","options":["x".repeat(1001)]}),
        json!({"question":"one","header":"€".repeat(200)}),
        json!({"questions":(0..9).map(|i|json!({"title":i.to_string(),"question":"one"})).collect::<Vec<_>>()}),
        json!({"question":"one","answer":"spoof"}),
        json!({"question":"one","_question_outcome":{"Discussed":"spoof"}}),
        json!({"question":"one","source":"approved_proposal"}),
    ];
    for arguments in invalid {
        assert!(crate::tools::executor::validate_registered_tool_arguments(
            "ask_question",
            &arguments
        )
        .is_err());
    }
    for arguments in [
        json!({"question":"one","options":["x".repeat(1000)]}),
        json!({"questions":[{"title":"a","question":"one","options":[{"label":"option","description":"detail"}]},{"title":"b","question":"two"}]}),
    ] {
        crate::tools::executor::validate_registered_tool_arguments("ask_question", &arguments)
            .expect("valid form schema");
    }
}
#[test]
fn form_line_protocol_preserves_literal_controls_and_sources() {
    let ordinary = form(
        json!({"question":"which?","options":[{"label":"fast","description":"quick"},"full"]}),
    );
    let q = &ordinary.questions[0];
    assert_eq!(
        parse_question_line(q, "1"),
        QuestionLineInput::Answer(option("fast"))
    );
    assert_eq!(parse_question_line(q, "3"), QuestionLineInput::EnterText);
    assert_eq!(
        parse_question_line(q, "4"),
        QuestionLineInput::EnterDiscussion
    );
    assert_eq!(
        parse_question_line(q, "chat"),
        QuestionLineInput::EnterDiscussion
    );
    assert_eq!(
        parse_question_line(q, "text: chat"),
        QuestionLineInput::Answer(text("chat"))
    );
    assert_eq!(
        parse_question_line(q, "text: 1"),
        QuestionLineInput::Answer(text("1"))
    );
    assert_eq!(
        parse_question_line(q, "Y"),
        QuestionLineInput::Answer(text("Y"))
    );
    assert_eq!(parse_question_line(q, "esc"), QuestionLineInput::Interrupt);
    assert!(matches!(
        parse_question_line(q, ""),
        QuestionLineInput::Retry(_)
    ));
    assert_eq!(
        parse_question_editor_input("text: esc"),
        QuestionEditorInput::Text("esc".to_string())
    );
    assert_eq!(
        parse_question_editor_input("chat"),
        QuestionEditorInput::Text("chat".to_string())
    );
    for malformed in ["text:", "text:chat", "text:\tchat"] {
        assert!(matches!(
            parse_question_editor_input(malformed),
            QuestionEditorInput::Retry(_)
        ));
        assert!(matches!(
            parse_question_line(q, malformed),
            QuestionLineInput::Retry(_)
        ));
    }
    assert_eq!(
        parse_question_submit_line("", 2),
        QuestionSubmitInput::Submit
    );
    assert_eq!(
        parse_question_submit_line("2", 2),
        QuestionSubmitInput::Reopen(1)
    );
    let proposed =
        form(json!({"question":"which?","proposed_answer":"exact","options":["hidden"]}));
    assert_eq!(
        parse_question_line(&proposed.questions[0], "1"),
        QuestionLineInput::Answer(QuestionAnswer {
            answer: "exact".to_string(),
            source: QuestionAnswerSource::ApprovedProposal
        })
    );
    assert_eq!(
        parse_question_line(&proposed.questions[0], "2"),
        QuestionLineInput::Reject
    );
    assert_eq!(
        parse_question_line(&proposed.questions[0], "4"),
        QuestionLineInput::EnterDiscussion
    );
}
#[test]
fn displayed_form_collision_fails_without_exposing_sensitive_values() {
    let secrets = vec!["private-a".to_string(), "private-b".to_string()];
    let labels = form(json!({"question":"choose","options":["private-a","private-b"]}));
    let error = public_question_form(&labels, &secrets).expect_err("collapsed labels rejected");
    assert!(!error.contains("private-"));
    let titles = form(
        json!({"questions":[{"title":"private-a","question":"one"},{"title":"private-b","question":"two"}]}),
    );
    assert!(public_question_form(&titles, &secrets).is_err());
    let raw = form(
        json!({"header":"private-a\nheading","question":"which?","options":[{"label":"choice","description":"private-b\u{001b}[31m"}]}),
    );
    let safe = public_question_form(&raw, &secrets).expect("safe form");
    let encoded = serde_json::to_string(&safe).unwrap();
    assert!(!encoded.contains("private-"));
    assert!(!safe.header.as_ref().unwrap().chars().any(char::is_control));
}
#[tokio::test]
async fn exact_reuse_normalizes_string_options_and_reasks_changed_descriptions() {
    let (_dir, store, id, plan) = form_store();
    let original = form(json!({"question":"which?","options":["fast"]}));
    let (first, initial) = register(&store, &id, &plan, "run-a", &original, None);
    assert_eq!(initial, [None]);
    apply(
        &store,
        &id,
        first,
        &QuestionFormOutcome::Answered(vec![option("fast")]),
    );
    let normalized =
        form(json!({"questions":[{"question":"which?","options":[{"label":"fast"}]}]}));
    let (reused, initial) = register(&store, &id, &plan, "run-b", &normalized, None);
    assert_eq!(initial, [Some(option("fast"))]);
    let outcome = request_form_answer(
        &AgentLoopConfig::default(),
        reused,
        &normalized,
        &initial,
        &[],
    )
    .await;
    assert_eq!(outcome, QuestionFormOutcome::Answered(vec![option("fast")]));
    let changed = form(
        json!({"question":"which?","options":[{"label":"fast","description":"new description"}]}),
    );
    let (_, initial) = register(&store, &id, &plan, "run-c", &changed, None);
    assert_eq!(initial, [None]);
    let set = form(
        json!({"questions":[{"title":"choice","question":"which?","options":["fast"]},{"title":"target","question":"where?"}]}),
    );
    let (_, initial) = register(&store, &id, &plan, "run-d", &set, None);
    assert_eq!(initial, [Some(option("fast")), None]);
}
#[test]
fn discussion_retains_dependencies_and_exact_revisions_resolve_only_linked_origins() {
    let (_dir, store, id, plan) = form_store();
    let original = form(json!({"questions":[{"title":"target","question":"which target?"}]}));
    let (first, _) = register(&store, &id, &plan, "run-a", &original, None);
    apply(
        &store,
        &id,
        first,
        &QuestionFormOutcome::Discussed("explain options".to_string()),
    );
    let session = store.load(&id).unwrap();
    assert!(session.has_unresolved_clarification(Some(&plan)));
    let dependent = vec![ToolCallRequest::new(
        "read_file",
        json!({"path":"src/lib.rs"}),
    )];
    assert_eq!(
        unresolved_clarification_dependencies(&session, Some(&plan), Path::new("."), &dependent)
            .unwrap(),
        [first]
    );
    let disjoint = vec![ToolCallRequest::new(
        "read_file",
        json!({"path":"README.md"}),
    )];
    assert!(unresolved_clarification_dependencies(
        &session,
        Some(&plan),
        Path::new("."),
        &disjoint
    )
    .unwrap()
    .is_empty());
    let revised =
        form(json!({"questions":[{"title":"target","question":"which revised target?"}]}));
    let (second, _) = register(&store, &id, &plan, "run-a", &revised, None);
    let pending = crate::session::pending_question_forms(&store.load(&id).unwrap());
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].invocation_id, second);
    apply(
        &store,
        &id,
        second,
        &QuestionFormOutcome::Answered(vec![text("src/lib.rs")]),
    );
    let persisted = store.load(&id).unwrap();
    assert!(!persisted.has_unresolved_clarification(Some(&plan)));
    assert_eq!(
        persisted.clarifications[0].answer.as_deref(),
        Some("src/lib.rs")
    );
    assert_eq!(
        persisted.clarifications[1].origin_invocation_id,
        Some(first)
    );
}
#[test]
fn unrelated_untitled_question_in_the_same_run_never_clears_discussed_blocker() {
    let (_dir, store, id, plan) = form_store();
    let original = form(json!({"question":"which file?"}));
    let (first, _) = register(&store, &id, &plan, "run-a", &original, None);
    apply(
        &store,
        &id,
        first,
        &QuestionFormOutcome::Discussed("discuss".to_string()),
    );
    let unrelated = form(json!({"question":"which other file?"}));
    let (second, _) = register(&store, &id, &plan, "run-a", &unrelated, None);
    apply(
        &store,
        &id,
        second,
        &QuestionFormOutcome::Answered(vec![text("elsewhere")]),
    );
    let reloaded = store.load(&id).unwrap();
    assert_eq!(
        reloaded.clarifications[0].status,
        ClarificationStatus::Discussed
    );
    assert!(reloaded.has_unresolved_clarification(Some(&plan)));
}
#[test]
fn recovered_set_requires_atomic_submit_and_preserves_provenance_across_reload() {
    let (_dir, store, id, plan) = form_store();
    let set = form(
        json!({"header":"two answers","questions":[{"title":"target","question":"where?"},{"title":"mode","question":"how?","proposed_answer":"carefully"}]}),
    );
    let (invocation, _) = register(&store, &id, &plan, "run-a", &set, None);
    apply(
        &store,
        &id,
        invocation,
        &QuestionFormOutcome::LeftUnanswered,
    );
    runtime_terminal_event(&store, &id, "run-a", "waiting_for_user_input").unwrap();
    assert!(crate::session::persist_recovered_form_answer(
        &store,
        &id,
        invocation,
        0,
        &text("src/lib.rs")
    )
    .is_err());
    assert!(crate::session::persist_recovered_form_answers(
        &store,
        &id,
        invocation,
        &[text("src/lib.rs")]
    )
    .is_err());
    let wrong = vec![
        text("src/lib.rs"),
        QuestionAnswer {
            answer: "different".to_string(),
            source: QuestionAnswerSource::ApprovedProposal,
        },
    ];
    assert!(
        crate::session::persist_recovered_form_answers(&store, &id, invocation, &wrong).is_err()
    );
    let pending = crate::session::pending_question_forms(&store.load(&id).unwrap());
    assert_eq!(pending[0].initial_answers, [None, None]);
    assert_eq!(pending[0].run_id.as_deref(), Some("run-a"));
    let answers = vec![
        text("src/lib.rs"),
        QuestionAnswer {
            answer: "carefully".to_string(),
            source: QuestionAnswerSource::ApprovedProposal,
        },
    ];
    store
        .update_session(&id, |session| {
            session.clarifications[1].plan_id = Some("different-plan".to_string());
            Ok(())
        })
        .expect("malformed mixed-operation storage fixture");
    assert!(
        crate::session::persist_recovered_form_answers(&store, &id, invocation, &answers).is_err()
    );
    assert!(store
        .load(&id)
        .unwrap()
        .clarifications
        .iter()
        .all(|record| { record.status != ClarificationStatus::Answered }));
    store
        .update_session(&id, |session| {
            session.clarifications[1].plan_id = Some(plan.clone());
            session.clarifications.reverse();
            Ok(())
        })
        .expect("stored index identity survives record order changes");
    assert_eq!(
        crate::session::persist_recovered_form_answers(&store, &id, invocation, &answers).unwrap(),
        plan
    );
    let reloaded = store.load(&id).unwrap();
    assert!(reloaded
        .clarifications
        .iter()
        .all(|record| record.status == ClarificationStatus::Answered));
    assert!(reloaded
        .events
        .iter()
        .any(|event| event.kind == "human_question_answer_received"
            && event.details["decision"] == "approved_proposal"
            && event.details["recovered"] == true));
}
#[test]
fn recovery_rejects_cancelled_terminal_operation_and_redacts_human_input() {
    let (_dir, store, id, plan) = form_store();
    let one = form(json!({"question":"where?"}));
    let (invocation, _) = register(&store, &id, &plan, "run-a", &one, None);
    apply(
        &store,
        &id,
        invocation,
        &QuestionFormOutcome::LeftUnanswered,
    );
    runtime_terminal_event(&store, &id, "run-a", "cancelled_by_user").unwrap();
    assert!(crate::session::persist_recovered_form_answers(
        &store,
        &id,
        invocation,
        &[text("src")]
    )
    .is_err());
    let (_dir, store, id, plan) = form_store();
    let store = store.with_sensitive_values(vec!["private-answer".to_string()]);
    let (invocation, _) = register(&store, &id, &plan, "run-b", &one, None);
    runtime_terminal_event(&store, &id, "run-b", "waiting_for_user_input").unwrap();
    crate::session::persist_recovered_form_answers(
        &store,
        &id,
        invocation,
        &[text("private-answer")],
    )
    .unwrap();
    assert!(!serde_json::to_string(&store.load(&id).unwrap())
        .unwrap()
        .contains("private-answer"));
}
#[test]
fn guarded_discussion_continuation_requires_fresh_single_use_human_evidence() {
    let (_dir, store, id, plan) = form_store();
    let one = form(json!({"question":"where?"}));
    let (invocation, _) = register(&store, &id, &plan, "run-a", &one, None);
    apply(
        &store,
        &id,
        invocation,
        &QuestionFormOutcome::LeftUnanswered,
    );
    runtime_terminal_event(&store, &id, "run-a", "waiting_for_user_input").unwrap();
    assert!(
        validate_discussion_admission(&store, &id, &plan, "finish work", "run-b", invocation)
            .is_err()
    );
    crate::session::persist_recovered_form_outcome(
        &store,
        &id,
        invocation,
        &QuestionFormOutcome::Discussed("explain the tradeoffs".to_string()),
    )
    .unwrap();
    validate_discussion_admission(&store, &id, &plan, "finish work", "run-b", invocation).unwrap();
    prepare_discussion_turn(&store, &id, &plan, "finish work", "run-b", invocation).unwrap();
    assert!(
        validate_discussion_admission(&store, &id, &plan, "finish work", "run-c", invocation)
            .is_err()
    );
    assert!(store
        .load(&id)
        .unwrap()
        .has_unresolved_clarification(Some(&plan)));
    store
        .update_session(&id, |session| {
            session.plan.as_mut().unwrap().outcome =
                Some("provider_continuation_interrupted".to_string());
            Ok(())
        })
        .unwrap();
    assert!(
        validate_discussion_admission(&store, &id, &plan, "finish work", "run-b", invocation)
            .is_err()
    );
}
#[test]
fn legacy_records_load_with_optional_form_fields_and_string_option_results() {
    let record:ClarificationRecord=serde_json::from_value(json!({"invocation_id":ToolInvocationId::new(),"question":"which?","options":["fast"],"status":"unresolved","question_event_index":0})).unwrap();
    assert_eq!(record.question_index, 0);
    assert!(record.run_id.is_none());
    assert!(record.option_details.is_empty());
    let one = form(json!({"question":"which?","options":["fast"]}));
    let output = one
        .observation(&QuestionFormOutcome::Answered(vec![option("fast")]))
        .unwrap();
    assert_eq!(output["question"], "which?");
    assert_eq!(output["options"], json!(["fast"]));
    assert_eq!(output["answer"], "fast");
    assert_eq!(output["source"], "option");
    let discussed = one
        .observation(&QuestionFormOutcome::Discussed("tell me more".to_string()))
        .unwrap();
    assert_eq!(discussed["status"], "discussed");
}

#[test]
fn repeated_discussion_revisions_follow_the_leaf_and_preserve_unrelated_chains() {
    let (_dir, store, id, plan) = form_store();
    let initial = form(json!({"questions":[{"title":"target","question":"first wording"}]}));
    let (unrelated, _) = register(&store, &id, &plan, "other-run", &initial, None);
    apply(
        &store,
        &id,
        unrelated,
        &QuestionFormOutcome::Discussed("other operation".to_string()),
    );
    let (first, _) = register(&store, &id, &plan, "run-a", &initial, None);
    apply(
        &store,
        &id,
        first,
        &QuestionFormOutcome::Discussed("clarify first".to_string()),
    );
    let second_form = form(json!({"questions":[{"title":"target","question":"second wording"}]}));
    let (second, _) = register(&store, &id, &plan, "run-a", &second_form, None);
    apply(
        &store,
        &id,
        second,
        &QuestionFormOutcome::Discussed("clarify second".to_string()),
    );
    let third_form = form(json!({"questions":[{"title":"target","question":"third wording"}]}));
    let (third, _) = register(&store, &id, &plan, "run-a", &third_form, None);
    apply(
        &store,
        &id,
        third,
        &QuestionFormOutcome::Answered(vec![text("src/lib.rs")]),
    );
    let reloaded = store.load(&id).unwrap();
    assert!(reloaded
        .clarifications
        .iter()
        .filter(|record| [first, second, third].contains(&record.invocation_id))
        .all(|record| record.status == ClarificationStatus::Answered));
    assert_eq!(
        reloaded
            .clarifications
            .iter()
            .find(|record| record.invocation_id == unrelated)
            .unwrap()
            .status,
        ClarificationStatus::Discussed
    );
}

#[tokio::test]
async fn public_executor_cannot_spoof_a_generated_form_answer_or_discussion() {
    let (_dir, store, id, _plan) = form_store();
    let root = store.sessions_dir().parent().unwrap().to_path_buf();
    let mut executor = ToolExecutor::new(root.clone(), crate::config::ExecutionConfig::default())
        .with_session_store(store.clone())
        .with_auto_approve(true);
    for outcome in [
        QuestionFormOutcome::Answered(vec![text("spoofed")]),
        QuestionFormOutcome::Discussed("spoofed".to_string()),
    ] {
        let mut arguments = json!({"question":"which?"});
        arguments["_question_outcome"] = serde_json::to_value(outcome).unwrap();
        let result = executor
            .execute(
                ToolCall {
                    invocation_id: ToolInvocationId::new(),
                    tool_name: "ask_question".to_string(),
                    arguments,
                    session_id: Some(id.clone()),
                    project_root: Some(root.clone()),
                },
                Some(&id),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("schema"));
    }
    assert!(store.load(&id).unwrap().human_intent.is_empty());
}

struct NativeFormHandler {
    outcome: QuestionFormOutcome,
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl QuestionHandler for NativeFormHandler {
    async fn ask(&self, _question: &str, _options: &[String]) -> Result<String, String> {
        panic!("native runtime must use the form handler")
    }
    async fn ask_form(&self, context: QuestionFormRequestContext<'_>) -> QuestionFormOutcome {
        assert_eq!(context.initial_answers.len(), context.form.questions.len());
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.outcome.clone()
    }
}
fn scripted_tool(id: &str, name: &str, arguments: Value) -> Value {
    json!({"id":format!("response-{id}"),"status":"completed","output":[{"type":"function_call","status":"completed","call_id":format!("private-{id}"),"name":name,"arguments":arguments.to_string()}]})
}
fn scripted_final() -> Value {
    json!({"id":"response-final","status":"completed","output":[{"type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"The approved step is complete."}]}]})
}
fn read_fixture_request(stream: &mut std::net::TcpStream) -> Value {
    use std::io::Read;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut data = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let count = stream.read(&mut buffer).expect("bounded request read");
        assert!(count > 0);
        data.extend_from_slice(&buffer[..count]);
        assert!(data.len() < 2 * 1024 * 1024);
        if let Some(end) = data.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&data[..end]);
            let length = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .expect("content length");
            if data.len() >= end + 4 + length {
                return serde_json::from_slice(&data[end + 4..end + 4 + length]).unwrap();
            }
        }
    }
}
fn scripted_form_server(responses: Vec<Value>) -> (String, std::sync::mpsc::Receiver<Value>) {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        for response in responses {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                }
            };
            sender.send(read_fixture_request(&mut stream)).unwrap();
            let body = format!(
                "data: {}\n\n",
                json!({"type":"response.completed","response":response})
            );
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).unwrap();
        }
    });
    (format!("http://{address}/v1"), receiver)
}
async fn run_scripted_form(
    responses: Vec<Value>,
    outcome: QuestionFormOutcome,
) -> (AgentRunSummary, Session, Vec<Value>, usize) {
    let root = tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/lib.rs"), "content").unwrap();
    std::fs::write(root.path().join("README.md"), "reference").unwrap();
    let (url, requests) = scripted_form_server(responses);
    let mut config = NibConfig::default();
    config.execution.provider = "internal".to_string();
    config.daemons.cron_enabled = false;
    config.daemons.curator_enabled = false;
    config.llm = LlmConfig {
        active_provider: Some("openai".to_string()),
        providers: HashMap::from([(
            "openai".to_string(),
            ProviderEntry {
                model: "fixture-model".to_string(),
                api_key: Some("fixture-key".to_string()),
                base_url: Some(url),
                api: Some(crate::config::LlmApiMode::Responses),
                ..Default::default()
            },
        )]),
        context_length: 128_000,
    };
    save_nib_config_full(root.path(), &mut config).unwrap();
    let store = SessionStore::for_project(root.path()).unwrap();
    let mut session = store.create_session();
    let mut plan = pending_plan("finish work", "finish the approved step");
    plan.approve();
    session.plan = Some(plan);
    store.save(&mut session).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let summary = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        run_agent_loop(
            root.path().to_path_buf(),
            &session.id,
            "finish work",
            AgentLoopConfig {
                max_steps: 8,
                auto_approve: true,
                question_handler: Some(Arc::new(NativeFormHandler {
                    outcome,
                    calls: calls.clone(),
                })),
                ..Default::default()
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let persisted = store.load(&session.id).unwrap();
    persisted.validate_message_sequence().unwrap();
    (
        summary,
        persisted,
        requests.try_iter().collect(),
        calls.load(Ordering::SeqCst),
    )
}
#[tokio::test]
async fn native_grouped_form_flows_through_tool_execution_and_atomic_persistence() {
    let arguments = json!({"header":"choose related answers","questions":[{"title":"target","question":"which target?","options":[{"label":"src/lib.rs","description":"the implementation"}]},{"title":"mode","question":"which mode?","proposed_answer":"carefully"}]});
    let answers = vec![
        option("src/lib.rs"),
        QuestionAnswer {
            answer: "carefully".to_string(),
            source: QuestionAnswerSource::ApprovedProposal,
        },
    ];
    let (summary, session, requests, calls) = run_scripted_form(
        vec![
            scripted_tool("form", "ask_question", arguments),
            scripted_final(),
        ],
        QuestionFormOutcome::Answered(answers),
    )
    .await;
    assert_eq!(summary.outcome, "completed");
    assert_eq!(calls, 1);
    assert_eq!(requests.len(), 2);
    assert_eq!(summary.tool_call_count, 1);
    assert_eq!(session.clarifications.len(), 2);
    assert!(session
        .clarifications
        .iter()
        .all(|record| record.status == ClarificationStatus::Answered));
    let output = &session
        .tool_calls
        .iter()
        .find(|call| call.tool_name.as_deref() == Some("ask_question"))
        .unwrap()
        .result;
    assert_eq!(output.as_ref().unwrap()["output"]["status"], "answered");
    assert_eq!(
        output.as_ref().unwrap()["output"]["answers"][1]["source"],
        "approved_proposal"
    );
}
#[tokio::test]
async fn discussed_tool_success_continues_the_model_and_rejects_dependent_work() {
    let question = json!({"question":"which target?","dependent_paths":["src"]});
    let (summary, session, requests, calls) = run_scripted_form(
        vec![
            scripted_tool("form", "ask_question", question),
            scripted_tool("dependent", "read_file", json!({"path":"src/lib.rs"})),
        ],
        QuestionFormOutcome::Discussed("explain choices".to_string()),
    )
    .await;
    assert_eq!(summary.outcome, "unresolved_clarification");
    assert_eq!(requests.len(), 2);
    assert_eq!(calls, 1);
    assert_eq!(
        session.clarifications[0].status,
        ClarificationStatus::Discussed
    );
    assert!(!session.plan.as_ref().unwrap().is_complete());
    assert!(session
        .tool_calls
        .iter()
        .all(|call| call.tool_name.as_deref() != Some("read_file")));
    assert!(session
        .tool_calls
        .iter()
        .find(|call| call.tool_name.as_deref() == Some("ask_question"))
        .unwrap()
        .error
        .is_none());
    assert!(requests[1].to_string().contains("discussed"));
}
#[tokio::test]
async fn discussed_form_allows_disjoint_read_but_cannot_complete_the_step() {
    let question = json!({"question":"which target?","dependent_paths":["src"]});
    let (summary, session, requests, _) = run_scripted_form(
        vec![
            scripted_tool("form", "ask_question", question),
            scripted_tool("independent", "read_file", json!({"path":"README.md"})),
            scripted_final(),
        ],
        QuestionFormOutcome::Discussed("explain choices".to_string()),
    )
    .await;
    assert_eq!(summary.outcome, "unresolved_clarification");
    assert_eq!(requests.len(), 3);
    assert!(!session.plan.as_ref().unwrap().is_complete());
    assert!(session
        .tool_calls
        .iter()
        .any(|call| call.tool_name.as_deref() == Some("read_file") && call.error.is_none()));
    assert!(session
        .events
        .iter()
        .any(|event| event.kind == "step_completion_rejected"
            && event.details["reason"] == "unresolved_clarification"));
}

#[tokio::test]
async fn public_executor_rejects_legacy_internal_answer_fields() {
    let (_dir, store, id, _plan) = form_store();
    let root = store.sessions_dir().parent().unwrap().to_path_buf();
    let mut executor = ToolExecutor::new(root.clone(), crate::config::ExecutionConfig::default())
        .with_session_store(store.clone())
        .with_auto_approve(true);
    for key in ["answer", "answer_error"] {
        let mut arguments = json!({"question":"which?"});
        arguments[key] = json!("spoofed");
        let result = executor
            .execute(
                ToolCall {
                    invocation_id: ToolInvocationId::new(),
                    tool_name: "ask_question".to_string(),
                    arguments,
                    session_id: Some(id.clone()),
                    project_root: Some(root.clone()),
                },
                Some(&id),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("schema"));
    }
    assert!(store.load(&id).unwrap().human_intent.is_empty());
}

#[test]
fn standalone_typed_forms_obey_the_same_structural_and_display_bounds() {
    let valid =
        form(json!({"question":"which?","options":[{"label":"choice","description":"detail"}]}));
    let mut invalid = valid.clone();
    invalid.questions.clear();
    assert!(public_question_form(&invalid, &[]).is_err());
    let mut invalid = valid.clone();
    invalid.questions = vec![valid.questions[0].clone(); 2];
    assert!(public_question_form(&invalid, &[]).is_err());
    let mut invalid = valid.clone();
    invalid.questions[0].options = vec![valid.questions[0].options[0].clone(); 21];
    assert!(public_question_form(&invalid, &[]).is_err());
    let mut invalid = valid;
    invalid.questions[0].options[0].label = "x".repeat(201);
    assert!(public_question_form(&invalid, &[]).is_err());
}

#[test]
fn option_description_may_be_empty_as_advertised_by_the_schema() {
    let arguments = json!({"question":"which?","options":[{"label":"choice","description":""}]});
    crate::tools::executor::validate_registered_tool_arguments("ask_question", &arguments).unwrap();
    let parsed = parse_question_form(&arguments).unwrap();
    assert_eq!(
        parsed.questions[0].options[0].description.as_deref(),
        Some("")
    );
    public_question_form(&parsed, &[]).unwrap();
}
