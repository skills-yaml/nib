//! Integration tests for scoped, approved, isolated, and audited tool execution.

use nib::config::{ApprovalsConfig, BoundaryConfig, ExecutionConfig, TerminalConfig};
use nib::session::{Plan, PlanStep, SessionStore};
use nib::tools::executor::ApprovalHandler;
use nib::tools::models::{
    AfterToolHook, ApprovalDecision, PermissionLevel, PolicyEffect, PolicyRule,
};
use nib::tools::{ApprovalMode, ToolCall, ToolExecutor};
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tempfile::{tempdir, TempDir};

#[test]
fn executor_constructor_does_not_create_legacy_session_state() {
    let root = tempdir().expect("tempdir");
    let _executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default());
    assert!(!root.path().join(".nib/sessions").exists());
}

#[test]
fn task_reads_are_noninteractive_but_state_reconciliation_requires_approval() {
    let root = tempdir().expect("tempdir");
    let executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default());
    let task_call = |action: &str| ToolCall {
        tool_name: "manage_task".to_string(),
        arguments: json!({"action": action}),
        session_id: None,
        project_root: Some(root.path().to_path_buf()),
    };

    assert!(!executor.requires_interactive_approval(&task_call("list")));
    assert!(executor.requires_interactive_approval(&task_call("reconcile")));
    assert!(executor.requires_interactive_approval(&task_call("cancel")));
}

#[tokio::test]
async fn executor_fallback_audit_store_is_profile_scoped() {
    let root = tempdir().expect("tempdir");
    let mut executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default());

    let result = executor
        .execute(
            call("list_directory", json!({"path": "."}), root.path()),
            Some("profile-fallback"),
        )
        .await;

    assert!(result.success, "{:?}", result.error);
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    assert!(store.load_result("profile-fallback").unwrap().is_some());
    assert!(!root.path().join(".nib/sessions").exists());
}

#[tokio::test]
async fn profile_memory_tool_is_session_audited_and_survives_restart() {
    let root = tempdir().expect("tempdir");
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    store.create_session_with_id("memory-session");
    let mut executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
        .with_session_store(store.clone())
        .with_approval_mode(ApprovalMode::Policy)
        .with_policy_rules([PolicyRule {
            effect: PolicyEffect::Allow,
            tool_name: "manage_memory".to_string(),
            argument_contains: Some("set".to_string()),
            reason: "test permits the explicit memory write".to_string(),
        }]);
    let memory_call = |action: &str, value: Option<&str>| {
        let mut arguments = json!({
            "action": action,
            "namespace": "environment",
            "key": "canonical_test",
        });
        if let Some(value) = value {
            arguments["value"] = json!(value);
        }
        ToolCall {
            tool_name: "manage_memory".to_string(),
            arguments,
            session_id: Some("memory-session".to_string()),
            project_root: Some(root.path().to_path_buf()),
        }
    };

    let written = executor
        .execute(
            memory_call("set", Some("task check")),
            Some("memory-session"),
        )
        .await;
    assert!(written.success, "{:?}", written.error);
    let read = executor
        .execute(memory_call("get", None), Some("memory-session"))
        .await;
    assert!(read.success, "{:?}", read.error);
    assert_eq!(read.output.as_ref().unwrap()["value"], "task check");
    let denied_delete = executor
        .execute(memory_call("delete", None), Some("memory-session"))
        .await;
    assert!(!denied_delete.success);
    assert_eq!(denied_delete.approval_source.as_deref(), Some("policy"));

    let profiles = nib::profile::ProfileRegistry::load(
        root.path(),
        &nib::config::load_nib_config_full(root.path())
            .expect("config")
            .profiles,
    )
    .expect("restarted profiles");
    assert_eq!(
        profiles
            .default_profile()
            .memory_store()
            .environment_result("canonical_test")
            .expect("restarted memory")
            .as_deref(),
        Some("task check")
    );
    let session = store.load("memory-session").expect("audited session");
    assert_eq!(
        session
            .tool_calls
            .iter()
            .filter(|record| record.tool_name.as_deref() == Some("manage_memory"))
            .count(),
        3
    );
}

#[tokio::test]
async fn sessionless_read_is_redacted_and_persisted_in_an_implicit_audit_session() {
    let root = tempdir().expect("tempdir");
    let secret = "provider-credential-without-a-known-prefix";
    std::fs::write(
        root.path().join("config.txt"),
        format!("credential={secret}\ncolor=green\n"),
    )
    .expect("secret fixture");
    let mut executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
        .with_sensitive_values([secret.to_string()]);

    let result = executor
        .execute(
            call("read_file", json!({"path": "config.txt"}), root.path()),
            None,
        )
        .await;

    assert!(result.success, "{:?}", result.error);
    let content = result.output.unwrap()["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!content.contains(secret));
    assert!(content.contains("credential=[REDACTED]"));
    assert!(content.contains("color=green"));
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    let session_ids = store.list_result().expect("implicit audit sessions");
    assert_eq!(session_ids.len(), 1);
    let session = store
        .load_result(&session_ids[0])
        .expect("load audit session")
        .expect("audit session");
    assert!(session
        .events
        .iter()
        .any(|event| event.kind == "implicit_audit_session"));
    assert_eq!(session.tool_calls.len(), 1);
    assert_eq!(
        session.tool_calls[0].tool_name.as_deref(),
        Some("read_file")
    );
    assert_eq!(
        session.tool_calls[0].result.as_ref().unwrap()["success"],
        true
    );
}

fn call(tool_name: &str, arguments: serde_json::Value, root: &Path) -> ToolCall {
    ToolCall {
        tool_name: tool_name.to_string(),
        arguments,
        session_id: None,
        project_root: Some(root.to_path_buf()),
    }
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .expect("git starts");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_repository() -> TempDir {
    let directory = tempdir().expect("tempdir");
    git(directory.path(), &["init", "-q"]);
    git(
        directory.path(),
        &["config", "user.email", "nib-tests@example.invalid"],
    );
    git(directory.path(), &["config", "user.name", "nib tests"]);
    std::fs::write(directory.path().join("note.txt"), "old\n").expect("fixture");
    git(directory.path(), &["add", "note.txt"]);
    git(directory.path(), &["commit", "-qm", "initial"]);
    directory
}

fn execution_without_plan_gate() -> ExecutionConfig {
    ExecutionConfig {
        plan_mode: false,
        ..ExecutionConfig::default()
    }
}

struct GrantAndCount {
    approvals: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ApprovalHandler for GrantAndCount {
    async fn handle_approval(&self, _call: &ToolCall, _level: PermissionLevel) -> ApprovalDecision {
        self.approvals.fetch_add(1, Ordering::SeqCst);
        ApprovalDecision::granted_user()
    }
}

#[tokio::test]
async fn read_only_call_succeeds_and_outside_root_attempt_is_audited() {
    let root = tempdir().expect("root");
    let outside = tempdir().expect("outside");
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    let mut executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
        .with_session_store(store.clone());

    let result = executor
        .execute(
            call("list_directory", json!({"path": "."}), root.path()),
            None,
        )
        .await;
    assert!(result.success);

    let denied = executor
        .execute(
            call("read_file", json!({"path": "secret"}), outside.path()),
            Some("scope-test"),
        )
        .await;
    assert!(!denied.success);
    assert!(denied.error.unwrap().contains("outside configured root"));

    let session = store.load("scope-test").expect("audit session");
    assert_eq!(session.events[0].kind, "tool_attempted");
    assert_eq!(session.tool_calls.len(), 1);
    assert_eq!(
        session.tool_calls[0].tool_name.as_deref(),
        Some("read_file")
    );
    assert_eq!(
        session.tool_calls[0].result.as_ref().unwrap()["success"],
        false
    );
}

#[tokio::test]
async fn agents_policy_denies_before_creating_a_worktree() {
    let root = tempdir().expect("root");
    std::fs::write(
        root.path().join("AGENTS.md"),
        "- nib-policy: deny run_terminal cargo check\n",
    )
    .expect("policy");
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    let mut executor = ToolExecutor::new(root.path().to_path_buf(), execution_without_plan_gate())
        .with_session_store(store.clone());

    let result = executor
        .execute(
            call(
                "run_terminal",
                json!({"command": "cargo check"}),
                root.path(),
            ),
            Some("policy-test"),
        )
        .await;

    assert!(!result.success);
    assert_eq!(result.approval_source.as_deref(), Some("policy"));
    assert!(!root.path().join(".nib/worktrees").exists());
    let session = store.load("policy-test").expect("audit session");
    assert_eq!(session.tool_calls.len(), 1);
    assert_eq!(
        session.tool_calls[0].result.as_ref().unwrap()["risk"],
        "safe"
    );
}

#[tokio::test]
async fn agents_named_boundary_profile_preserves_approval_worktree_and_audit() {
    let root = git_repository();
    std::fs::write(
        root.path().join("AGENTS.md"),
        "- nib-policy: require-approval run_terminal\n- nib-boundary: profile locked\n",
    )
    .expect("instruction fixture");
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    let mut session = store.create_session_with_id("named-profile");
    let mut plan = Plan::new(
        "run the named-profile command",
        vec![PlanStep {
            description: "run the named-profile command".to_string(),
            status: "Pending".to_string(),
            outcome: None,
            attempts: 0,
            updated_at: None,
        }],
    );
    plan.approve();
    let expected_plan_id = plan.id.clone();
    session.plan = Some(plan);
    store.save(&mut session).expect("approved session plan");
    let approvals = Arc::new(AtomicUsize::new(0));
    let mut config = ExecutionConfig {
        provider: "internal".to_string(),
        default_profile: "internal".to_string(),
        plan_mode: true,
        boundaries: BoundaryConfig {
            allow_write: Vec::new(),
            network: "enabled".to_string(),
        },
        boundary_profiles: HashMap::new(),
    };
    config.boundary_profiles.insert(
        "locked".to_string(),
        BoundaryConfig {
            allow_write: Vec::new(),
            network: "restricted".to_string(),
        },
    );
    let executor = ToolExecutor::new(root.path().to_path_buf(), config);
    assert_eq!(executor.execution_config.provider, "hybrid");
    assert_eq!(executor.execution_config.default_profile, "locked");
    assert_eq!(executor.execution_config.boundaries.network, "restricted");
    let mut executor = executor
        .with_session_store(store.clone())
        .with_approval_handler(Arc::new(GrantAndCount {
            approvals: Arc::clone(&approvals),
        }));

    let result = executor
        .execute(
            call(
                "run_terminal",
                json!({"command": "printf 'selected\\n' > selected.txt"}),
                root.path(),
            ),
            Some("named-profile"),
        )
        .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.approval_source.as_deref(), Some("user"));
    assert_eq!(approvals.load(Ordering::SeqCst), 1);
    assert_eq!(result.output.as_ref().unwrap()["sandbox_profile"], "locked");
    assert_eq!(
        result.output.as_ref().unwrap()["boundaries"]["network"],
        "restricted"
    );
    assert!(!root.path().join("selected.txt").exists());

    let session = store.load("named-profile").expect("persisted audit");
    let record = session.tool_calls.last().expect("terminal record");
    let worktree = Path::new(record.worktree_path.as_deref().expect("session worktree"));
    assert_eq!(
        std::fs::read_to_string(worktree.join("selected.txt")).expect("worktree output"),
        "selected\n"
    );
    assert_eq!(record.sandbox_profile.as_deref(), Some("locked"));
    assert_eq!(
        record.provider.as_deref(),
        result.output.as_ref().unwrap()["provider"].as_str()
    );
    assert_eq!(
        record
            .boundaries
            .as_ref()
            .expect("resolved boundaries")
            .network,
        "restricted"
    );
    assert_eq!(
        record.result.as_ref().unwrap()["approval"]["source"],
        "user"
    );
    assert_eq!(record.plan_id.as_deref(), Some(expected_plan_id.as_str()));
}

#[tokio::test]
async fn invalid_unknown_or_weaker_agents_boundary_profiles_fail_before_approval_and_worktree() {
    let mut weaker = execution_without_plan_gate();
    weaker.boundary_profiles.insert(
        "open".to_string(),
        BoundaryConfig {
            allow_write: Vec::new(),
            network: "enabled".to_string(),
        },
    );
    let cases = [
        (
            "invalid-profile",
            "- nib-boundary: profile ../escape\n",
            execution_without_plan_gate(),
            "invalid or reserved boundary profile name",
        ),
        (
            "unknown-profile",
            "- nib-boundary: profile missing\n",
            execution_without_plan_gate(),
            "unknown boundary profile 'missing'",
        ),
        (
            "weaker-profile",
            "- nib-boundary: profile open\n",
            weaker,
            "would weaken configured boundaries",
        ),
    ];

    for (session_id, directive, config, expected_reason) in cases {
        let root = git_repository();
        std::fs::write(root.path().join("AGENTS.md"), directive).expect("instruction fixture");
        let store = SessionStore::for_project(root.path()).expect("profile session store");
        store.create_session_with_id(session_id);
        let approvals = Arc::new(AtomicUsize::new(0));
        let executor = ToolExecutor::new(root.path().to_path_buf(), config);
        assert_eq!(executor.execution_config.provider, "bwrap");
        assert_eq!(executor.execution_config.default_profile, "restricted");
        assert_eq!(executor.execution_config.boundaries.network, "disabled");
        assert!(executor.execution_config.boundaries.allow_write.is_empty());
        let mut executor = executor
            .with_session_store(store.clone())
            .with_approval_handler(Arc::new(GrantAndCount {
                approvals: Arc::clone(&approvals),
            }));

        let result = executor
            .execute(
                call(
                    "run_terminal",
                    json!({"command": "touch must-not-exist"}),
                    root.path(),
                ),
                Some(session_id),
            )
            .await;

        assert!(!result.success);
        assert_eq!(result.approval_source.as_deref(), Some("policy"));
        assert_eq!(approvals.load(Ordering::SeqCst), 0);
        assert!(!root.path().join("must-not-exist").exists());
        assert!(!root.path().join(".nib/worktrees").exists());
        let session = store.load(session_id).expect("denial audit");
        let record = session.tool_calls.last().expect("denied tool record");
        assert!(record.worktree_path.is_none());
        assert_eq!(record.sandbox_profile.as_deref(), Some("restricted"));
        assert_eq!(
            record
                .boundaries
                .as_ref()
                .expect("fail-closed boundary")
                .network,
            "disabled"
        );
        let note = record.result.as_ref().unwrap()["approval"]["note"]
            .as_str()
            .expect("policy denial reason");
        assert!(note.contains(expected_reason), "{note}");
    }
}

#[tokio::test]
async fn mutating_tools_require_an_approved_plan() {
    let root = git_repository();
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    let mut session = store.create_session_with_id("planned");
    let plan = Plan::new(
        "inspect repository",
        vec![PlanStep {
            description: "inspect repository".to_string(),
            status: "Pending".to_string(),
            outcome: None,
            attempts: 0,
            updated_at: None,
        }],
    );
    let expected_plan_id = plan.id.clone();
    session.plan = Some(plan);
    store.save(&mut session).expect("save plan");
    let mut executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
        .with_session_store(store.clone());

    let terminal_call = || {
        call(
            "run_terminal",
            json!({"command": "printf safe"}),
            root.path(),
        )
    };
    let denied = executor.execute(terminal_call(), Some("planned")).await;
    assert!(!denied.success);
    assert!(denied
        .error
        .unwrap()
        .contains("approved persisted session plan"));
    assert!(!root.path().join(".nib/worktrees/sessions").exists());

    let mut session = store.load("planned").expect("plan session");
    session.plan.as_mut().unwrap().approve();
    store.save(&mut session).expect("approve plan");
    let allowed = executor.execute(terminal_call(), Some("planned")).await;
    assert!(allowed.success, "{:?}", allowed.error);
    assert_eq!(allowed.approval_source.as_deref(), Some("classifier"));

    let session = store.load("planned").expect("audit session");
    let record = session.tool_calls.last().expect("terminal audit");
    assert_eq!(record.plan_id.as_deref(), Some(expected_plan_id.as_str()));
    assert!(record.worktree_path.is_some());
    assert!(record.boundaries.is_some());

    let mut legacy_session = store.create_session_with_id("legacy-plan");
    let mut legacy_plan = Plan::new(
        "legacy plan",
        vec![PlanStep {
            description: "legacy plan".to_string(),
            status: "Pending".to_string(),
            outcome: None,
            attempts: 0,
            updated_at: None,
        }],
    );
    legacy_plan.id.clear();
    legacy_plan.approve();
    legacy_session.plan = Some(legacy_plan);
    store
        .save(&mut legacy_session)
        .expect("save legacy approved plan");
    let denied = executor.execute(terminal_call(), Some("legacy-plan")).await;
    assert!(!denied.success);
    assert!(denied
        .error
        .as_deref()
        .is_some_and(|error| error.contains("valid identity")));

    let mut completed_session = store.create_session_with_id("completed-plan");
    let mut completed_plan = Plan::new(
        "completed plan",
        vec![PlanStep {
            description: "completed plan".to_string(),
            status: "Pending".to_string(),
            outcome: None,
            attempts: 0,
            updated_at: None,
        }],
    );
    completed_plan.approve();
    completed_plan.complete_current_step("already completed");
    assert!(completed_plan.is_complete());
    assert!(completed_plan.is_structured());
    completed_session.plan = Some(completed_plan);
    store
        .save(&mut completed_session)
        .expect("save completed approved plan");
    let denied = executor
        .execute(terminal_call(), Some("completed-plan"))
        .await;
    assert!(!denied.success);
    assert!(denied
        .error
        .as_deref()
        .is_some_and(|error| error.contains("incomplete work")));
}

#[tokio::test]
async fn caller_plan_id_cannot_forge_or_override_audit_linkage() {
    let root = git_repository();
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    store.create_session_with_id("authoritative-plan-link");
    let mut executor = ToolExecutor::new(root.path().to_path_buf(), execution_without_plan_gate())
        .with_auto_approve(true)
        .with_session_store(store.clone());

    let forged = executor
        .execute(
            call(
                "run_terminal",
                json!({"command": "printf no-plan", "plan_id": "caller-forged"}),
                root.path(),
            ),
            Some("authoritative-plan-link"),
        )
        .await;
    assert!(forged.success, "{:?}", forged.error);
    let session = store
        .load("authoritative-plan-link")
        .expect("forged-link audit");
    assert!(
        session
            .tool_calls
            .last()
            .expect("forged call")
            .plan_id
            .is_none(),
        "caller input must not create authoritative plan linkage"
    );

    let mut session = session;
    let mut plan = Plan::new(
        "run the persisted plan",
        vec![PlanStep {
            description: "run the persisted plan".to_string(),
            status: "Pending".to_string(),
            outcome: None,
            attempts: 0,
            updated_at: None,
        }],
    );
    plan.approve();
    let expected_plan_id = plan.id.clone();
    session.plan = Some(plan);
    store.save(&mut session).expect("persist identified plan");

    let mismatched = executor
        .execute(
            call(
                "run_terminal",
                json!({"command": "printf persisted", "plan_id": "caller-mismatch"}),
                root.path(),
            ),
            Some("authoritative-plan-link"),
        )
        .await;
    assert!(mismatched.success, "{:?}", mismatched.error);
    let session = store
        .load("authoritative-plan-link")
        .expect("authoritative-link audit");
    assert_eq!(
        session
            .tool_calls
            .last()
            .expect("mismatched call")
            .plan_id
            .as_deref(),
        Some(expected_plan_id.as_str())
    );
}

#[tokio::test]
async fn patch_defaults_to_dry_run_and_terminal_nonzero_is_failure() {
    let root = git_repository();
    let environment = HashMap::from([("NIB_HOOK_VALUE".to_string(), "profile-value".to_string())]);
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    let mut executor = ToolExecutor::new(root.path().to_path_buf(), execution_without_plan_gate())
        .with_auto_approve(true)
        .with_environment(&environment)
        .with_session_store(store.clone())
        .with_after_tool_hooks([AfterToolHook {
            source: "test-skill".to_string(),
            tool_name: "apply_patch".to_string(),
            command: "printf %s \"$NIB_HOOK_VALUE\" > hook.txt".to_string(),
        }]);
    let patch = "diff --git a/note.txt b/note.txt\n--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";

    let dry_run = executor
        .execute(
            call("apply_patch", json!({"patch": patch}), root.path()),
            Some("edit-test"),
        )
        .await;
    assert!(dry_run.success, "{:?}", dry_run.error);
    assert_eq!(dry_run.output.as_ref().unwrap()["dry_run"], true);
    assert_eq!(
        dry_run.output.as_ref().unwrap()["post_hooks"][0]["success"],
        true
    );
    let session = store.load("edit-test").expect("edit audit");
    let worktree = session.tool_calls[0]
        .worktree_path
        .as_deref()
        .expect("worktree");
    assert_eq!(
        std::fs::read_to_string(Path::new(worktree).join("note.txt")).expect("note"),
        "old\n"
    );
    assert_eq!(
        std::fs::read_to_string(Path::new(worktree).join("hook.txt")).expect("hook output"),
        "profile-value"
    );

    let applied = executor
        .execute(
            call(
                "apply_patch",
                json!({"patch": patch, "dry_run": false}),
                root.path(),
            ),
            Some("edit-test"),
        )
        .await;
    assert!(applied.success, "{:?}", applied.error);
    assert_eq!(
        std::fs::read_to_string(Path::new(worktree).join("note.txt")).expect("note"),
        "new\n"
    );

    let failed = executor
        .execute(
            call("run_terminal", json!({"command": "false"}), root.path()),
            Some("edit-test"),
        )
        .await;
    assert!(!failed.success);
    assert!(failed.error.unwrap().contains("command exited"));

    executor = executor.with_after_tool_hooks([AfterToolHook {
        source: "failing-skill".to_string(),
        tool_name: "list_directory".to_string(),
        command: "false".to_string(),
    }]);
    let hook_failed = executor
        .execute(
            call("list_directory", json!({"path": "."}), root.path()),
            Some("edit-test"),
        )
        .await;
    assert!(!hook_failed.success);
    assert!(hook_failed
        .error
        .as_deref()
        .unwrap()
        .contains("after-tool hook from failing-skill failed"));

    let raw_audit =
        std::fs::read_to_string(store.sessions_dir().join("edit-test.json")).expect("raw audit");
    assert!(raw_audit.contains("NIB_HOOK_VALUE"));
    assert!(!raw_audit.contains("profile-value"));
}

#[test]
fn executor_accepts_terminal_and_approval_subconfigs() {
    let root = tempdir().expect("root");
    let audit_root = tempdir().expect("audit root");
    let executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
        .with_terminal_config(&TerminalConfig {
            backend: "local".to_string(),
            timeout: 17,
        })
        .with_approvals_config(&ApprovalsConfig {
            mode: "policy".to_string(),
        })
        .with_session_store(SessionStore::new(audit_root.path()));
    assert_eq!(executor.terminal_timeout_secs, 17);
    assert_eq!(executor.terminal_backend, "local");
    assert_eq!(executor.approval_mode, ApprovalMode::Policy);
}

#[tokio::test]
async fn background_terminal_success_returns_to_trusted_session_context() {
    let root = git_repository();
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    store.create_session_with_id("background-success");
    let mut executor = ToolExecutor::new(root.path().to_path_buf(), execution_without_plan_gate())
        .with_auto_approve(true)
        .with_session_store(store.clone());

    let started = executor
        .execute(
            call(
                "run_terminal",
                json!({
                    "command": "printf 'first line\\nsecond\\tline\\n'",
                    "background": true,
                    "_session_id": "spoofed",
                    "_sessions_dir": root.path().join("spoofed"),
                }),
                root.path(),
            ),
            Some("background-success"),
        )
        .await;
    assert!(started.success, "{:?}", started.error);
    let task_id = started.output.as_ref().unwrap()["task_id"]
        .as_str()
        .unwrap();
    wait_for_background_task(task_id).await;

    let task = nib::daemons::task::TASK_MANAGER
        .get_task(task_id)
        .expect("retained task");
    assert_eq!(task["status"], "completed");
    assert_eq!(task["result"]["stdout"], "first line\nsecond\tline\n");
    assert!(!root.path().join("spoofed").exists());
    let session = store.load("background-success").expect("origin session");
    session.validate_message_sequence().unwrap();
    let tool_record = session
        .tool_calls
        .iter()
        .find(|record| {
            record
                .result
                .as_ref()
                .and_then(|result| result.get("output"))
                .and_then(|output| output.get("task_id"))
                .and_then(serde_json::Value::as_str)
                == Some(task_id)
        })
        .expect("reconciled tool record");
    let reconciled = tool_record.result.as_ref().unwrap();
    let final_output = &reconciled["output"];
    assert_eq!(reconciled["success"], true);
    assert!(reconciled["error"].is_null());
    assert_eq!(final_output["stdout"], "first line\nsecond\tline\n");
    assert!(final_output.get("status").is_none());
    assert!(tool_record.error.is_none());
    assert_eq!(
        tool_record.duration_seconds,
        final_output["duration"].as_f64()
    );
    assert_eq!(
        tool_record.provider.as_deref(),
        final_output["provider"].as_str()
    );
    assert_eq!(
        tool_record.sandbox_profile.as_deref(),
        final_output["sandbox_profile"].as_str()
    );
    assert_eq!(
        serde_json::to_value(tool_record.boundaries.as_ref().unwrap()).unwrap(),
        final_output["boundaries"]
    );
    let expected_bwrap: Option<Vec<String>> =
        serde_json::from_value(final_output["bwrap_args"].clone()).unwrap();
    assert_eq!(tool_record.bwrap_args.as_ref(), expected_bwrap.as_ref());
    let observation: serde_json::Value =
        serde_json::from_str(&session.messages.last().unwrap().content).unwrap();
    assert_eq!(session.messages.last().unwrap().role, "tool");
    assert_eq!(observation["success"], true);
    assert_eq!(
        observation["result"]["stdout"],
        "first line\nsecond\tline\n"
    );
    let audit = nib::daemons::task::DaemonAuditLog::at_path(
        store
            .sessions_dir()
            .parent()
            .unwrap()
            .join("daemons/audit.jsonl"),
    )
    .read_all()
    .expect("daemon audit");
    assert_eq!(audit.last().unwrap().outcome, "completed");
}

#[tokio::test]
async fn background_terminal_failure_returns_to_trusted_session_context() {
    let root = git_repository();
    let store = SessionStore::for_project(root.path()).expect("profile session store");
    store.create_session_with_id("background-failure");
    let mut executor = ToolExecutor::new(root.path().to_path_buf(), execution_without_plan_gate())
        .with_auto_approve(true)
        .with_session_store(store.clone());

    let started = executor
        .execute(
            call(
                "run_terminal",
                json!({
                    "command": "printf 'failure detail\\n' >&2; exit 9",
                    "background": true,
                }),
                root.path(),
            ),
            Some("background-failure"),
        )
        .await;
    assert!(started.success, "{:?}", started.error);
    let task_id = started.output.as_ref().unwrap()["task_id"]
        .as_str()
        .unwrap();
    wait_for_background_task(task_id).await;

    let task = nib::daemons::task::TASK_MANAGER
        .get_task(task_id)
        .expect("retained task");
    assert_eq!(task["status"], "failed");
    assert_eq!(task["result"]["stderr"], "failure detail\n");
    assert_eq!(task["result"]["exit_code"], 9);
    let session = store.load("background-failure").expect("origin session");
    session.validate_message_sequence().unwrap();
    let tool_record = session
        .tool_calls
        .iter()
        .find(|record| {
            record
                .result
                .as_ref()
                .and_then(|result| result.get("output"))
                .and_then(|output| output.get("task_id"))
                .and_then(serde_json::Value::as_str)
                == Some(task_id)
        })
        .expect("reconciled tool record");
    let reconciled = tool_record.result.as_ref().unwrap();
    let final_output = &reconciled["output"];
    assert_eq!(reconciled["success"], false);
    assert_eq!(final_output["stderr"], "failure detail\n");
    assert!(reconciled["error"]
        .as_str()
        .unwrap()
        .contains("command exited with 9"));
    assert!(tool_record
        .error
        .as_deref()
        .unwrap()
        .contains("command exited with 9"));
    assert_eq!(
        tool_record.duration_seconds,
        final_output["duration"].as_f64()
    );
    assert_eq!(
        tool_record.provider.as_deref(),
        final_output["provider"].as_str()
    );
    assert_eq!(
        tool_record.sandbox_profile.as_deref(),
        final_output["sandbox_profile"].as_str()
    );
    assert_eq!(
        serde_json::to_value(tool_record.boundaries.as_ref().unwrap()).unwrap(),
        final_output["boundaries"]
    );
    let expected_bwrap: Option<Vec<String>> =
        serde_json::from_value(final_output["bwrap_args"].clone()).unwrap();
    assert_eq!(tool_record.bwrap_args.as_ref(), expected_bwrap.as_ref());
    let observation: serde_json::Value =
        serde_json::from_str(&session.messages.last().unwrap().content).unwrap();
    assert_eq!(observation["success"], false);
    assert!(observation["error"]
        .as_str()
        .unwrap()
        .contains("command exited with 9"));
    let audit = nib::daemons::task::DaemonAuditLog::at_path(
        store
            .sessions_dir()
            .parent()
            .unwrap()
            .join("daemons/audit.jsonl"),
    )
    .read_all()
    .expect("daemon audit");
    assert_eq!(audit.last().unwrap().outcome, "failed");
}

async fn wait_for_background_task(task_id: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if nib::daemons::task::TASK_MANAGER
                .get_status(task_id)
                .as_deref()
                != Some("running")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("background task completes");
}
