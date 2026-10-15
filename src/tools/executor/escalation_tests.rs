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
    details: Mutex<Vec<String>>,
}

impl Answering {
    fn new(interactive: bool, grant: bool) -> Arc<Self> {
        Arc::new(Self {
            interactive,
            grant,
            prompts: AtomicUsize::new(0),
            reasons: Mutex::new(Vec::new()),
            details: Mutex::new(Vec::new()),
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
        *self.details.lock().expect("details") = context.command_extras.clone();
        self.handle_approval(call, level).await
    }
}

/// A level chosen for the run (`nib run --sandbox`), so `full-access` needs
/// no trusted project.
fn with_sandbox(level: &str) -> ExecutionConfig {
    ExecutionConfig {
        plan_mode: false,
        sandbox: level.to_string(),
        sandbox_explicit: true,
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

/// Review of 080c50a, R1: any instruction tightening, including a named
/// boundary profile, keeps the sandbox; so do `provider = "bwrap"` and a
/// disabled network (R2).
#[test]
fn every_instruction_tightening_and_sandbox_pin_keeps_full_access_off() {
    let root = tempfile::tempdir().expect("root");
    std::fs::write(
        root.path().join("AGENTS.md"),
        "- nib-boundary: profile locked\n",
    )
    .expect("instructions");
    let mut profiled = with_sandbox("full-access");
    profiled.boundary_profiles.insert(
        "locked".to_string(),
        crate::config::BoundaryConfig {
            network: "restricted".to_string(),
            ..Default::default()
        },
    );
    let posture = ToolExecutor::effective_execution_posture(
        root.path(),
        profiled.clone(),
        &ApprovalsConfig::default(),
    );
    assert_eq!(
        posture.sandbox,
        crate::sandbox::SandboxLevel::WorkspaceWrite
    );
    assert_eq!(
        posture.sandbox_note,
        Some("project instructions tighten the sandbox")
    );
    let config = ToolExecutor::new(root.path().to_path_buf(), profiled)
        .effective_execution_config(PermissionLevel::Destructive, ToolRisk::Destructive);
    assert_ne!(config.provider, "internal");
    assert_eq!(config.boundaries.network, "restricted");

    // A worktree directive changes where sessions work, not the sandbox.
    let worktree_only = tempfile::tempdir().expect("worktree only");
    std::fs::write(
        worktree_only.path().join("AGENTS.md"),
        format!("- {}\n", crate::tools::workspace::WORKTREE_DIRECTIVE),
    )
    .expect("instructions");
    let posture = ToolExecutor::effective_execution_posture(
        worktree_only.path(),
        with_sandbox("full-access"),
        &ApprovalsConfig::default(),
    );
    assert_eq!(posture.sandbox, crate::sandbox::SandboxLevel::FullAccess);
    assert_eq!(posture.sandbox_note, None);

    let plain = tempfile::tempdir().expect("plain");
    for pin in ["bwrap", "disabled"] {
        let mut config = with_sandbox("full-access");
        if pin == "bwrap" {
            config.provider = "bwrap".to_string();
        } else {
            config.boundaries.network = "disabled".to_string();
        }
        let posture = ToolExecutor::effective_execution_posture(
            plain.path(),
            config,
            &ApprovalsConfig::default(),
        );
        assert_eq!(
            posture.sandbox,
            crate::sandbox::SandboxLevel::WorkspaceWrite,
            "{pin}"
        );
        assert!(posture.sandbox_note.is_some(), "{pin}");
    }
}

/// Review of 080c50a, R7: a project's own configuration selects
/// `full-access` only after the user trusts the project.
#[test]
#[serial_test::serial]
fn configured_full_access_needs_a_trusted_project() {
    let _home = crate::tools::permission_rules::TestNibHome::isolated();
    let root = tempfile::tempdir().expect("root");
    let configured = ExecutionConfig {
        sandbox: "full-access".to_string(),
        ..ExecutionConfig::default()
    };
    let posture = ToolExecutor::effective_execution_posture(
        root.path(),
        configured.clone(),
        &ApprovalsConfig::default(),
    );
    assert_eq!(
        posture.sandbox,
        crate::sandbox::SandboxLevel::WorkspaceWrite
    );
    assert!(posture
        .sandbox_note
        .is_some_and(|note| note.contains("trusted project")));

    crate::tools::permission_rules::set_project_trust(root.path(), true).expect("trust");
    let posture = ToolExecutor::effective_execution_posture(
        root.path(),
        configured,
        &ApprovalsConfig::default(),
    );
    assert_eq!(posture.sandbox, crate::sandbox::SandboxLevel::FullAccess);
    assert_eq!(posture.sandbox_note, None);
}

#[test]
fn read_only_profile_name_is_reserved_and_kept_by_the_tools_without_bwrap() {
    assert!(!valid_boundary_profile_name(
        crate::sandbox::READ_ONLY_PROFILE
    ));
    let read_only = apply_sandbox_level(with_sandbox("read-only"));
    let direct = crate::sandbox::SandboxExecutionRoute::Direct;
    let refusal = read_only_direct_refusal(&direct, &read_only, ToolRisk::Destructive)
        .expect("refused without bwrap");
    assert_eq!(
        refusal.note.as_deref(),
        Some(READ_ONLY_UNENFORCEABLE_DENIAL)
    );
    assert!(read_only_direct_refusal(&direct, &read_only, ToolRisk::ReadOnly).is_none());
    let bwrap = crate::sandbox::SandboxExecutionRoute::Bwrap;
    assert!(read_only_direct_refusal(&bwrap, &read_only, ToolRisk::Destructive).is_none());
    let write = apply_sandbox_level(with_sandbox("workspace-write"));
    assert!(read_only_direct_refusal(&direct, &write, ToolRisk::Destructive).is_none());
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

    // An allow rule for the command does not grant the retry.
    let handler = Answering::new(true, true);
    let executor = ToolExecutor::new(root.path().to_path_buf(), with_sandbox("workspace-write"))
        .with_approval_handler(handler.clone())
        .with_policy_rules([PolicyRule {
            effect: PolicyEffect::Allow,
            tool_name: "run_terminal".to_string(),
            argument_contains: None,
            reason: "allowed".to_string(),
        }]);
    assert!(matches!(
        escalation(&executor, root.path(), &blocked_output()).await,
        Some(Escalation::Retry(..))
    ));
    assert_eq!(handler.prompts(), 1);
    let details = handler.details.lock().expect("details").clone();
    assert!(
        details.iter().any(|line| line.contains("Blocked: touch")),
        "{details:?}"
    );
    assert!(details
        .iter()
        .any(|line| line.contains("outside the sandbox")));

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

    let mut offline = with_sandbox("workspace-write");
    offline.boundaries.network = "disabled".to_string();
    let offline = ToolExecutor::new(root.path().to_path_buf(), offline)
        .with_approval_handler(handler.clone());
    let Some(Escalation::Note(note)) = escalation(&offline, root.path(), &blocked_output()).await
    else {
        panic!("expected a note");
    };
    assert_eq!(note, PINNED_SANDBOX_NOTE);

    // Review of 080c50a, R5: the read-only level offers no retry.
    let read_only = ToolExecutor::new(root.path().to_path_buf(), with_sandbox("read-only"))
        .with_approval_handler(handler.clone());
    let Some(Escalation::Note(note)) = escalation(&read_only, root.path(), &blocked_output()).await
    else {
        panic!("expected a note");
    };
    assert_eq!(note, READ_ONLY_ESCALATION_NOTE);

    // R3: an approval ceiling that refuses destructive actions refuses the
    // retry, even when the handler could prompt.
    let capped = Arc::new(Capped(Answering::new(true, true)));
    let ceiling = ToolExecutor::new(root.path().to_path_buf(), with_sandbox("workspace-write"))
        .with_approval_handler(capped.clone());
    let Some(Escalation::Note(note)) = escalation(&ceiling, root.path(), &blocked_output()).await
    else {
        panic!("expected a note");
    };
    assert_eq!(note, HEADLESS_ESCALATION_NOTE);
    assert_eq!(capped.0.prompts(), 0);
    assert_eq!(handler.prompts(), 0);
}

/// Can prompt, but refuses every destructive action by its ceiling.
struct Capped(Arc<Answering>);

#[async_trait::async_trait]
impl ApprovalHandler for Capped {
    fn approval_ceiling(
        &self,
        _call: &ToolCall,
        level: PermissionLevel,
        _risk: ToolRisk,
    ) -> Option<ApprovalDecision> {
        (level == PermissionLevel::Destructive)
            .then(|| ApprovalDecision::denied_by_policy("ceiling"))
    }

    fn can_prompt(&self) -> bool {
        true
    }

    async fn handle_approval(&self, call: &ToolCall, level: PermissionLevel) -> ApprovalDecision {
        self.0.handle_approval(call, level).await
    }
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
    let store = SessionStore::new(&root);
    let session = store.create_session_with_id("escalation-session");
    let mut executor = ToolExecutor::new(root.clone(), with_sandbox("workspace-write"))
        .with_approval_handler(person.clone())
        .with_approval_mode(ApprovalMode::Off)
        .with_session_store(store.clone());
    let result = executor
        .execute(terminal(&root, command), Some(&session.id))
        .await;
    assert!(result.success, "{result:?}");
    let output = result.output.expect("output");
    assert_eq!(output["escalated"], true);
    assert_eq!(output["provider"], "internal");
    // The retry's output is its own, not the blocked run's.
    let stderr = output["stderr"].as_str().unwrap_or_default();
    assert!(!stderr.contains("Read-only file system"), "{stderr}");
    assert!(root.join(".git/escalated").exists());
    assert_eq!(person.prompts(), 1);
    // The audit record keeps the escalation.
    let recorded = store
        .load_result(&session.id)
        .expect("load")
        .expect("session")
        .tool_calls;
    let recorded = serde_json::to_string(&recorded).expect("records");
    assert!(recorded.contains("\"escalated\":true"), "{recorded}");
    assert!(recorded.contains("escalation_approval"), "{recorded}");
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
