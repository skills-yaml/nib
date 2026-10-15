//! T080 phase 6: sandbox levels and retrying a blocked command outside the
//! sandbox (AC-7).

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Counts prompts and answers each with `grant`.
struct Answering {
    interactive: bool,
    grant: bool,
    prompts: AtomicUsize,
    reasons: Mutex<Vec<String>>,
}

impl Answering {
    fn new(interactive: bool, grant: bool) -> Arc<Self> {
        Arc::new(Self {
            interactive,
            grant,
            prompts: AtomicUsize::new(0),
            reasons: Mutex::new(Vec::new()),
        })
    }

    fn prompts(&self) -> usize {
        self.prompts.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ApprovalHandler for Answering {
    fn can_prompt(&self) -> bool {
        self.interactive
    }

    async fn handle_approval(&self, _call: &ToolCall, _level: PermissionLevel) -> ApprovalDecision {
        self.prompts.fetch_add(1, Ordering::SeqCst);
        if self.grant {
            ApprovalDecision::granted_user()
        } else {
            ApprovalDecision::denied()
        }
    }

    async fn handle_approval_with_context(
        &self,
        call: &ToolCall,
        level: PermissionLevel,
        context: &ApprovalContext,
    ) -> ApprovalDecision {
        self.reasons
            .lock()
            .expect("reasons")
            .push(context.reason.clone());
        self.handle_approval(call, level).await
    }
}

fn with_sandbox(level: &str) -> ExecutionConfig {
    ExecutionConfig {
        plan_mode: false,
        sandbox: level.to_string(),
        ..ExecutionConfig::default()
    }
}

fn terminal(root: &Path, command: &str) -> ToolCall {
    ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "run_terminal".to_string(),
        arguments: json!({"command": command}),
        session_id: None,
        project_root: Some(root.to_path_buf()),
    }
}

fn blocked_output() -> Value {
    json!({
        "command_success": false,
        "provider": "bwrap",
        "error": "command exited with 1",
        "stderr": "touch: cannot touch '.git/x': Read-only file system",
    })
}

async fn escalation(executor: &ToolExecutor, root: &Path, output: &Value) -> Option<Escalation> {
    let call = terminal(root, "touch .git/x");
    let risk = crate::tools::classifier::classify_tool_call(&call);
    let level = PermissionLevel::Destructive;
    let config = executor.effective_execution_config(level, risk);
    executor
        .sandbox_escalation(&call, level, risk, (root, &config, false), None, output)
        .await
}

#[test]
fn each_sandbox_level_shapes_the_command_config() {
    let root = tempfile::tempdir().expect("root");
    let elevated = |config: ExecutionConfig| {
        ToolExecutor::new(root.path().to_path_buf(), config)
            .effective_execution_config(PermissionLevel::Destructive, ToolRisk::Destructive)
    };

    let write = elevated(with_sandbox("workspace-write"));
    assert_eq!(write.provider, "hybrid");
    assert_eq!(write.default_profile, "restricted");

    // full-access stays direct even for commands the sandbox would take.
    let full = elevated(with_sandbox("full-access"));
    assert_eq!(full.provider, "internal");
    assert_eq!(full.default_profile, "internal");
    assert_eq!(full.boundaries.network, "enabled");

    // read-only always uses the sandbox, with nothing writable and no network.
    let mut read_only = with_sandbox("read-only");
    read_only.provider = "internal".to_string();
    read_only.boundaries.network = "enabled".to_string();
    read_only.boundaries.allow_write = vec!["out".to_string()];
    let read_only = elevated(read_only);
    assert_eq!(read_only.provider, "hybrid");
    assert_eq!(read_only.default_profile, crate::sandbox::READ_ONLY_PROFILE);
    assert_eq!(read_only.boundaries.network, "restricted");
    assert!(read_only.boundaries.allow_write.is_empty());
}

#[test]
fn instructions_that_require_bwrap_turn_full_access_into_workspace_write() {
    let root = tempfile::tempdir().expect("root");
    std::fs::write(
        root.path().join("AGENTS.md"),
        "- nib-sandbox: require-bwrap\n",
    )
    .expect("instructions");
    let posture = ToolExecutor::effective_execution_posture(
        root.path(),
        with_sandbox("full-access"),
        &ApprovalsConfig::default(),
    );
    assert_eq!(
        posture.sandbox,
        crate::sandbox::SandboxLevel::WorkspaceWrite
    );
    let config = ToolExecutor::new(root.path().to_path_buf(), with_sandbox("full-access"))
        .effective_execution_config(PermissionLevel::Destructive, ToolRisk::Destructive);
    assert_eq!(config.provider, "bwrap");
}

#[tokio::test]
async fn the_read_only_level_refuses_edits_and_git_writes() {
    let root = tempfile::tempdir().expect("root");
    let handler = Answering::new(true, true);
    let executor = ToolExecutor::new(root.path().to_path_buf(), with_sandbox("read-only"))
        .with_approval_handler(handler.clone())
        .with_approval_mode(ApprovalMode::Off);
    for (tool, arguments) in [
        (
            "apply_patch",
            json!({"patch": "*** Begin Patch\n*** Add File: notes.txt\n+hi\n*** End Patch\n"}),
        ),
        ("git_commit", json!({"message": "x"})),
        ("git_push", json!({})),
    ] {
        let call = ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: tool.to_string(),
            arguments,
            session_id: None,
            project_root: Some(root.path().to_path_buf()),
        };
        assert!(!executor.requires_interactive_approval(&call), "{tool}");
        let metadata = crate::tools::registry::get_tool_metadata(tool).expect("tool");
        let risk = crate::tools::classifier::classify_tool_call(&call);
        let config = executor.effective_execution_config(metadata.permission_level, risk);
        let decision = executor
            .handle_approval(
                &call,
                metadata.permission_level,
                risk,
                metadata.requires_approval,
                metadata.requires_worktree,
                root.path(),
                &config,
                None,
            )
            .await;
        assert!(!decision.granted, "{tool}");
        assert_eq!(decision.note.as_deref(), Some(READ_ONLY_SANDBOX_DENIAL));
    }
    assert_eq!(handler.prompts(), 0);
}

/// Escalation always asks a person, in every mode but plan, even with
/// `--yes`; without a person it adds a note instead of prompting.
#[tokio::test]
async fn a_blocked_command_is_offered_outside_the_sandbox_only_by_prompt() {
    let root = tempfile::tempdir().expect("root");
    for mode in [ApprovalMode::Off, ApprovalMode::Manual, ApprovalMode::Smart] {
        let handler = Answering::new(true, true);
        let executor =
            ToolExecutor::new(root.path().to_path_buf(), with_sandbox("workspace-write"))
                .with_approval_handler(handler.clone())
                .with_approval_mode(mode)
                .with_auto_approve(true);
        let Some(Escalation::Retry(decision, config)) =
            escalation(&executor, root.path(), &blocked_output()).await
        else {
            panic!("{mode:?}: expected a retry");
        };
        assert!(decision.granted);
        assert_eq!(config.provider, "internal");
        assert_eq!(config.boundaries.network, "enabled");
        assert_eq!(handler.prompts(), 1, "{mode:?}");
        assert_eq!(
            handler.reasons.lock().expect("reasons").as_slice(),
            [ESCALATION_REASON.to_string()]
        );
    }

    let declining = Answering::new(true, false);
    let executor = ToolExecutor::new(root.path().to_path_buf(), with_sandbox("workspace-write"))
        .with_approval_handler(declining.clone());
    let Some(Escalation::Note(note)) = escalation(&executor, root.path(), &blocked_output()).await
    else {
        panic!("expected a note");
    };
    assert!(note.contains("did not retry"), "{note}");

    let headless = Answering::new(false, true);
    let executor = ToolExecutor::new(root.path().to_path_buf(), with_sandbox("workspace-write"))
        .with_approval_handler(headless.clone())
        .with_approval_mode(ApprovalMode::Off)
        .with_auto_approve(true);
    let Some(Escalation::Note(note)) = escalation(&executor, root.path(), &blocked_output()).await
    else {
        panic!("expected a note");
    };
    assert_eq!(note, HEADLESS_ESCALATION_NOTE);
    assert_eq!(headless.prompts(), 0);
}

#[tokio::test]
async fn no_escalation_in_plan_mode_for_other_failures_or_when_bwrap_is_required() {
    let root = tempfile::tempdir().expect("root");
    let handler = Answering::new(true, true);
    let plan = ToolExecutor::new(root.path().to_path_buf(), with_sandbox("workspace-write"))
        .with_approval_handler(handler.clone())
        .with_approval_mode(ApprovalMode::Plan);
    assert!(escalation(&plan, root.path(), &blocked_output())
        .await
        .is_none());

    let ask = ToolExecutor::new(root.path().to_path_buf(), with_sandbox("workspace-write"))
        .with_approval_handler(handler.clone());
    let mut compile_error = blocked_output();
    compile_error["stderr"] = json!("error[E0308]: mismatched types");
    assert!(escalation(&ask, root.path(), &compile_error)
        .await
        .is_none());
    let mut direct = blocked_output();
    direct["provider"] = json!("internal");
    assert!(escalation(&ask, root.path(), &direct).await.is_none());

    let mut pinned = with_sandbox("workspace-write");
    pinned.provider = "bwrap".to_string();
    let pinned =
        ToolExecutor::new(root.path().to_path_buf(), pinned).with_approval_handler(handler.clone());
    let Some(Escalation::Note(note)) = escalation(&pinned, root.path(), &blocked_output()).await
    else {
        panic!("expected a note");
    };
    assert_eq!(note, PINNED_SANDBOX_NOTE);
    assert_eq!(handler.prompts(), 0);
}

fn bwrap_usable() -> bool {
    let available = crate::sandbox::detect_capabilities().bwrap_available;
    if std::env::var_os("NIB_REQUIRE_BWRAP_TESTS").is_some() {
        assert!(
            available,
            "native bwrap isolation is required on this runner"
        );
    }
    available
}

fn git_project() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().expect("project");
    let root = directory.path().canonicalize().expect("canonical");
    super::stale_write_tests::git(&root, &["init", "--quiet"]);
    (directory, root)
}

/// End to end in bwrap: a write to read-only Git metadata fails, is offered
/// for a retry and, on yes, runs on the host. Headless, nothing changes.
#[tokio::test]
async fn a_sandbox_blocked_write_runs_on_the_host_only_after_a_yes() {
    if !bwrap_usable() {
        return;
    }
    let (_directory, root) = git_project();
    let command = "touch .git/escalated";

    let headless = Answering::new(false, true);
    let mut executor = ToolExecutor::new(root.clone(), with_sandbox("workspace-write"))
        .with_approval_handler(headless.clone())
        .with_approval_mode(ApprovalMode::Off);
    let result = executor.execute(terminal(&root, command), None).await;
    assert!(!result.success, "{result:?}");
    let error = result.error.unwrap_or_default();
    assert!(error.contains(HEADLESS_ESCALATION_NOTE), "{error}");
    assert!(!root.join(".git/escalated").exists());
    assert_eq!(headless.prompts(), 0);

    let person = Answering::new(true, true);
    let mut executor = ToolExecutor::new(root.clone(), with_sandbox("workspace-write"))
        .with_approval_handler(person.clone())
        .with_approval_mode(ApprovalMode::Off);
    let result = executor.execute(terminal(&root, command), None).await;
    assert!(result.success, "{result:?}");
    let output = result.output.expect("output");
    assert_eq!(output["escalated"], true);
    assert_eq!(output["provider"], "internal");
    // The retry's output is its own, not the blocked run's.
    let stderr = output["stderr"].as_str().unwrap_or_default();
    assert!(!stderr.contains("Read-only file system"), "{stderr}");
    assert!(root.join(".git/escalated").exists());
    assert_eq!(person.prompts(), 1);
}

/// End to end in bwrap: the read-only level writes nothing to the project
/// but has a private /tmp.
#[tokio::test]
async fn the_read_only_level_writes_only_to_a_private_tmp() {
    if !bwrap_usable() {
        return;
    }
    let (_directory, root) = git_project();
    let handler = Answering::new(false, true);
    let mut executor = ToolExecutor::new(root.clone(), with_sandbox("read-only"))
        .with_approval_handler(handler)
        .with_approval_mode(ApprovalMode::Off);

    let result = executor
        .execute(terminal(&root, "touch created.txt"), None)
        .await;
    assert!(!result.success, "{result:?}");
    assert!(!root.join("created.txt").exists());

    let marker = format!("nib-read-only-{}", Uuid::new_v4());
    let result = executor
        .execute(
            terminal(
                &root,
                &format!("touch /tmp/{marker} && test -f /tmp/{marker}"),
            ),
            None,
        )
        .await;
    assert!(result.success, "{result:?}");
    assert!(!Path::new("/tmp").join(&marker).exists());
}
