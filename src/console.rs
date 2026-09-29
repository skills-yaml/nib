use nib::agent::{QuestionHandler, QuestionOutcome, QuestionRequestContext};
#[cfg(test)]
use nib::interactive::InteractionConsumer;
use nib::interactive::{
    modal_command_unsupported_message, reduce_interaction, InteractionDecision, InteractionInput,
    InteractionReduction, InteractionState,
};
use nib::tools::executor::{ApprovalContext, ApprovalHandler};
use nib::tools::models::{ApprovalDecision, PermissionLevel, ToolCall};
use std::io::{self, BufRead, Write};
use std::sync::Arc;

const MAX_CONSOLE_LINE_BYTES: usize = 16 * 1024 + 2;
const MAX_BUFFERED_CONSOLE_LINES: usize = 16;

#[derive(Clone)]
pub struct ConsoleInput {
    state: Arc<ConsoleInputState>,
}

type ConsoleLine = Result<String, String>;
type ConsoleReader = Box<dyn BufRead + Send>;

struct ConsoleInputState {
    lines: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<ConsoleLine>>,
    pending_reader:
        std::sync::Mutex<Option<(ConsoleReader, tokio::sync::mpsc::Sender<ConsoleLine>)>>,
}

impl ConsoleInput {
    pub fn stdin() -> Self {
        Self::new(io::BufReader::new(io::stdin()))
    }

    pub fn new(reader: impl BufRead + Send + 'static) -> Self {
        let (line_tx, line_rx) = tokio::sync::mpsc::channel(MAX_BUFFERED_CONSOLE_LINES);
        Self {
            state: Arc::new(ConsoleInputState {
                lines: tokio::sync::Mutex::new(line_rx),
                pending_reader: std::sync::Mutex::new(Some((Box::new(reader), line_tx))),
            }),
        }
    }

    pub fn read_line_blocking(&self) -> Result<String, String> {
        self.ensure_broker_started();
        self.state
            .lines
            .blocking_lock()
            .blocking_recv()
            .unwrap_or_else(|| {
                Err("console input closed before a response was received".to_string())
            })
    }

    pub(crate) async fn read_line_async(&self) -> Result<String, String> {
        self.ensure_broker_started();
        self.state
            .lines
            .lock()
            .await
            .recv()
            .await
            .unwrap_or_else(|| {
                Err("console input closed before a response was received".to_string())
            })
    }

    fn ensure_broker_started(&self) {
        let pending = self
            .state
            .pending_reader
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let Some((mut reader, line_tx)) = pending else {
            return;
        };
        std::thread::Builder::new()
            .name("nib-console-input".to_string())
            .spawn(move || loop {
                match read_bounded_console_line(&mut reader) {
                    Ok(None) => {
                        let _ = line_tx.blocking_send(Err(
                            "console input closed before a response was received".to_string(),
                        ));
                        break;
                    }
                    Ok(Some(line)) => {
                        if line_tx.blocking_send(Ok(line)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = line_tx.blocking_send(Err(format!(
                            "failed to read bounded console input: {error}"
                        )));
                        break;
                    }
                }
            })
            .expect("console input broker thread must start");
    }

    #[cfg(test)]
    fn broker_started(&self) -> bool {
        self.state
            .pending_reader
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_none()
    }
}

fn read_bounded_console_line(reader: &mut (impl BufRead + ?Sized)) -> io::Result<Option<String>> {
    let mut bytes = Vec::with_capacity(256);
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            return String::from_utf8(bytes).map(Some).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "console input is not valid UTF-8",
                )
            });
        }
        let chunk_len = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if bytes.len().saturating_add(chunk_len) > MAX_CONSOLE_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("console line exceeds the {MAX_CONSOLE_LINE_BYTES}-byte framing limit"),
            ));
        }
        bytes.extend_from_slice(&available[..chunk_len]);
        let complete = available[chunk_len - 1] == b'\n';
        reader.consume(chunk_len);
        if complete {
            return String::from_utf8(bytes).map(Some).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "console input is not valid UTF-8",
                )
            });
        }
    }
}

pub struct ConsoleApprovalHandler {
    input: ConsoleInput,
}

impl ConsoleApprovalHandler {
    pub fn new(input: ConsoleInput) -> Self {
        Self { input }
    }
}

#[async_trait::async_trait]
impl ApprovalHandler for ConsoleApprovalHandler {
    async fn handle_approval(&self, call: &ToolCall, level: PermissionLevel) -> ApprovalDecision {
        let context = ApprovalContext::compatibility(call, level);
        self.prompt(&context).await
    }

    async fn handle_approval_with_context(
        &self,
        _call: &ToolCall,
        _level: PermissionLevel,
        context: &ApprovalContext,
    ) -> ApprovalDecision {
        self.prompt(context).await
    }
}

impl ConsoleApprovalHandler {
    async fn prompt(&self, context: &ApprovalContext) -> ApprovalDecision {
        if context.shown_command.is_some() {
            return self.prompt_command_card(context).await;
        }
        eprintln!("\nApproval required\n{}", context.render());
        loop {
            eprint!("Approve? [y/N] (details): ");
            let _ = io::stderr().flush();
            let line = match self.input.read_line_async().await {
                Ok(line) => line,
                Err(_) => return ApprovalDecision::denied_input_closed(),
            };
            if line.trim().eq_ignore_ascii_case("details") {
                eprintln!("{}", context.render());
                continue;
            }
            let state = InteractionState {
                approval_pending: true,
                ..InteractionState::default()
            };
            match reduce_interaction(&state, InteractionInput::ApprovalAnswer(&line)) {
                InteractionReduction::ApprovalDecision(InteractionDecision::Accept) => {
                    return ApprovalDecision::granted_user();
                }
                InteractionReduction::ApprovalDecision(InteractionDecision::Reject) => {
                    return ApprovalDecision::denied();
                }
                InteractionReduction::Error { message, .. } => {
                    eprintln!("{message}");
                }
                _ => return ApprovalDecision::denied(),
            }
        }
    }

    async fn prompt_command_card(&self, context: &ApprovalContext) -> ApprovalDecision {
        let Some(command) = context.shown_command.as_deref() else {
            return ApprovalDecision::denied_unshowable();
        };
        let environment = if context.command_environment.is_empty() {
            "local"
        } else {
            context.command_environment.as_str()
        };
        let card = nib::interaction_card::command_approval_card(
            environment,
            &context.reason,
            command,
            &context.command_extras,
            context.remember_exact.as_deref(),
            0,
            false,
        );
        eprintln!("\n{}", card.text);
        if let Some(error) = &context.input_error {
            eprintln!("{error}");
        }
        loop {
            eprint!("> ");
            let _ = io::stderr().flush();
            let line = match self.input.read_line_async().await {
                Ok(line) => line,
                Err(_) => return ApprovalDecision::denied_input_closed(),
            };
            match nib::interaction_card::plain_command_line(&card.rows, &line) {
                nib::interaction_card::PlainCommandLine::Retry(message) => eprintln!("{message}"),
                nib::interaction_card::PlainCommandLine::GrantOnce => {
                    return ApprovalDecision::granted_user();
                }
                nib::interaction_card::PlainCommandLine::Remember => {
                    let Some(exact) = context.remember_exact.clone() else {
                        eprintln!("this command cannot be remembered");
                        continue;
                    };
                    return ApprovalDecision::granted_remembered(exact);
                }
                nib::interaction_card::PlainCommandLine::Deny => return ApprovalDecision::denied(),
                nib::interaction_card::PlainCommandLine::NeedReason => {
                    eprint!("Reason to record: ");
                    let _ = io::stderr().flush();
                    let reason = match self.input.read_line_async().await {
                        Ok(reason) => reason,
                        Err(_) => return ApprovalDecision::denied_input_closed(),
                    };
                    let reason = reason.trim();
                    if reason.is_empty() {
                        return ApprovalDecision::denied();
                    }
                    if reason.len() > 240 {
                        eprintln!("Input error: the reason is too long");
                        continue;
                    }
                    return ApprovalDecision::denied_with_reason(reason.to_string());
                }
            }
        }
    }
}

pub struct ConsoleQuestionHandler {
    input: ConsoleInput,
}

impl ConsoleQuestionHandler {
    pub fn new(input: ConsoleInput) -> Self {
        Self { input }
    }
}

#[async_trait::async_trait]
impl QuestionHandler for ConsoleQuestionHandler {
    async fn ask(&self, question: &str, options: &[String]) -> Result<String, String> {
        match self
            .ask_with_context(QuestionRequestContext {
                invocation_id: nib::tools::ToolInvocationId::new(),
                question,
                proposed_answer: None,
                options,
            })
            .await
        {
            QuestionOutcome::Answered(answer) => Ok(answer),
            QuestionOutcome::ApprovedProposal(answer) => Ok(answer),
            QuestionOutcome::LeftUnanswered => Err("left unanswered".to_string()),
            QuestionOutcome::Cancelled => Err("cancelled".to_string()),
            QuestionOutcome::InputClosed => Err("input closed".to_string()),
            QuestionOutcome::InputUnavailable(error) => Err(error),
        }
    }

    async fn ask_with_context(&self, context: QuestionRequestContext<'_>) -> QuestionOutcome {
        println!("\nQuestion: {}", context.question);
        if let Some(proposal) = context.proposed_answer {
            println!("Proposed answer: {proposal}");
            println!(
                "1. Approve proposed answer\n2. Reject and leave unanswered\n3. Instruct otherwise"
            );
        }
        if context.proposed_answer.is_none() {
            for (index, option) in context.options.iter().enumerate() {
                println!("  {}. {}", index + 1, option);
            }
        }
        loop {
            if context.proposed_answer.is_some() {
                print!("Decision or answer: ");
            } else if context.options.is_empty() {
                print!("Answer: ");
            } else {
                print!("Answer (number or text): ");
            }
            if io::stdout().flush().is_err() {
                return QuestionOutcome::InputUnavailable(
                    "failed to flush question prompt".to_string(),
                );
            }
            let line = match self.input.read_line_async().await {
                Ok(line) => line,
                Err(_) => return QuestionOutcome::InputClosed,
            };
            if let Some(proposal) = context.proposed_answer {
                match nib::interactive::parse_proposed_question_input(&line) {
                    nib::interactive::ProposedQuestionInput::Approve => {
                        return QuestionOutcome::ApprovedProposal(proposal.to_string());
                    }
                    nib::interactive::ProposedQuestionInput::Reject => {
                        return QuestionOutcome::LeftUnanswered;
                    }
                    nib::interactive::ProposedQuestionInput::InstructOtherwise => {
                        println!("Type the alternative answer, then press Enter");
                        continue;
                    }
                    nib::interactive::ProposedQuestionInput::Answer(answer) => {
                        return QuestionOutcome::Answered(answer);
                    }
                    nib::interactive::ProposedQuestionInput::Retry(message) => {
                        eprintln!("{message}");
                        continue;
                    }
                }
            }
            let state = InteractionState {
                question_pending: true,
                ..InteractionState::default()
            };
            match reduce_interaction(
                &state,
                InteractionInput::QuestionAnswer {
                    answer: &line,
                    options: context.options,
                    selected_option: None,
                },
            ) {
                InteractionReduction::QuestionAnswered(answer) => {
                    return QuestionOutcome::Answered(answer);
                }
                InteractionReduction::QuestionLeftUnanswered => {
                    return QuestionOutcome::LeftUnanswered;
                }
                InteractionReduction::QuestionInputClosed => {
                    return QuestionOutcome::InputClosed;
                }
                InteractionReduction::ModalCommand(_) => {
                    eprintln!("{}", modal_command_unsupported_message());
                }
                InteractionReduction::Error { message, .. } => {
                    eprintln!("{message}");
                }
                _ => {
                    return QuestionOutcome::InputUnavailable(
                        "question input was rejected by the shared reducer".to_string(),
                    );
                }
            }
        }
    }
}

#[cfg(test)]
fn plain_modal_line_is_owned(
    state: &InteractionState,
    line: &str,
    expected: InteractionConsumer,
) -> bool {
    matches!(
        reduce_interaction(state, InteractionInput::SubmittedLine(line)),
        InteractionReduction::Consumed(consumer) if consumer == expected
    )
}

#[cfg(test)]
fn parse_question_answer(line: &str, options: &[String]) -> Result<String, String> {
    let state = InteractionState {
        question_pending: true,
        ..InteractionState::default()
    };
    match reduce_interaction(
        &state,
        InteractionInput::QuestionAnswer {
            answer: line,
            options,
            selected_option: None,
        },
    ) {
        InteractionReduction::QuestionAnswered(answer) => Ok(answer),
        InteractionReduction::Error { message, .. } => Err(message),
        InteractionReduction::ModalCommand(_) => {
            Err(modal_command_unsupported_message().to_string())
        }
        _ => Err("question input was rejected by the shared reducer".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Cursor, Read};

    struct ChannelRead {
        receiver: std::sync::mpsc::Receiver<Vec<u8>>,
        current: Cursor<Vec<u8>>,
    }

    impl Read for ChannelRead {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            loop {
                let read = self.current.read(buffer)?;
                if read > 0 {
                    return Ok(read);
                }
                self.current = Cursor::new(self.receiver.recv().map_err(|_| {
                    io::Error::new(io::ErrorKind::UnexpectedEof, "test input closed")
                })?);
            }
        }
    }

    #[test]
    fn shared_interaction_reducer_routes_plain_approval_and_question_lines() {
        for (state, expected) in [
            (
                InteractionState {
                    approval_pending: true,
                    question_pending: true,
                    ..InteractionState::default()
                },
                InteractionConsumer::Approval,
            ),
            (
                InteractionState {
                    question_pending: true,
                    ..InteractionState::default()
                },
                InteractionConsumer::Question,
            ),
        ] {
            assert!(plain_modal_line_is_owned(&state, "response\n", expected));
        }
    }

    #[test]
    fn console_broker_starts_only_when_input_is_requested() {
        let input = ConsoleInput::new(Cursor::new(b"answer\n"));
        assert!(!input.broker_started());
        assert_eq!(input.read_line_blocking().unwrap(), "answer\n");
        assert!(input.broker_started());
    }

    #[tokio::test]
    async fn contextual_console_approval_grants_yes_and_retries_empty_input() {
        let call = ToolCall {
            invocation_id: nib::tools::ToolInvocationId::new(),
            tool_name: "run_terminal".to_string(),
            arguments: json!({"command": "task test"}),
            session_id: None,
            project_root: None,
        };
        let context = ApprovalContext::compatibility(&call, PermissionLevel::Destructive);
        let approved = ConsoleApprovalHandler::new(ConsoleInput::new(Cursor::new(b"y\n")))
            .handle_approval_with_context(&call, PermissionLevel::Destructive, &context)
            .await;
        assert!(approved.granted);
        assert_eq!(approved.source, "user");

        let denied = ConsoleApprovalHandler::new(ConsoleInput::new(Cursor::new(b"\n")))
            .handle_approval_with_context(&call, PermissionLevel::Destructive, &context)
            .await;
        assert!(!denied.granted);
        assert_eq!(denied.source, "input_closed");
    }

    #[tokio::test]
    async fn cancelled_modal_waiters_do_not_own_or_consume_the_next_console_line() {
        let call = ToolCall {
            invocation_id: nib::tools::ToolInvocationId::new(),
            tool_name: "run_terminal".to_string(),
            arguments: json!({"command": "task test"}),
            session_id: None,
            project_root: None,
        };
        let context = ApprovalContext::compatibility(&call, PermissionLevel::Destructive);
        let (approval_tx, approval_rx) = std::sync::mpsc::channel();
        let approval_input = ConsoleInput::new(io::BufReader::new(ChannelRead {
            receiver: approval_rx,
            current: Cursor::new(Vec::new()),
        }));
        let approval_waiter = {
            let input = approval_input.clone();
            let call = call.clone();
            tokio::spawn(async move {
                ConsoleApprovalHandler::new(input)
                    .handle_approval_with_context(&call, PermissionLevel::Destructive, &context)
                    .await
            })
        };
        tokio::task::yield_now().await;
        approval_waiter.abort();
        let _ = approval_waiter.await;
        approval_tx.send(b"next-after-approval\n".to_vec()).unwrap();
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                approval_input.read_line_async()
            )
            .await
            .expect("approval input remains available")
            .unwrap(),
            "next-after-approval\n"
        );

        let (question_tx, question_rx) = std::sync::mpsc::channel();
        let question_input = ConsoleInput::new(io::BufReader::new(ChannelRead {
            receiver: question_rx,
            current: Cursor::new(Vec::new()),
        }));
        let question_waiter = {
            let input = question_input.clone();
            tokio::spawn(async move {
                ConsoleQuestionHandler::new(input)
                    .ask("continue?", &[])
                    .await
            })
        };
        tokio::task::yield_now().await;
        question_waiter.abort();
        let _ = question_waiter.await;
        question_tx.send(b"next-after-question\n".to_vec()).unwrap();
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                question_input.read_line_async()
            )
            .await
            .expect("question input remains available")
            .unwrap(),
            "next-after-question\n"
        );
    }

    #[test]
    fn question_answers_accept_numbered_options_and_free_form_text() {
        let options = ["fast".to_string(), "full".to_string()];
        assert_eq!(parse_question_answer("1\n", &options).unwrap(), "fast");
        assert_eq!(parse_question_answer("2\n", &options).unwrap(), "full");
        assert_eq!(
            parse_question_answer("custom response\n", &["fast".to_string()]).unwrap(),
            "custom response"
        );
        assert!(parse_question_answer("\n", &[]).is_err());
        assert!(parse_question_answer("0\n", &options).is_err());
        assert!(parse_question_answer("3\n", &options).is_err());
        assert!(parse_question_answer("999999999999999999999999\n", &options).is_err());
        assert_eq!(parse_question_answer("123\n", &[]).unwrap(), "123");
    }

    #[test]
    fn console_broker_framing_rejects_unbounded_and_invalid_lines() {
        let mut maximum = vec![b'x'; MAX_CONSOLE_LINE_BYTES - 1];
        maximum.push(b'\n');
        assert_eq!(
            read_bounded_console_line(&mut Cursor::new(maximum))
                .expect("maximum line")
                .expect("line"),
            format!("{}\n", "x".repeat(MAX_CONSOLE_LINE_BYTES - 1))
        );

        let oversized = vec![b'x'; MAX_CONSOLE_LINE_BYTES + 1];
        let error = read_bounded_console_line(&mut Cursor::new(oversized))
            .expect_err("newline-free oversized input must fail without unbounded allocation");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("framing limit"));

        let invalid = vec![0xff, b'\n'];
        let error = read_bounded_console_line(&mut Cursor::new(invalid))
            .expect_err("invalid UTF-8 must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
