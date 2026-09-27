use super::*;
use crate::config::{save_config, save_nib_config_full, LlmConfig, NibConfig, ProviderEntry};
use crate::tools::models::{ApprovalDecision, PermissionLevel, ToolCall};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tempfile::tempdir;

pub(crate) fn failed_batch_fixture() -> (Vec<ToolCallRequest>, Vec<Value>) {
    let request = ToolCallRequest::new("read_file", json!({"path": "missing.txt"}));
    let observation = json!({
        "invocation_id": request.invocation_id,
        "tool": request.name,
        "success": false,
        "output": Value::Null,
        "error": "file is missing",
    });
    (vec![request], vec![observation])
}

pub(crate) struct EnvironmentGuard {
    pub(crate) name: &'static str,
    pub(crate) previous: Option<std::ffi::OsString>,
}

impl EnvironmentGuard {
    pub(crate) fn set(name: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(name);
        std::env::set_var(name, value);
        Self { name, previous }
    }
}

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.name, value),
            None => std::env::remove_var(self.name),
        }
    }
}

pub(crate) struct DenyApproval;

pub(crate) struct AnswerQuestion;

pub(crate) struct ApprovePlanOnly {
    pub(crate) calls: Arc<AtomicUsize>,
}

pub(crate) struct BlockingApproval {
    pub(crate) entered: Arc<tokio::sync::Notify>,
}

pub(crate) struct ControlledApproval {
    pub(crate) entered: Arc<tokio::sync::Notify>,
    pub(crate) release: Arc<tokio::sync::Notify>,
    pub(crate) granted: bool,
}

#[async_trait::async_trait]
impl QuestionHandler for AnswerQuestion {
    async fn ask(&self, question: &str, options: &[String]) -> Result<String, String> {
        assert_eq!(question, "Which verification mode?");
        assert_eq!(options, ["fast", "full"]);
        Ok("full".to_string())
    }
}

pub(crate) fn mock_config() -> LlmConfig {
    LlmConfig {
        active_provider: Some("mock".to_string()),
        providers: HashMap::from([(
            "mock".to_string(),
            ProviderEntry {
                model: "mock-model".to_string(),
                api_key: None,
                api_keys: Vec::new(),
                base_url: None,
                ..ProviderEntry::default()
            },
        )]),
        ..Default::default()
    }
}

pub(crate) fn pending_plan(goal: &str, description: &str) -> crate::session::Plan {
    crate::session::Plan::new(
        goal,
        vec![crate::session::PlanStep {
            description: description.to_string(),
            status: "Pending".to_string(),
            outcome: None,
            attempts: 0,
            updated_at: None,
            verification_obligations: Vec::new(),
            content_generation: 0,
        }],
    )
}

#[async_trait::async_trait]
impl ApprovalHandler for DenyApproval {
    async fn handle_approval(&self, _call: &ToolCall, _level: PermissionLevel) -> ApprovalDecision {
        ApprovalDecision::denied()
    }
}

#[async_trait::async_trait]
impl ApprovalHandler for ApprovePlanOnly {
    async fn handle_approval(&self, call: &ToolCall, _level: PermissionLevel) -> ApprovalDecision {
        assert_eq!(call.tool_name, "approve_plan");
        self.calls.fetch_add(1, Ordering::SeqCst);
        ApprovalDecision::granted_user()
    }
}

#[async_trait::async_trait]
impl ApprovalHandler for BlockingApproval {
    async fn handle_approval(&self, _call: &ToolCall, _level: PermissionLevel) -> ApprovalDecision {
        self.entered.notify_one();
        std::future::pending().await
    }
}

#[async_trait::async_trait]
impl ApprovalHandler for ControlledApproval {
    async fn handle_approval(&self, _call: &ToolCall, _level: PermissionLevel) -> ApprovalDecision {
        self.entered.notify_one();
        self.release.notified().await;
        if self.granted {
            ApprovalDecision::granted_user()
        } else {
            ApprovalDecision::denied()
        }
    }
}

pub(crate) fn initialize_git_repository(path: &Path) {
    std::fs::write(path.join("README.md"), "test project\n").expect("seed file");
    for args in [
        vec!["init", "--quiet"],
        vec!["config", "user.email", "nib-tests@example.invalid"],
        vec!["config", "user.name", "nib tests"],
        vec!["add", "README.md"],
        vec!["commit", "--quiet", "-m", "initial"],
    ] {
        let status = Command::new("git")
            .args(args)
            .current_dir(path)
            .status()
            .expect("git command");
        assert!(status.success());
    }
}

#[path = "part_a.rs"]
mod part_a;
#[path = "part_b.rs"]
mod part_b;
#[path = "part_c.rs"]
mod part_c;
