use nib::agent::QuestionHandler;
use nib::tools::executor::ApprovalHandler;
use nib::tools::models::{ApprovalDecision, PermissionLevel, ToolCall};
use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct ConsoleInput {
    reader: Arc<Mutex<Box<dyn BufRead + Send>>>,
}

impl ConsoleInput {
    pub fn stdin() -> Self {
        Self::new(io::BufReader::new(io::stdin()))
    }

    pub fn new(reader: impl BufRead + Send + 'static) -> Self {
        Self {
            reader: Arc::new(Mutex::new(Box::new(reader))),
        }
    }

    pub fn read_line_blocking(&self) -> Result<String, String> {
        let mut line = String::new();
        let read = self
            .reader
            .lock()
            .map_err(|_| "console input lock was poisoned".to_string())?
            .read_line(&mut line)
            .map_err(|error| format!("failed to read console input: {error}"))?;
        if read == 0 {
            return Err("console input closed before a response was received".to_string());
        }
        Ok(line)
    }

    async fn read_line(&self) -> Result<String, String> {
        let input = self.clone();
        tokio::task::spawn_blocking(move || input.read_line_blocking())
            .await
            .map_err(|error| format!("console input worker failed: {error}"))?
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
        eprintln!("\nApproval required for {}", call.tool_name);
        eprintln!("Permission level: {level:?}");
        eprintln!("Arguments: {}", call.arguments);
        eprint!("Approve? [y/N]: ");
        let _ = io::stderr().flush();

        match self.input.read_line().await {
            Ok(line) if line.trim().eq_ignore_ascii_case("y") => ApprovalDecision::granted_user(),
            _ => ApprovalDecision::denied(),
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
        println!("\nQuestion: {question}");
        for (index, option) in options.iter().enumerate() {
            println!("  {}. {}", index + 1, option);
        }
        if options.is_empty() {
            print!("Answer: ");
        } else {
            print!("Answer (number or text): ");
        }
        io::stdout()
            .flush()
            .map_err(|error| format!("failed to flush question prompt: {error}"))?;

        parse_question_answer(&self.input.read_line().await?, options)
    }
}

fn parse_question_answer(line: &str, options: &[String]) -> Result<String, String> {
    let answer = line.trim();
    if answer.is_empty() {
        return Err("question response cannot be empty".to_string());
    }
    if options.is_empty() {
        return Ok(answer.to_string());
    }
    if answer.bytes().all(|byte| byte.is_ascii_digit()) {
        let index = answer
            .parse::<usize>()
            .map_err(|_| format!("question option {answer} is out of range"))?;
        if !(1..=options.len()).contains(&index) {
            return Err(format!("question option {index} is out of range"));
        }
        return options
            .get(index - 1)
            .cloned()
            .ok_or_else(|| format!("question option {index} is out of range"));
    }
    Ok(answer.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
