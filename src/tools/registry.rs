//! Static tool registry with permission metadata and concrete JSON schemas.

use crate::tools::models::{PermissionLevel, ToolMetadata};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::LazyLock;

fn metadata(
    name: &str,
    description: &str,
    permission_level: PermissionLevel,
    requires_approval: bool,
    requires_worktree: bool,
    input_schema: Value,
) -> ToolMetadata {
    ToolMetadata {
        name: name.to_string(),
        description: description.to_string(),
        permission_level,
        requires_approval,
        requires_worktree,
        mcp_exposable: true,
        input_schema,
    }
}

static REGISTRY: LazyLock<HashMap<&'static str, ToolMetadata>> = LazyLock::new(|| {
    let tools = vec![
        metadata(
            "read_file",
            "Read a bounded section of a scoped UTF-8 file using a zero-based line range.",
            PermissionLevel::ReadOnly,
            false,
            false,
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "minLength": 1, "maxLength": 4096},
                    "start_line": {"type": "integer", "minimum": 0},
                    "end_line": {"type": "integer", "minimum": 0},
                    "max_bytes": {"type": "integer", "minimum": 1, "maximum": 1048576, "default": 65536},
                    "max_lines": {"type": "integer", "minimum": 1, "maximum": 10000, "default": 1000}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        metadata(
            "list_directory",
            "List scoped directory entries with type, size, and modification time.",
            PermissionLevel::ReadOnly,
            false,
            false,
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "minLength": 1, "maxLength": 4096, "default": "."},
                    "recursive": {"type": "boolean", "default": false},
                    "include_hidden": {"type": "boolean", "default": false},
                    "max_depth": {"type": "integer", "minimum": 1, "maximum": 20, "default": 5}
                },
                "additionalProperties": false
            }),
        ),
        metadata(
            "grep",
            "Search scoped UTF-8 files with a regular expression and optional glob filter.",
            PermissionLevel::ReadOnly,
            false,
            false,
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "minLength": 1, "maxLength": 4096},
                    "path": {"type": "string", "minLength": 1, "maxLength": 4096, "default": "."},
                    "glob": {"type": "string", "minLength": 1, "maxLength": 4096},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 50}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        ),
        metadata(
            "apply_patch",
            "Validate or apply a unified diff in the session worktree.",
            PermissionLevel::Safe,
            true,
            true,
            json!({
                "type": "object",
                "properties": {
                    "patch": {"type": "string", "minLength": 1, "maxLength": 2097152},
                    "dry_run": {"type": "boolean", "default": true},
                    "plan_id": {"type": "string", "minLength": 1, "maxLength": 128}
                },
                "required": ["patch"],
                "additionalProperties": false
            }),
        ),
        metadata(
            "run_terminal",
            "Execute a classified shell command in the session worktree and configured sandbox.",
            PermissionLevel::Destructive,
            true,
            true,
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "minLength": 1, "maxLength": 65536},
                    "cwd": {"type": "string", "minLength": 1, "maxLength": 4096, "default": "."},
                    "timeout": {"type": "integer", "minimum": 1, "maximum": 3600, "description": "Overrides the configured terminal timeout."},
                    "background": {"type": "boolean", "default": false},
                    "max_output_bytes": {"type": "integer", "minimum": 1, "maximum": 1048576, "default": 131072, "description": "Maximum retained tail bytes for each of stdout and stderr."},
                    "plan_id": {"type": "string", "minLength": 1, "maxLength": 128}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        ),
        metadata(
            "write_plan",
            "Write a structured plan artifact under .nib/plans.",
            PermissionLevel::Plan,
            false,
            false,
            json!({
                "type": "object",
                "properties": {"content": {"type": "string", "minLength": 1, "maxLength": 1048576}},
                "required": ["content"],
                "additionalProperties": false
            }),
        ),
        metadata(
            "spawn_subagent",
            "Launch a child nib loop with a linked session and dedicated worktree.",
            PermissionLevel::Safe,
            false,
            false,
            json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "minLength": 1, "maxLength": 20000},
                    "max_steps": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Optional explicit child turn bound; omitted uses agent.max_turns."}
                },
                "required": ["prompt"],
                "additionalProperties": false
            }),
        ),
        metadata(
            "invoke_subagent",
            "Launch a child nib loop with a linked session and dedicated worktree.",
            PermissionLevel::Safe,
            false,
            false,
            json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "minLength": 1, "maxLength": 20000},
                    "max_steps": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Optional explicit child turn bound; omitted uses agent.max_turns."}
                },
                "required": ["prompt"],
                "additionalProperties": false
            }),
        ),
        metadata(
            "merge_subagent_worktree",
            "Verify and merge a completed subagent branch into the current branch.",
            PermissionLevel::Destructive,
            true,
            false,
            json!({
                "type": "object",
                "properties": {
                    "subagent_id": {"type": "string", "minLength": 1, "maxLength": 128},
                    "verification_command": {"type": "string", "minLength": 1, "maxLength": 65536},
                    "verification_timeout": {"type": "integer", "minimum": 1, "maximum": 3600, "default": 300}
                },
                "required": ["subagent_id", "verification_command"],
                "additionalProperties": false
            }),
        ),
        metadata(
            "manage_subagents",
            "List, inspect, or terminate linked subagent jobs.",
            PermissionLevel::Destructive,
            true,
            false,
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "get", "cancel", "terminate"], "default": "list"},
                    "subagent_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Required for get, cancel, and terminate."}
                },
                "additionalProperties": false
            }),
        ),
        metadata(
            "send_message",
            "Append a user message to a linked subagent session.",
            PermissionLevel::Safe,
            false,
            false,
            json!({
                "type": "object",
                "properties": {
                    "subagent_id": {"type": "string", "minLength": 1, "maxLength": 128},
                    "message": {"type": "string", "minLength": 1, "maxLength": 20000}
                },
                "required": ["subagent_id", "message"],
                "additionalProperties": false
            }),
        ),
        metadata(
            "search_web",
            "Perform a web query through the configured network integration.",
            PermissionLevel::Network,
            true,
            false,
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "minLength": 1, "maxLength": 500},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 10, "default": 5}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        ),
        metadata(
            "read_url_content",
            "Fetch URL content through the configured network integration.",
            PermissionLevel::Network,
            true,
            false,
            json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "minLength": 1, "maxLength": 8192, "format": "uri", "pattern": "^https?://"},
                    "max_chars": {"type": "integer", "minimum": 1000, "maximum": 100000, "default": 50000}
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        ),
        metadata(
            "manage_task",
            "List, inspect, cancel, or reconcile durable background terminal and timer tasks.",
            PermissionLevel::Safe,
            true,
            false,
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "get", "cancel", "reconcile"], "default": "list"},
                    "task_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Required for get and cancel."}
                },
                "additionalProperties": false
            }),
        ),
        metadata(
            "manage_memory",
            "List, read, persist, or remove profile-scoped environment and user memory.",
            PermissionLevel::Safe,
            true,
            false,
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "get", "set", "delete"]},
                    "namespace": {"type": "string", "enum": ["environment", "user"]},
                    "key": {"type": "string", "minLength": 1, "maxLength": 256},
                    "value": {"type": "string", "maxLength": 65536}
                },
                "required": ["action", "namespace"],
                "additionalProperties": false
            }),
        ),
        metadata(
            "schedule",
            "Schedule a bounded one-shot or recurring timer that emits future session messages.",
            PermissionLevel::Safe,
            false,
            false,
            json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "minLength": 1, "maxLength": 20000},
                    "duration_secs": {"type": "integer", "minimum": 1, "maximum": 31536000, "description": "Delay before the first delivery."},
                    "interval_secs": {"type": "integer", "minimum": 1, "maximum": 31536000, "description": "Delay between recurring deliveries; defaults to duration_secs."},
                    "repeat_count": {"type": "integer", "minimum": 1, "maximum": 100, "default": 1}
                },
                "required": ["prompt", "duration_secs"],
                "additionalProperties": false
            }),
        ),
        metadata(
            "ask_question",
            "Pause the loop and request structured user input.",
            PermissionLevel::Safe,
            false,
            false,
            json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string", "minLength": 1, "maxLength": 20000},
                    "options": {
                        "type": "array",
                        "items": {"type": "string", "minLength": 1, "maxLength": 1000},
                        "maxItems": 20,
                        "default": []
                    }
                },
                "required": ["question"],
                "additionalProperties": false
            }),
        ),
    ];

    tools
        .into_iter()
        .map(|tool| {
            let key: &'static str = Box::leak(tool.name.clone().into_boxed_str());
            (key, tool)
        })
        .collect()
});

pub fn get_tool_metadata(name: &str) -> Option<&'static ToolMetadata> {
    REGISTRY.get(name)
}

pub fn list_tools() -> Vec<&'static ToolMetadata> {
    let mut tools: Vec<_> = REGISTRY.values().collect();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    tools
}

pub fn get_permission_level(name: &str) -> Option<PermissionLevel> {
    get_tool_metadata(name).map(|metadata| metadata.permission_level)
}

pub fn tool_names() -> Vec<&'static str> {
    let mut names: Vec<_> = REGISTRY.keys().copied().collect();
    names.sort_unstable();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_tools_registered_with_concrete_schemas() {
        let names = tool_names();
        for name in [
            "read_file",
            "list_directory",
            "grep",
            "apply_patch",
            "run_terminal",
        ] {
            assert!(names.contains(&name));
            let schema = &get_tool_metadata(name).expect("metadata").input_schema;
            assert_eq!(schema["type"], "object");
        }
    }

    #[test]
    fn delegation_manages_its_own_worktree() {
        assert!(
            !get_tool_metadata("spawn_subagent")
                .expect("metadata")
                .requires_worktree
        );
        assert!(
            !get_tool_metadata("merge_subagent_worktree")
                .expect("metadata")
                .requires_worktree
        );
    }

    #[test]
    fn expanded_tools_have_complete_mcp_schemas() {
        for name in [
            "invoke_subagent",
            "manage_subagents",
            "search_web",
            "read_url_content",
            "manage_task",
            "manage_memory",
            "schedule",
            "ask_question",
        ] {
            let metadata = get_tool_metadata(name).expect("expanded tool metadata");
            assert!(metadata.mcp_exposable, "{name} must be MCP exposed");
            assert_eq!(metadata.input_schema["type"], "object");
        }
        assert_eq!(
            get_tool_metadata("ask_question").unwrap().input_schema["properties"]["options"]
                ["type"],
            "array"
        );
        assert!(
            get_tool_metadata("schedule").unwrap().input_schema["properties"]
                .get("repeat_count")
                .is_some()
        );
        assert!(
            get_tool_metadata("spawn_subagent").unwrap().input_schema["properties"]["max_steps"]
                .get("default")
                .is_none()
        );
    }
}
