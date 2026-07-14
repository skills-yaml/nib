//! Smart Approval Classifier

use crate::tools::models::ToolCall;

#[derive(Debug, PartialEq, Eq)]
pub enum ToolRisk {
    Safe,
    RequiresApproval,
}

pub fn classify_tool_call(call: &ToolCall) -> ToolRisk {
    if call.tool_name != "run_command" {
        return ToolRisk::RequiresApproval;
    }

    if let Some(command) = call.arguments.get("command").and_then(|v| v.as_str()) {
        let cmd = command.trim();

        let safe_prefixes = [
            "git status",
            "git log",
            "git diff",
            "git show",
            "cargo test",
            "cargo check",
            "cargo build",
            "cargo fmt",
            "cargo clippy",
            "ls",
            "cat ",
            "echo ",
            "pwd",
            "whoami",
        ];

        for prefix in &safe_prefixes {
            if cmd.starts_with(prefix) {
                if cmd.contains(";")
                    || cmd.contains("&&")
                    || cmd.contains("||")
                    || cmd.contains("|")
                {
                    return ToolRisk::RequiresApproval;
                }
                return ToolRisk::Safe;
            }
        }
    }

    ToolRisk::RequiresApproval
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_classify_safe_commands() {
        let call = ToolCall {
            tool_name: "run_command".to_string(),
            arguments: json!({"command": "cargo check"}),
            session_id: None,
            project_root: None,
        };
        assert_eq!(classify_tool_call(&call), ToolRisk::Safe);
    }

    #[test]
    fn test_classify_chained_commands() {
        let call = ToolCall {
            tool_name: "run_command".to_string(),
            arguments: json!({"command": "cargo check && rm -rf /"}),
            session_id: None,
            project_root: None,
        };
        assert_eq!(classify_tool_call(&call), ToolRisk::RequiresApproval);
    }
}
