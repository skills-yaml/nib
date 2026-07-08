//! Static tool registry with permission metadata.

use crate::tools::models::{PermissionLevel, ToolMetadata};
use std::collections::HashMap;
use std::sync::LazyLock;

static REGISTRY: LazyLock<HashMap<&'static str, ToolMetadata>> = LazyLock::new(|| {
    let tools = vec![
        ToolMetadata {
            name: "read_file".to_string(),
            description: "Read file contents (optionally limited to line range).".to_string(),
            permission_level: PermissionLevel::ReadOnly,
            requires_approval: false,
            requires_worktree: false,
        },
        ToolMetadata {
            name: "list_directory".to_string(),
            description: "List directory contents.".to_string(),
            permission_level: PermissionLevel::ReadOnly,
            requires_approval: false,
            requires_worktree: false,
        },
        ToolMetadata {
            name: "grep".to_string(),
            description: "Search files by content (substring match).".to_string(),
            permission_level: PermissionLevel::ReadOnly,
            requires_approval: false,
            requires_worktree: false,
        },
        ToolMetadata {
            name: "apply_patch".to_string(),
            description: "Apply a unified diff/patch inside a worktree.".to_string(),
            permission_level: PermissionLevel::Safe,
            requires_approval: true,
            requires_worktree: true,
        },
        ToolMetadata {
            name: "run_terminal".to_string(),
            description: "Execute a shell command (highest risk).".to_string(),
            permission_level: PermissionLevel::Destructive,
            requires_approval: true,
            requires_worktree: true,
        },
        ToolMetadata {
            name: "write_plan".to_string(),
            description: "Write a structured plan to disk (used in plan mode).".to_string(),
            permission_level: PermissionLevel::Plan,
            requires_approval: false,
            requires_worktree: false,
        },
    ];
    tools
        .into_iter()
        .map(|t| {
            let key: &'static str = Box::leak(t.name.clone().into_boxed_str());
            (key, t)
        })
        .collect()
});

pub fn get_tool_metadata(name: &str) -> Option<&'static ToolMetadata> {
    REGISTRY.get(name)
}

pub fn list_tools() -> Vec<&'static ToolMetadata> {
    REGISTRY.values().collect()
}

pub fn get_permission_level(name: &str) -> Option<PermissionLevel> {
    get_tool_metadata(name).map(|m| m.permission_level)
}

pub fn tool_names() -> Vec<&'static str> {
    REGISTRY.keys().copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_tools_registered() {
        let names: Vec<_> = tool_names();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"run_terminal"));
    }
}
