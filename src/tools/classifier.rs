//! Conservative command classifier used by the approval gate.

use crate::tools::models::ToolCall;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolRisk {
    ReadOnly,
    Safe,
    RequiresApproval,
    Destructive,
    Network,
}

impl ToolRisk {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Safe => "safe",
            Self::RequiresApproval => "requires_approval",
            Self::Destructive => "destructive",
            Self::Network => "network",
        }
    }
}

pub fn classify_tool_call(call: &ToolCall) -> ToolRisk {
    match call.tool_name.as_str() {
        "read_file" | "list_directory" | "grep" => ToolRisk::ReadOnly,
        "manage_subagents" => match call
            .arguments
            .get("action")
            .and_then(|value| value.as_str())
        {
            Some("cancel" | "terminate") => ToolRisk::Destructive,
            _ => ToolRisk::ReadOnly,
        },
        "manage_task" => match call
            .arguments
            .get("action")
            .and_then(|value| value.as_str())
        {
            Some("list" | "get") => ToolRisk::ReadOnly,
            Some("cancel" | "reconcile") => ToolRisk::Safe,
            _ => ToolRisk::RequiresApproval,
        },
        "manage_memory" => match call
            .arguments
            .get("action")
            .and_then(|value| value.as_str())
        {
            Some("delete") => ToolRisk::Destructive,
            Some("list" | "get") => ToolRisk::ReadOnly,
            Some("set") => ToolRisk::Safe,
            _ => ToolRisk::RequiresApproval,
        },
        "run_terminal" => call
            .arguments
            .get("command")
            .and_then(|value| value.as_str())
            .map(classify_command)
            .unwrap_or(ToolRisk::RequiresApproval),
        "search_web" | "read_url_content" => ToolRisk::Network,
        "apply_patch" | "merge_subagent_worktree" => ToolRisk::Destructive,
        "write_plan" | "spawn_subagent" | "invoke_subagent" | "send_message" | "schedule"
        | "ask_question" => ToolRisk::Safe,
        _ => ToolRisk::RequiresApproval,
    }
}

pub fn classify_command(command: &str) -> ToolRisk {
    let command = command.trim();
    if command.is_empty() {
        return ToolRisk::RequiresApproval;
    }

    let lower = command.to_ascii_lowercase();
    let words = command_words(&lower);

    if contains_any(
        &words,
        &[
            "curl", "wget", "ssh", "scp", "sftp", "ftp", "nc", "ncat", "telnet",
        ],
    ) || starts_with_words(&words, &["git", "push"])
        || starts_with_words(&words, &["git", "pull"])
        || starts_with_words(&words, &["git", "fetch"])
        || starts_with_words(&words, &["git", "clone"])
        || starts_with_words(&words, &["cargo", "install"])
    {
        return ToolRisk::Network;
    }

    if contains_any(
        &words,
        &[
            "rm", "rmdir", "sudo", "doas", "dd", "mkfs", "mount", "umount", "shutdown", "reboot",
            "poweroff", "kill", "pkill", "killall", "chmod", "chown",
        ],
    ) || starts_with_words(&words, &["git", "reset"])
        || starts_with_words(&words, &["git", "clean"])
        || starts_with_words(&words, &["git", "checkout", "-f"])
        || lower.contains("drop database")
        || lower.contains("drop table")
        || lower.contains("truncate table")
    {
        return ToolRisk::Destructive;
    }

    if has_shell_composition(command) {
        return ToolRisk::RequiresApproval;
    }

    if references_unscoped_path_or_variable(&words) {
        return ToolRisk::RequiresApproval;
    }

    if is_safe_command(&words) {
        ToolRisk::Safe
    } else {
        ToolRisk::RequiresApproval
    }
}

pub fn safe_command_requires_isolation(command: &str) -> bool {
    let lower = command.trim().to_ascii_lowercase();
    let words = command_words(&lower);
    let program = words
        .first()
        .and_then(|word| word.rsplit('/').next())
        .unwrap_or_default();
    matches!(program, "cargo" | "git")
}

fn command_words(command: &str) -> Vec<&str> {
    command
        .split(|character: char| {
            character.is_whitespace()
                || matches!(character, ';' | '|' | '&' | '(' | ')' | '<' | '>')
        })
        .filter(|word| !word.is_empty())
        .collect()
}

fn contains_any(words: &[&str], candidates: &[&str]) -> bool {
    words.iter().any(|word| {
        let program = word.rsplit('/').next().unwrap_or(word);
        candidates.contains(&program)
    })
}

fn starts_with_words(words: &[&str], prefix: &[&str]) -> bool {
    words.len() >= prefix.len()
        && words
            .iter()
            .zip(prefix)
            .all(|(actual, expected)| actual.rsplit('/').next().unwrap_or(actual) == *expected)
}

fn has_shell_composition(command: &str) -> bool {
    command.contains('\n')
        || command.contains(';')
        || command.contains("&&")
        || command.contains("||")
        || command.contains('|')
        || command.contains('>')
        || command.contains('<')
        || command.contains('`')
        || command.contains("$(")
        || command.trim_end().ends_with('&')
}

fn references_unscoped_path_or_variable(words: &[&str]) -> bool {
    words.iter().skip(1).any(|word| {
        let word = word.trim_matches(['\'', '"']);
        word.starts_with('/')
            || word.starts_with('~')
            || word.contains('$')
            || word == ".."
            || word.starts_with("../")
            || word.contains("/../")
            || word.contains("=/")
            || word.contains("=~")
    })
}

fn is_safe_command(words: &[&str]) -> bool {
    if words.is_empty() {
        return false;
    }

    let program = words[0].rsplit('/').next().unwrap_or(words[0]);
    match program {
        "pwd" | "whoami" | "ls" | "wc" => true,
        "echo" | "printf" => true,
        "cargo" => matches!(
            words.get(1).copied(),
            Some("test" | "check" | "build" | "fmt" | "clippy" | "metadata")
        ),
        "git" => matches!(
            words.get(1).copied(),
            Some("status" | "log" | "diff" | "show" | "rev-parse")
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn terminal(command: &str) -> ToolCall {
        ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: "run_terminal".to_string(),
            arguments: json!({"command": command}),
            session_id: None,
            project_root: None,
        }
    }

    #[test]
    fn classifies_registered_terminal_tool_safe_commands() {
        assert_eq!(classify_tool_call(&terminal("cargo check")), ToolRisk::Safe);
        assert_eq!(
            classify_tool_call(&terminal("git status --short")),
            ToolRisk::Safe
        );
        assert!(safe_command_requires_isolation("cargo check"));
        assert!(safe_command_requires_isolation("git status --short"));
        assert!(!safe_command_requires_isolation("ls ."));
    }

    #[test]
    fn shell_composition_is_not_auto_approved() {
        assert_eq!(
            classify_tool_call(&terminal("cargo check && echo done")),
            ToolRisk::RequiresApproval
        );
        assert_eq!(
            classify_tool_call(&terminal("echo $(touch escaped)")),
            ToolRisk::RequiresApproval
        );
    }

    #[test]
    fn variable_external_and_content_read_arguments_are_not_auto_approved() {
        for command in [
            "printf $DEPLOY_TOKEN",
            "ls /etc",
            "ls ../outside",
            "cat local-secret-link",
            "rg password .",
        ] {
            assert_eq!(
                classify_tool_call(&terminal(command)),
                ToolRisk::RequiresApproval,
                "{command}"
            );
        }
        assert_eq!(classify_tool_call(&terminal("ls .")), ToolRisk::Safe);
    }

    #[test]
    fn destructive_and_network_commands_are_escalated() {
        assert_eq!(
            classify_tool_call(&terminal("rm -rf target")),
            ToolRisk::Destructive
        );
        assert_eq!(
            classify_tool_call(&terminal("curl https://example.com")),
            ToolRisk::Network
        );
        assert_eq!(
            classify_tool_call(&terminal("cargo check; wget https://example.com")),
            ToolRisk::Network
        );
    }

    #[test]
    fn management_actions_are_classified_by_effect() {
        let call = |tool_name: &str, action: &str| ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: tool_name.to_string(),
            arguments: json!({"action": action}),
            session_id: None,
            project_root: None,
        };
        assert_eq!(
            classify_tool_call(&call("manage_subagents", "list")),
            ToolRisk::ReadOnly
        );
        assert_eq!(
            classify_tool_call(&call("manage_subagents", "terminate")),
            ToolRisk::Destructive
        );
        assert_eq!(
            classify_tool_call(&call("manage_task", "cancel")),
            ToolRisk::Safe
        );
        assert_eq!(
            classify_tool_call(&call("manage_task", "reconcile")),
            ToolRisk::Safe
        );
        assert_eq!(
            classify_tool_call(&call("manage_task", "list")),
            ToolRisk::ReadOnly
        );
        assert_eq!(
            classify_tool_call(&call("manage_memory", "list")),
            ToolRisk::ReadOnly
        );
        assert_eq!(
            classify_tool_call(&call("manage_memory", "set")),
            ToolRisk::Safe
        );
        assert_eq!(
            classify_tool_call(&call("manage_memory", "delete")),
            ToolRisk::Destructive
        );
    }
}
