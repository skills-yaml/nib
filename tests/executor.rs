//! Integration tests for tool executor.

use nib::tools::{ToolCall, ToolExecutor};
use serde_json::json;
use std::path::PathBuf;
use tempfile::tempdir;

#[tokio::test]
async fn executor_list_directory_read_only() {
    let dir = tempdir().unwrap();
    let mut exec = ToolExecutor::new(
        dir.path().to_path_buf(),
        nib::config::ExecutionConfig::default(),
    )
    .with_auto_approve(true);
    let call = ToolCall {
        tool_name: "list_directory".to_string(),
        arguments: json!({"path": "."}),
        session_id: None,
        project_root: Some(dir.path().to_path_buf()),
    };
    let result = exec.execute(call, None).await;
    assert!(result.success);
}

#[tokio::test]
async fn executor_unknown_tool_fails() {
    let dir = tempdir().unwrap();
    let mut exec = ToolExecutor::new(
        PathBuf::from(dir.path()),
        nib::config::ExecutionConfig::default(),
    );
    let call = ToolCall {
        tool_name: "nonexistent".to_string(),
        arguments: json!({}),
        session_id: None,
        project_root: Some(dir.path().to_path_buf()),
    };
    let result = exec.execute(call, None).await;
    assert!(!result.success);
}
