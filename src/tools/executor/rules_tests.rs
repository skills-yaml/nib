//! T080 phase 3: permission rules in the approval engine.

use super::*;
use crate::tools::permission_rules::{set_project_trust, TestNibHome};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Answers every prompt with "don't ask again" when the card offers it.
#[derive(Default)]
struct Remembering {
    prompts: AtomicUsize,
    offered: Mutex<Vec<Option<String>>>,
}

#[async_trait::async_trait]
impl ApprovalHandler for Remembering {
    fn can_prompt(&self) -> bool {
        true
    }

    async fn handle_approval(&self, _call: &ToolCall, _level: PermissionLevel) -> ApprovalDecision {
        ApprovalDecision::denied()
    }

    async fn handle_approval_with_context(
        &self,
        _call: &ToolCall,
        _level: PermissionLevel,
        context: &ApprovalContext,
    ) -> ApprovalDecision {
        self.prompts.fetch_add(1, Ordering::SeqCst);
        self.offered
            .lock()
            .expect("offered")
            .push(context.remember_exact.clone());
        let mut decision = ApprovalDecision::granted_user();
        decision.remember_command = context.remember_exact.clone();
        decision
    }
}

fn executor(root: &Path, handler: &Arc<Remembering>) -> ToolExecutor {
    ToolExecutor::new(root.to_path_buf(), ExecutionConfig::default())
        .with_approval_handler(handler.clone())
        .with_approval_mode(ApprovalMode::Manual)
}

fn call(root: &Path, tool: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: tool.to_string(),
        arguments,
        session_id: Some("rules-session".to_string()),
        project_root: Some(root.to_path_buf()),
    }
}

fn terminal(root: &Path, command: &str) -> ToolCall {
    call(
        root,
        "run_terminal",
        json!({"command": command, "affected_paths": ["."]}),
    )
}

async fn decide(executor: &ToolExecutor, root: &Path, call: &ToolCall) -> ApprovalDecision {
    let metadata = crate::tools::registry::get_tool_metadata(&call.tool_name).expect("tool");
    let risk = crate::tools::classifier::classify_tool_call(call);
    let effective = executor.effective_execution_config(metadata.permission_level, risk);
    executor
        .handle_approval(
            call,
            metadata.permission_level,
            risk,
            metadata.requires_approval,
            metadata.requires_worktree,
            root,
            &effective,
            Some("rules-session"),
        )
        .await
}

#[tokio::test]
#[serial_test::serial]
async fn always_allow_saves_a_project_prefix_rule_that_skips_later_prompts() {
    let home = TestNibHome::isolated();
    let root = tempfile::tempdir().expect("root");
    let handler = Arc::new(Remembering::default());
    let executor = executor(root.path(), &handler);

    let first = decide(&executor, root.path(), &terminal(root.path(), "make lint")).await;
    assert!(
        first.granted && first.source == "command_prefix",
        "{first:?}"
    );
    assert_eq!(
        handler.offered.lock().expect("offered").as_slice(),
        [Some("make lint *".to_string())]
    );
    let saved = std::fs::read_to_string(home.directory.path().join("permissions.json"))
        .expect("saved rules");
    assert!(saved.contains("run_terminal(make lint *)"), "{saved}");

    let again = decide(
        &executor,
        root.path(),
        &terminal(root.path(), "make lint -j4"),
    )
    .await;
    assert!(again.granted && again.source == "policy", "{again:?}");
    assert_eq!(handler.prompts.load(Ordering::SeqCst), 1);
    assert!(!executor.requires_interactive_approval(&terminal(root.path(), "make lint -j4")));

    let compound = terminal(root.path(), "make lint && make deploy");
    assert!(executor.requires_interactive_approval(&compound));
    decide(&executor, root.path(), &compound).await;
    assert_eq!(handler.prompts.load(Ordering::SeqCst), 2);
    assert_eq!(handler.offered.lock().expect("offered")[1], None);
}

#[tokio::test]
#[serial_test::serial]
async fn project_instruction_allow_lines_need_trust_and_user_deny_wins() {
    let home = TestNibHome::isolated();
    let root = tempfile::tempdir().expect("root");
    std::fs::write(
        root.path().join("AGENTS.md"),
        "- nib-policy: allow run_terminal make\n",
    )
    .expect("instructions");
    let handler = Arc::new(Remembering::default());
    let command = terminal(root.path(), "make build");

    assert!(executor(root.path(), &handler).requires_interactive_approval(&command));
    set_project_trust(root.path(), true).expect("trust");
    let trusted = executor(root.path(), &handler);
    assert!(!trusted.requires_interactive_approval(&command));
    let decision = decide(&trusted, root.path(), &command).await;
    assert!(
        decision.granted && decision.source == "policy",
        "{decision:?}"
    );
    assert_eq!(handler.prompts.load(Ordering::SeqCst), 0);

    std::fs::write(
        home.directory.path().join("config.toml"),
        "[permissions]\ndeny = [\"terminal(make build *)\"]\n",
    )
    .expect("user config");
    let denied = decide(&trusted, root.path(), &command).await;
    assert!(!denied.granted, "{denied:?}");
    assert!(
        denied
            .note
            .as_deref()
            .is_some_and(|note| note.contains("user deny rule")),
        "{denied:?}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn ask_rules_prompt_even_for_read_only_actions() {
    let home = TestNibHome::isolated();
    let root = tempfile::tempdir().expect("root");
    std::fs::write(
        home.directory.path().join("config.toml"),
        "[permissions]\nask = [\"read(secrets/**)\"]\n",
    )
    .expect("user config");
    let handler = Arc::new(Remembering::default());
    let executor = executor(root.path(), &handler);
    let read = call(root.path(), "read_file", json!({"path": "secrets/key.txt"}));
    assert!(executor.requires_interactive_approval(&read));
    decide(&executor, root.path(), &read).await;
    assert_eq!(handler.prompts.load(Ordering::SeqCst), 1);
    let other = call(root.path(), "read_file", json!({"path": "README.md"}));
    assert!(decide(&executor, root.path(), &other).await.granted);
    assert_eq!(handler.prompts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn apply_patch_refuses_nib_state_and_git_metadata() {
    let root = tempfile::tempdir().expect("root");
    for patch in [
        "--- /dev/null\n+++ b/.nib/permissions.json\n@@ -0,0 +1 @@\n+x\n".to_string(),
        "--- /dev/null\n+++ b/.NIB/x\n@@ -0,0 +1 @@\n+x\n".to_string(),
        // Review F1: a C-quoted name that Git decodes to `.nib`.
        "--- /dev/null\n+++ \"b/\\056nib/config.toml\"\n@@ -0,0 +1 @@\n+x\n".to_string(),
        "diff --git x/.nib/f y/.nib/f\nnew file mode 100644\n".to_string(),
    ] {
        let error = crate::tools::core::dispatch(
            "apply_patch",
            crate::tools::ToolInvocationId::new(),
            &json!({"patch": patch, "dry_run": false}),
            root.path(),
            &ExecutionConfig::default(),
            "local",
            30,
            &HashMap::new(),
            None,
            None,
        )
        .await
        .expect_err("refused");
        assert!(error.contains("nib state or Git metadata"), "{error}");
    }
}

/// Review F6: edits to instruction files always ask, even where file edits
/// are otherwise automatic, and are refused when nobody can answer.
#[tokio::test]
async fn instruction_file_edits_always_ask() {
    let _home = TestNibHome::isolated();
    let root = tempfile::tempdir().expect("root");
    let handler = Arc::new(Remembering::default());
    let accept_edits = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
        .with_approval_handler(handler.clone())
        .with_approval_mode(ApprovalMode::Smart);
    let edit = |path: &str| {
        call(
            root.path(),
            "apply_patch",
            json!({"patch": format!("--- /dev/null\n+++ b/{path}\n@@ -0,0 +1 @@\n+x\n")}),
        )
    };
    let ordinary = decide(&accept_edits, root.path(), &edit("src/lib.rs")).await;
    assert!(
        ordinary.granted && ordinary.source == "mode",
        "{ordinary:?}"
    );
    assert_eq!(handler.prompts.load(Ordering::SeqCst), 0);

    for path in ["AGENTS.md", "claude.local.md"] {
        assert!(accept_edits.requires_interactive_approval(&edit(path)));
        decide(&accept_edits, root.path(), &edit(path)).await;
    }
    assert_eq!(handler.prompts.load(Ordering::SeqCst), 2);
    assert_eq!(handler.offered.lock().expect("offered")[0], None);

    let headless = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
        .with_approval_mode(ApprovalMode::Off);
    let refused = decide(&headless, root.path(), &edit("AGENTS.md")).await;
    assert!(!refused.granted, "{refused:?}");
}

/// Re-review N5: a worktree's own instruction files cannot add allow lines;
/// they come from the trusted main checkout.
#[tokio::test]
async fn worktree_allow_lines_come_from_the_main_checkout() {
    let _home = TestNibHome::isolated();
    let directory = tempfile::tempdir().expect("root");
    let project = directory.path().join("project");
    std::fs::create_dir(&project).expect("project");
    let git = |cwd: &Path, args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .expect("git");
        assert!(output.status.success(), "{output:?}");
    };
    git(&project, &["init", "-q"]);
    git(
        &project,
        &[
            "-c",
            "user.name=f",
            "-c",
            "user.email=f@example.invalid",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "start",
        ],
    );
    let worktree = directory.path().join("worktree");
    git(
        &project,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            worktree.to_str().expect("path"),
        ],
    );
    set_project_trust(&project, true).expect("trust");
    std::fs::write(
        worktree.join("AGENTS.md"),
        "- nib-policy: allow run_terminal\n",
    )
    .expect("planted");
    let handler = Arc::new(Remembering::default());
    let command = terminal(&worktree, "make build");
    assert!(executor(&worktree, &handler).requires_interactive_approval(&command));

    std::fs::write(
        project.join("AGENTS.md"),
        "- nib-policy: allow run_terminal make\n",
    )
    .expect("main");
    assert!(!executor(&worktree, &handler).requires_interactive_approval(&command));
}

/// Re-review N1: an instruction edit inside a project nested in a larger
/// repository still asks.
#[tokio::test]
async fn nested_project_instruction_edits_still_ask() {
    let directory = tempfile::tempdir().expect("root");
    std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(directory.path())
        .output()
        .expect("git init");
    let project = directory.path().join("proj");
    std::fs::create_dir(&project).expect("project");
    let edit = call(
        &project,
        "apply_patch",
        json!({"patch": "--- /dev/null\n+++ b/AGENTS.md\n@@ -0,0 +1 @@\n+x\n"}),
    );
    assert!(always_asks(&edit, &project).is_some());
}
