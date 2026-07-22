use chrono::Utc;
use nib::agent::CancellationSignal;
use nib::config::{ExecutionConfig, TerminalConfig};
use nib::sandbox::worktree::Worktree;
use nib::session::SessionStore;
#[cfg(all(unix, debug_assertions))]
use nib::tools::delegation::install_merge_interruption_test_barrier;
#[cfg(unix)]
use nib::tools::delegation::list_subagents;
#[cfg(target_os = "linux")]
use nib::tools::delegation::spawn_subagent;
use nib::tools::delegation::{
    cancel_subagent, get_subagent_record, send_message_to_subagent, write_subagent_record,
    SubagentRecord,
};
use nib::tools::executor::{ApprovalHandler, ToolExecutor};
use nib::tools::models::{ApprovalDecision, PermissionLevel, ToolCall};
use serde_json::json;
use std::fs::OpenOptions;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;
use tempfile::{tempdir, TempDir};

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

fn git_stdout(root: &Path, args: &[&str]) -> String {
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
    String::from_utf8(output.stdout)
        .expect("git stdout")
        .trim()
        .to_string()
}

#[tokio::test]
async fn legacy_running_subagent_cancellation_is_explicitly_unresolved() {
    let root = git_repository();
    let worktree = Worktree::create(root.path(), "sub-cancel").expect("worktree");
    let record = SubagentRecord {
        id: "sub-cancel".to_string(),
        parent_session_id: Some("parent".to_string()),
        child_session_id: "child-cancel".to_string(),
        prompt: "wait".to_string(),
        status: "running".to_string(),
        execution_generation: None,
        owner_lease: None,
        worktree_path: worktree.path.clone(),
        branch: worktree.branch.clone(),
        branch_oid: Some(worktree.branch_oid.clone()),
        result: None,
        error: None,
        verification: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    write_subagent_record(root.path(), &record).expect("record");
    nib::daemons::task::TASK_MANAGER.register_subagent("sub-cancel".to_string());
    let handle = tokio::spawn(std::future::pending::<()>());
    nib::daemons::task::TASK_MANAGER
        .attach_abort_handle("sub-cancel", handle.abort_handle())
        .expect("attach abort handle");

    let error = cancel_subagent(root.path(), "sub-cancel")
        .expect_err("legacy running record must fail closed");
    assert!(error.contains("legacy execution ownership"), "{error}");
    assert!(error.contains("manager_stopped: true"), "{error}");
    assert!(
        handle
            .await
            .expect_err("legacy task is aborted")
            .is_cancelled(),
        "legacy task cancellation must join as aborted"
    );
    let persisted = std::fs::read_to_string(root.path().join(".nib/subagents/sub-cancel.json"))
        .expect("persisted legacy record");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&persisted).unwrap()["status"],
        "running"
    );
    assert_eq!(
        nib::daemons::task::TASK_MANAGER
            .get_status("sub-cancel")
            .as_deref(),
        Some("cancelled")
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
    git(directory.path(), &["config", "core.autocrlf", "false"]);
    std::fs::write(directory.path().join(".gitignore"), ".nib/\n").expect("gitignore");
    std::fs::write(directory.path().join("README.md"), "fixture\n").expect("fixture");
    git(directory.path(), &["add", ".gitignore", "README.md"]);
    git(directory.path(), &["commit", "-qm", "initial"]);
    directory
}

fn git_repository_without_ignore() -> TempDir {
    let directory = tempdir().expect("tempdir");
    git(directory.path(), &["init", "-q"]);
    git(
        directory.path(),
        &["config", "user.email", "nib-tests@example.invalid"],
    );
    git(directory.path(), &["config", "user.name", "nib tests"]);
    git(directory.path(), &["config", "core.autocrlf", "false"]);
    std::fs::write(directory.path().join("README.md"), "fixture\n").expect("fixture");
    git(directory.path(), &["add", "README.md"]);
    git(directory.path(), &["commit", "-qm", "initial"]);
    directory
}

#[test]
fn subagent_records_are_bounded_and_atomically_serialized() {
    let root = tempdir().expect("root");
    let template = SubagentRecord {
        id: "shared".to_string(),
        parent_session_id: Some("parent".to_string()),
        child_session_id: "child".to_string(),
        prompt: "fixture".to_string(),
        status: "running".to_string(),
        execution_generation: None,
        owner_lease: None,
        worktree_path: root.path().to_path_buf(),
        branch: "fixture".to_string(),
        branch_oid: None,
        result: None,
        error: None,
        verification: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let successful_creations = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for worker in 0..8 {
            let root = root.path();
            let template = template.clone();
            let successful_creations = &successful_creations;
            scope.spawn(move || {
                for iteration in 0..25 {
                    let mut record = template.clone();
                    record.status = format!("worker-{worker}-{iteration}");
                    match write_subagent_record(root, &record) {
                        Ok(()) => {
                            successful_creations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                        Err(error) => assert!(
                            error.contains("appeared") || error.contains("already exists"),
                            "unexpected creation failure: {error}"
                        ),
                    }
                }
            });
        }
    });
    assert_eq!(
        successful_creations.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    get_subagent_record(root.path(), "shared").expect("final record is valid JSON");

    let oversized = root.path().join(".nib/subagents/oversized.json");
    std::fs::File::create(&oversized)
        .and_then(|file| file.set_len(16 * 1024 * 1024 + 1))
        .expect("oversized sparse record");
    let error = get_subagent_record(root.path(), "oversized")
        .expect_err("oversized record must fail closed");
    assert!(error.contains("exceeds"), "{error}");
}

#[cfg(unix)]
#[test]
fn subagent_listing_rejects_symlinked_records() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("root");
    let outside = tempdir().expect("outside");
    let record_dir = root.path().join(".nib/subagents");
    std::fs::create_dir_all(&record_dir).expect("records");
    std::fs::write(outside.path().join("record.json"), b"{}\n").expect("outside record");
    symlink(
        outside.path().join("record.json"),
        record_dir.join("linked.json"),
    )
    .expect("record symlink");

    let error = list_subagents(root.path()).expect_err("symlinked record must fail closed");
    assert!(error.contains("regular file"), "{error}");
}

#[cfg(unix)]
#[test]
fn subagent_record_creation_rejects_symlinked_project_state() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("root");
    let outside = tempdir().expect("outside");
    symlink(outside.path(), root.path().join(".nib")).expect("state symlink");
    let record = SubagentRecord {
        id: "escaped".to_string(),
        parent_session_id: None,
        child_session_id: "child".to_string(),
        prompt: "fixture".to_string(),
        status: "running".to_string(),
        execution_generation: None,
        owner_lease: None,
        worktree_path: root.path().to_path_buf(),
        branch: "fixture".to_string(),
        branch_oid: None,
        result: None,
        error: None,
        verification: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };

    let error = write_subagent_record(root.path(), &record)
        .expect_err("symlinked project state must fail closed");
    assert!(
        error.contains("local directory") && error.contains("not a symlink"),
        "{error}"
    );
    assert!(!outside.path().join("subagents").exists());
}

fn merge_execution() -> ExecutionConfig {
    ExecutionConfig {
        provider: "internal".to_string(),
        default_profile: "internal".to_string(),
        plan_mode: false,
        ..ExecutionConfig::default()
    }
}

fn merge_executor(root: &Path, store: SessionStore) -> ToolExecutor {
    ToolExecutor::new(root.to_path_buf(), merge_execution())
        .with_auto_approve(true)
        .with_session_store(store)
}

struct MergeOnlyApproval;

#[async_trait::async_trait]
impl ApprovalHandler for MergeOnlyApproval {
    async fn handle_approval(&self, call: &ToolCall, _level: PermissionLevel) -> ApprovalDecision {
        if call.tool_name == "merge_subagent_worktree" {
            ApprovalDecision::granted_user()
        } else {
            ApprovalDecision::denied()
        }
    }
}

fn completed_record(root: &Path, id: &str, worktree: &Worktree) {
    let branch_oid = git_stdout(&worktree.path, &["rev-parse", "HEAD"]);
    Worktree::adopt_branch_revision(root, id, &branch_oid)
        .expect("adopt completed branch revision");
    write_subagent_record(
        root,
        &SubagentRecord {
            id: id.to_string(),
            parent_session_id: Some("parent".to_string()),
            child_session_id: format!("child-{id}"),
            prompt: "fixture".to_string(),
            status: "completed".to_string(),
            execution_generation: None,
            owner_lease: None,
            worktree_path: worktree.path.clone(),
            branch: worktree.branch.clone(),
            branch_oid: Some(branch_oid),
            result: Some(json!({"summary": "done"})),
            error: None,
            verification: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        },
    )
    .expect("record");
}

async fn execute_merge(
    executor: &mut ToolExecutor,
    root: &Path,
    session_id: &str,
    subagent_id: &str,
    verification_command: &str,
) -> nib::tools::ToolResult {
    executor
        .execute(
            ToolCall {
                tool_name: "merge_subagent_worktree".to_string(),
                arguments: json!({
                    "subagent_id": subagent_id,
                    "verification_command": verification_command,
                    "verification_timeout": 10,
                }),
                session_id: Some(session_id.to_string()),
                project_root: Some(root.to_path_buf()),
            },
            Some(session_id),
        )
        .await
}

#[cfg(target_os = "linux")]
async fn wait_for_terminal_record(root: &Path, id: &str) -> SubagentRecord {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let record = get_subagent_record(root, id).expect("subagent record");
            if record.status != "running" {
                return record;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("subagent reaches a terminal state")
}

#[cfg(target_os = "linux")]
fn wait_for_terminal_record_sync(root: &Path, id: &str) -> SubagentRecord {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let record = get_subagent_record(root, id).expect("subagent record");
        if record.status != "running" {
            return record;
        }
        assert!(
            Instant::now() < deadline,
            "subagent did not reach a terminal state after its runtime ended"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn spawned_subagents_reach_durable_completed_and_failed_results_without_stdin() {
    let root = git_repository();

    let completed = spawn_subagent(
        &json!({"prompt": "explore the project", "max_steps": 20}),
        root.path(),
    )
    .expect("spawn completed fixture");
    let completed_id = completed["subagent_id"].as_str().expect("completed id");
    let completed_record = wait_for_terminal_record(root.path(), completed_id).await;
    assert_eq!(
        completed_record.status, "completed",
        "completed subagent record: {completed_record:#?}"
    );
    assert_eq!(
        completed_record.result.as_ref().unwrap()["outcome"],
        "completed"
    );
    assert!(completed_record
        .worktree_path
        .join(".nib/profiles/default/sessions")
        .join(format!("{completed_id}.json"))
        .is_file());
    assert_eq!(
        nib::daemons::task::TASK_MANAGER
            .get_status(completed_id)
            .as_deref(),
        Some("completed")
    );

    let bounded = spawn_subagent(
        &json!({"prompt": "explore the project", "max_steps": 1}),
        root.path(),
    )
    .expect("spawn bounded fixture");
    let bounded_id = bounded["subagent_id"].as_str().expect("bounded id");
    let bounded_record = wait_for_terminal_record(root.path(), bounded_id).await;
    assert_eq!(bounded_record.status, "failed");
    assert_eq!(
        bounded_record.result.as_ref().unwrap()["bound_reached"],
        true
    );
    assert!(bounded_record
        .error
        .as_deref()
        .unwrap()
        .contains("without completion"));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn spawned_subagents_approve_their_plan_but_deny_destructive_actions() {
    let root = git_repository();
    let started = spawn_subagent(
        &json!({"prompt": "subagent destructive denial", "max_steps": 20}),
        root.path(),
    )
    .expect("spawn denial fixture");
    let id = started["subagent_id"].as_str().expect("subagent id");
    let record = wait_for_terminal_record(root.path(), id).await;
    assert_eq!(
        record.status, "completed",
        "destructive-denial subagent record: {record:#?}"
    );

    let store = SessionStore::for_project(&record.worktree_path).expect("child session store");
    let child = store.load(id).expect("child session");
    assert!(child.events.iter().any(|event| {
        event.kind == "plan_approved"
            && event
                .details
                .get("approved")
                .and_then(|value| value.as_bool())
                == Some(true)
    }));
    let denied = child
        .tool_calls
        .iter()
        .find(|call| call.tool_name.as_deref() == Some("run_terminal"))
        .expect("destructive attempt is audited");
    assert_eq!(
        denied.result.as_ref().unwrap()["approval"]["granted"],
        false
    );
    assert_eq!(
        denied.result.as_ref().unwrap()["approval"]["source"],
        "policy"
    );
    assert!(!record
        .worktree_path
        .join("delegated-side-effect.txt")
        .exists());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn subagent_policy_allow_cannot_bypass_mutation_or_network_ceiling() {
    let root = git_repository();
    std::fs::write(
        root.path().join("AGENTS.md"),
        "- nib-policy: allow run_terminal\n",
    )
    .expect("allow policy");
    git(root.path(), &["add", "AGENTS.md"]);
    git(root.path(), &["commit", "-qm", "allow terminal policy"]);

    for (prompt, side_effect) in [
        ("subagent destructive denial", "delegated-side-effect.txt"),
        (
            "subagent network denial",
            "delegated-network-side-effect.txt",
        ),
    ] {
        let started = spawn_subagent(&json!({"prompt": prompt, "max_steps": 20}), root.path())
            .expect("spawn policy fixture");
        let id = started["subagent_id"].as_str().expect("subagent id");
        let record = wait_for_terminal_record(root.path(), id).await;
        assert_eq!(record.status, "completed", "record: {record:#?}");

        let store = SessionStore::for_project(&record.worktree_path).expect("child session store");
        let child = store.load(id).expect("child session");
        let denied = child
            .tool_calls
            .iter()
            .find(|call| call.tool_name.as_deref() == Some("run_terminal"))
            .expect("terminal attempt is audited");
        assert_eq!(
            denied.result.as_ref().unwrap()["approval"]["granted"],
            false
        );
        assert_eq!(
            denied.result.as_ref().unwrap()["approval"]["source"],
            "policy"
        );
        assert!(denied.result.as_ref().unwrap()["approval"]["note"]
            .as_str()
            .unwrap()
            .contains("cannot obtain mutating approval"));
        assert!(!record.worktree_path.join(side_effect).exists());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn dropping_the_runtime_cannot_leave_a_spawned_record_running() {
    let root = git_repository();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let started = runtime.block_on(async {
        spawn_subagent(
            &json!({"prompt": "explore the project", "max_steps": 20}),
            root.path(),
        )
        .expect("spawn")
    });
    let id = started["subagent_id"].as_str().unwrap().to_string();
    drop(runtime);

    let record = wait_for_terminal_record_sync(root.path(), &id);
    assert_eq!(record.status, "cancelled");
    assert!(record.error.as_deref().unwrap().contains("cancelled"));
    assert_eq!(record.result.as_ref().unwrap()["cleanup_verified"], true);
    assert!(record.result.as_ref().unwrap()["cleanup_proof"].is_object());
    assert_eq!(
        nib::daemons::task::TASK_MANAGER.get_status(&id).as_deref(),
        Some("cancelled")
    );
}

#[tokio::test]
async fn subagent_merge_requires_successful_verification_and_preserves_result() {
    let root = git_repository();
    let worktree = Worktree::create(root.path(), "sub-test").expect("worktree");
    std::fs::write(worktree.path.join("result.txt"), "delegated\n").expect("result");
    let record = SubagentRecord {
        id: "sub-test".to_string(),
        parent_session_id: Some("parent".to_string()),
        child_session_id: "child".to_string(),
        prompt: "create result".to_string(),
        status: "completed".to_string(),
        execution_generation: None,
        owner_lease: None,
        worktree_path: worktree.path.clone(),
        branch: worktree.branch.clone(),
        branch_oid: Some(worktree.branch_oid.clone()),
        result: Some(json!({"summary": "done"})),
        error: None,
        verification: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    write_subagent_record(root.path(), &record).expect("record");
    let store = SessionStore::for_project(root.path()).expect("session store");
    store.create_session_with_id("parent");
    let mut executor = merge_executor(root.path(), store.clone());

    let sent = send_message_to_subagent(
        &json!({"subagent_id": "sub-test", "message": "verify before merge"}),
        root.path(),
    )
    .expect("message");
    assert_eq!(sent["child_session_id"], "child");
    assert!(worktree
        .path
        .join(".nib/profiles/default/sessions/child.json")
        .is_file());
    assert!(!worktree.path.join(".nib/sessions").exists());

    let failed = execute_merge(&mut executor, root.path(), "parent", "sub-test", "false").await;
    assert!(!failed.success, "verification must gate merge");
    assert!(failed
        .error
        .as_deref()
        .unwrap()
        .contains("verification command failed"));
    assert!(!root.path().join("result.txt").exists());
    let failed_record = get_subagent_record(root.path(), "sub-test").expect("failed record");
    assert_eq!(failed_record.status, "verification_failed");
    assert!(!failed_record.verification.as_ref().unwrap().success);

    let merged = execute_merge(
        &mut executor,
        root.path(),
        "parent",
        "sub-test",
        "test -f result.txt",
    )
    .await;
    assert!(merged.success, "{:?}", merged.error);
    assert_eq!(merged.output.as_ref().unwrap()["status"], "merged");
    assert_eq!(
        std::fs::read_to_string(root.path().join("result.txt")).expect("merged result"),
        "delegated\n"
    );
    assert!(!worktree.path.exists());
    let branch_ref = format!("refs/heads/{}", worktree.branch);
    assert!(!Command::new("git")
        .current_dir(root.path())
        .args(["show-ref", "--verify", branch_ref.as_str()])
        .status()
        .expect("inspect cleaned branch")
        .success());
    let merged_record = get_subagent_record(root.path(), "sub-test").expect("merged record");
    assert_eq!(merged_record.status, "merged");
    assert_eq!(
        merged_record.result.as_ref().unwrap()["subagent_result"]["summary"],
        "done"
    );
    let evidence = merged_record.verification.as_ref().expect("evidence");
    assert!(evidence.success);
    assert_eq!(evidence.tool_name, "run_terminal");
    let expected_provider = if nib::sandbox::detect_capabilities().bwrap_available {
        "bwrap"
    } else {
        "internal"
    };
    assert_eq!(
        evidence.output.as_ref().unwrap()["provider"],
        expected_provider
    );

    let parent = store.load("parent").expect("parent session");
    assert_eq!(
        parent
            .tool_calls
            .iter()
            .filter(|call| call.tool_name.as_deref() == Some("run_terminal"))
            .count(),
        2,
        "each verification must have its own terminal audit record"
    );
    assert_eq!(
        parent
            .tool_calls
            .iter()
            .filter(|call| call.tool_name.as_deref() == Some("merge_subagent_worktree"))
            .count(),
        2
    );
}

#[tokio::test]
async fn destructive_verification_requires_its_own_approval() {
    let root = git_repository();
    let worktree = Worktree::create(root.path(), "sub-approval").expect("worktree");
    std::fs::write(worktree.path.join("result.txt"), "keep\n").expect("result");
    completed_record(root.path(), "sub-approval", &worktree);
    let store = SessionStore::for_project(root.path()).expect("session store");
    store.create_session_with_id("parent-approval");
    let mut executor = ToolExecutor::new(root.path().to_path_buf(), merge_execution())
        .with_session_store(store.clone())
        .with_approval_handler(Arc::new(MergeOnlyApproval));

    let result = execute_merge(
        &mut executor,
        root.path(),
        "parent-approval",
        "sub-approval",
        "rm -rf result.txt",
    )
    .await;

    assert!(!result.success);
    assert!(worktree.path.join("result.txt").is_file());
    let record = get_subagent_record(root.path(), "sub-approval").expect("record");
    assert_eq!(record.status, "verification_failed");
    let evidence = record.verification.as_ref().expect("evidence");
    assert!(!evidence.approval_granted);
    assert_eq!(evidence.approval_source.as_deref(), Some("denied"));
    let session = store.load("parent-approval").expect("session");
    let verification = session
        .tool_calls
        .iter()
        .find(|call| call.tool_name.as_deref() == Some("run_terminal"))
        .expect("verification audit");
    assert_eq!(
        verification.result.as_ref().unwrap()["approval"]["granted"],
        false
    );
}

#[tokio::test]
async fn verification_honors_terminal_backend_and_persists_failure_evidence() {
    let root = git_repository();
    let worktree = Worktree::create(root.path(), "sub-backend").expect("worktree");
    completed_record(root.path(), "sub-backend", &worktree);
    let store = SessionStore::for_project(root.path()).expect("session store");
    store.create_session_with_id("parent-backend");
    let mut executor =
        merge_executor(root.path(), store.clone()).with_terminal_config(&TerminalConfig {
            backend: "unavailable-terminal".to_string(),
            timeout: 10,
        });

    let result = execute_merge(
        &mut executor,
        root.path(),
        "parent-backend",
        "sub-backend",
        "printf escaped > escaped.txt",
    )
    .await;

    assert!(!result.success);
    assert!(!worktree.path.join("escaped.txt").exists());
    let record = get_subagent_record(root.path(), "sub-backend").expect("record");
    assert_eq!(record.status, "verification_failed");
    let evidence = record.verification.as_ref().expect("failure evidence");
    assert_eq!(evidence.configured_provider, "internal");
    assert_eq!(evidence.sandbox_profile, "internal");
    assert!(evidence
        .error
        .as_deref()
        .unwrap()
        .contains("terminal backend"));
    let session = store.load("parent-backend").expect("session");
    let verification = session
        .tool_calls
        .iter()
        .find(|call| call.tool_name.as_deref() == Some("run_terminal"))
        .expect("verification audit");
    assert_eq!(verification.provider.as_deref(), Some("hybrid"));
    assert_eq!(verification.sandbox_profile.as_deref(), Some("restricted"));
}

#[tokio::test]
async fn merge_structurally_excludes_child_nib_state_and_respects_other_ignores() {
    let root = git_repository_without_ignore();
    let worktree = Worktree::create(root.path(), "sub-secret").expect("worktree");
    let sentinel = "NIB_SECRET_SENTINEL_DO_NOT_COMMIT";
    let ignored_sentinel = "IGNORED_SECRET_SENTINEL_DO_NOT_COMMIT";
    std::fs::create_dir_all(worktree.path.join(".nib")).expect("runtime state");
    std::fs::write(worktree.path.join(".nib/config.toml"), sentinel).expect("secret config");
    std::fs::write(worktree.path.join(".gitignore"), "ignored-secret.txt\n").expect("child ignore");
    std::fs::write(worktree.path.join("ignored-secret.txt"), ignored_sentinel)
        .expect("ignored secret");
    std::fs::write(worktree.path.join("result.txt"), "delegated\n").expect("result");
    git(&worktree.path, &["add", "-f", ".nib/config.toml"]);

    let hook_directory = tempdir().expect("hook directory");
    let hooks = hook_directory.path();
    let hook_marker = hook_directory.path().join("hook-fired");
    for hook in ["pre-commit", "commit-msg", "pre-merge-commit", "post-merge"] {
        let path = hooks.join(hook);
        std::fs::write(
            &path,
            format!("#!/bin/sh\nprintf fired > '{}'\n", hook_marker.display()),
        )
        .expect("hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("hook permissions");
        }
    }
    git(
        root.path(),
        &[
            "config",
            "core.hooksPath",
            hooks.to_str().expect("hook path"),
        ],
    );
    git(root.path(), &["config", "commit.gpgSign", "true"]);
    completed_record(root.path(), "sub-secret", &worktree);
    let store = SessionStore::for_project(root.path()).expect("session store");
    store.create_session_with_id("parent-secret");
    let mut executor = merge_executor(root.path(), store);

    let merged = execute_merge(
        &mut executor,
        root.path(),
        "parent-secret",
        "sub-secret",
        "test -f result.txt",
    )
    .await;

    assert!(merged.success, "{:?}", merged.error);
    let tree = git_stdout(root.path(), &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(!tree
        .lines()
        .any(|path| path == ".nib" || path.starts_with(".nib/")));
    assert!(!tree.lines().any(|path| path == "ignored-secret.txt"));
    let committed = git_stdout(root.path(), &["show", "HEAD:result.txt"]);
    assert_eq!(committed, "delegated");
    assert!(
        !hook_marker.exists(),
        "Git hooks must be disabled for integration"
    );
    let patch = git_stdout(root.path(), &["show", "--format=", "HEAD"]);
    assert!(!patch.contains(sentinel));
    assert!(!patch.contains(ignored_sentinel));
}

#[tokio::test]
async fn dirty_parent_is_preserved_and_retry_succeeds_after_user_cleans_it() {
    let root = git_repository();
    let worktree = Worktree::create(root.path(), "sub-dirty").expect("worktree");
    std::fs::write(worktree.path.join("result.txt"), "delegated\n").expect("result");
    completed_record(root.path(), "sub-dirty", &worktree);
    let store = SessionStore::for_project(root.path()).expect("session store");
    store.create_session_with_id("parent-dirty");
    let mut executor = merge_executor(root.path(), store);
    let head = git_stdout(root.path(), &["rev-parse", "HEAD"]);
    std::fs::write(root.path().join("user-draft.txt"), "preserve me\n").expect("dirty file");

    let rejected = execute_merge(
        &mut executor,
        root.path(),
        "parent-dirty",
        "sub-dirty",
        "test -f result.txt",
    )
    .await;

    assert!(!rejected.success);
    assert!(rejected.error.as_deref().unwrap().contains("must be clean"));
    assert_eq!(git_stdout(root.path(), &["rev-parse", "HEAD"]), head);
    assert_eq!(
        std::fs::read_to_string(root.path().join("user-draft.txt")).unwrap(),
        "preserve me\n"
    );
    assert_eq!(
        get_subagent_record(root.path(), "sub-dirty")
            .unwrap()
            .status,
        "merge_failed"
    );

    std::fs::remove_file(root.path().join("user-draft.txt")).expect("clean parent");
    let retried = execute_merge(
        &mut executor,
        root.path(),
        "parent-dirty",
        "sub-dirty",
        "test -f result.txt",
    )
    .await;
    assert!(retried.success, "{:?}", retried.error);
}

#[tokio::test]
async fn conflicting_merge_aborts_cleanly_and_remains_retryable() {
    let root = git_repository();
    let worktree = Worktree::create(root.path(), "sub-conflict").expect("worktree");
    std::fs::write(worktree.path.join("README.md"), "child version\n").expect("child change");
    completed_record(root.path(), "sub-conflict", &worktree);
    std::fs::write(root.path().join("README.md"), "parent version\n").expect("parent change");
    git(root.path(), &["add", "README.md"]);
    git(root.path(), &["commit", "-qm", "parent change"]);
    let premerge_head = git_stdout(root.path(), &["rev-parse", "HEAD"]);
    let store = SessionStore::for_project(root.path()).expect("session store");
    store.create_session_with_id("parent-conflict");
    let mut executor = merge_executor(root.path(), store);
    let verification = "test \"$(cat README.md)\" = \"child version\"";

    let conflicted = execute_merge(
        &mut executor,
        root.path(),
        "parent-conflict",
        "sub-conflict",
        verification,
    )
    .await;

    assert!(!conflicted.success);
    assert!(conflicted
        .error
        .as_deref()
        .unwrap()
        .contains("retry is allowed"));
    assert_eq!(
        git_stdout(root.path(), &["rev-parse", "HEAD"]),
        premerge_head
    );
    assert!(git_stdout(root.path(), &["status", "--porcelain"]).is_empty());
    let merge_head = Command::new("git")
        .current_dir(root.path())
        .args(["rev-parse", "-q", "--verify", "MERGE_HEAD"])
        .output()
        .expect("merge head probe");
    assert!(!merge_head.status.success());
    let pending = get_subagent_record(root.path(), "sub-conflict").expect("pending record");
    assert_eq!(pending.status, "merge_pending");

    std::fs::write(root.path().join("README.md"), "child version\n").expect("resolve parent");
    git(root.path(), &["add", "README.md"]);
    git(root.path(), &["commit", "-qm", "align parent result"]);
    let retried = execute_merge(
        &mut executor,
        root.path(),
        "parent-conflict",
        "sub-conflict",
        verification,
    )
    .await;
    assert!(retried.success, "{:?}", retried.error);
    assert_eq!(
        get_subagent_record(root.path(), "sub-conflict")
            .unwrap()
            .status,
        "merged"
    );
}

#[tokio::test]
async fn pending_recovery_never_aborts_an_unrelated_human_merge() {
    let root = git_repository();
    let base = git_stdout(root.path(), &["rev-parse", "HEAD"]);
    let worktree = Worktree::create(root.path(), "sub-unrelated").expect("subagent worktree");
    std::fs::write(worktree.path.join("subagent.txt"), "delegated\n").expect("subagent result");
    git(&worktree.path, &["add", "subagent.txt"]);
    git(&worktree.path, &["commit", "-qm", "subagent fixture"]);
    let subagent_commit = git_stdout(&worktree.path, &["rev-parse", "HEAD"]);

    let human_parent = tempdir().expect("human worktree parent");
    let human_worktree = human_parent.path().join("worktree");
    git(
        root.path(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "human-merge",
            human_worktree.to_str().expect("human worktree path"),
            &base,
        ],
    );
    std::fs::write(human_worktree.join("README.md"), "human version\n").expect("human change");
    git(&human_worktree, &["add", "README.md"]);
    git(&human_worktree, &["commit", "-qm", "human change"]);
    let human_commit = git_stdout(&human_worktree, &["rev-parse", "HEAD"]);
    git(
        root.path(),
        &[
            "worktree",
            "remove",
            "--force",
            human_worktree.to_str().expect("human worktree path"),
        ],
    );
    std::fs::write(root.path().join("README.md"), "parent version\n").expect("parent change");
    git(root.path(), &["add", "README.md"]);
    git(root.path(), &["commit", "-qm", "parent change"]);
    let parent_head = git_stdout(root.path(), &["rev-parse", "HEAD"]);
    let human_merge = Command::new("git")
        .current_dir(root.path())
        .args(["merge", "--no-edit", "human-merge"])
        .output()
        .expect("human merge starts");
    assert!(!human_merge.status.success(), "human merge must conflict");
    assert_eq!(
        git_stdout(root.path(), &["rev-parse", "MERGE_HEAD"]),
        human_commit
    );
    let status_before = git_stdout(root.path(), &["status", "--porcelain=v1"]);

    write_subagent_record(
        root.path(),
        &SubagentRecord {
            id: "sub-unrelated".to_string(),
            parent_session_id: Some("parent-unrelated".to_string()),
            child_session_id: "child-unrelated".to_string(),
            prompt: "fixture".to_string(),
            status: "merge_pending".to_string(),
            execution_generation: None,
            owner_lease: None,
            worktree_path: worktree.path.clone(),
            branch: worktree.branch.clone(),
            branch_oid: Some(subagent_commit.clone()),
            result: Some(json!({
                "subagent_result": {"summary": "done"},
                "verification_command": "test -f subagent.txt",
                "merge_commit": subagent_commit,
                "parent_head_before": base,
                "active_merge_base": parent_head,
                "merge_stdout": null,
            })),
            error: Some("interrupted merge".to_string()),
            verification: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        },
    )
    .expect("pending record");
    let store = SessionStore::for_project(root.path()).expect("session store");
    store.create_session_with_id("parent-unrelated");
    let mut executor = merge_executor(root.path(), store);

    let result = execute_merge(
        &mut executor,
        root.path(),
        "parent-unrelated",
        "sub-unrelated",
        "test -f subagent.txt",
    )
    .await;

    assert!(!result.success);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("unrelated merge untouched")),
        "{:?}",
        result.error
    );
    assert_eq!(
        git_stdout(root.path(), &["rev-parse", "MERGE_HEAD"]),
        human_commit
    );
    assert_eq!(
        git_stdout(root.path(), &["status", "--porcelain=v1"]),
        status_before
    );
}

#[tokio::test]
async fn pending_merge_recovers_after_commit_and_cleanup_preceded_final_write() {
    let root = git_repository();
    let worktree = Worktree::create(root.path(), "sub-recovery").expect("worktree");
    std::fs::write(worktree.path.join("recovered.txt"), "integrated\n").expect("result");
    git(&worktree.path, &["add", "recovered.txt"]);
    git(
        &worktree.path,
        &["commit", "-qm", "subagent recovery fixture"],
    );
    let merge_commit = git_stdout(&worktree.path, &["rev-parse", "HEAD"]);
    let parent_head = git_stdout(root.path(), &["rev-parse", "HEAD"]);
    git(root.path(), &["merge", "--no-edit", &merge_commit]);
    Worktree::adopt_branch_revision(root.path(), "sub-recovery", &merge_commit)
        .expect("adopt committed branch revision");
    Worktree::remove(root.path(), "sub-recovery").expect("cleanup before final write");

    let mut record = SubagentRecord {
        id: "sub-recovery".to_string(),
        parent_session_id: Some("parent-recovery".to_string()),
        child_session_id: "child-recovery".to_string(),
        prompt: "fixture".to_string(),
        status: "merge_pending".to_string(),
        execution_generation: None,
        owner_lease: None,
        worktree_path: worktree.path.clone(),
        branch: worktree.branch.clone(),
        branch_oid: Some(merge_commit.clone()),
        result: Some(json!({
            "subagent_result": {"summary": "done"},
            "verification_command": "test -f recovered.txt",
            "merge_commit": merge_commit,
            "parent_head_before": parent_head,
            "merge_stdout": null,
        })),
        error: None,
        verification: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    write_subagent_record(root.path(), &record).expect("pending record");
    let store = SessionStore::for_project(root.path()).expect("session store");
    store.create_session_with_id("parent-recovery");
    let mut executor = merge_executor(root.path(), store);

    let recovered = execute_merge(
        &mut executor,
        root.path(),
        "parent-recovery",
        "sub-recovery",
        "test -f recovered.txt",
    )
    .await;

    assert!(recovered.success, "{:?}", recovered.error);
    record = get_subagent_record(root.path(), "sub-recovery").expect("reconciled record");
    assert_eq!(record.status, "merged");
    assert!(record.result.unwrap()["merge_stdout"]
        .as_str()
        .unwrap()
        .contains("already integrated"));
}

#[tokio::test]
async fn integrated_pending_commit_ignores_stale_worktree_for_verification_then_cleans_it() {
    let root = git_repository();
    let worktree = Worktree::create(root.path(), "sub-partial-cleanup").expect("worktree");
    std::fs::write(worktree.path.join("partial.txt"), "integrated\n").expect("result");
    git(&worktree.path, &["add", "partial.txt"]);
    git(
        &worktree.path,
        &["commit", "-qm", "partial cleanup fixture"],
    );
    let merge_commit = git_stdout(&worktree.path, &["rev-parse", "HEAD"]);
    let parent_head = git_stdout(root.path(), &["rev-parse", "HEAD"]);
    git(root.path(), &["merge", "--no-edit", &merge_commit]);
    std::fs::write(worktree.path.join("leftover-only.txt"), "stale\n")
        .expect("stale worktree mutation");
    write_subagent_record(
        root.path(),
        &SubagentRecord {
            id: "sub-partial-cleanup".to_string(),
            parent_session_id: Some("parent-partial-cleanup".to_string()),
            child_session_id: "child-partial-cleanup".to_string(),
            prompt: "fixture".to_string(),
            status: "merge_pending".to_string(),
            execution_generation: None,
            owner_lease: None,
            worktree_path: worktree.path.clone(),
            branch: worktree.branch.clone(),
            branch_oid: Some(merge_commit.clone()),
            result: Some(json!({
                "subagent_result": {"summary": "done"},
                "verification_command": "test -f partial.txt && test ! -f leftover-only.txt",
                "merge_commit": merge_commit,
                "parent_head_before": parent_head,
                "active_merge_base": null,
                "merge_stdout": null,
            })),
            error: Some("cleanup was interrupted".to_string()),
            verification: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        },
    )
    .expect("pending record");
    let store = SessionStore::for_project(root.path()).expect("session store");
    store.create_session_with_id("parent-partial-cleanup");
    let mut executor = merge_executor(root.path(), store);

    let result = execute_merge(
        &mut executor,
        root.path(),
        "parent-partial-cleanup",
        "sub-partial-cleanup",
        "test -f partial.txt && test ! -f leftover-only.txt",
    )
    .await;

    assert!(result.success, "{:?}", result.error);
    assert!(!worktree.path.exists());
    let record = get_subagent_record(root.path(), "sub-partial-cleanup").expect("merged record");
    assert_eq!(record.status, "merged");
    let canonical_root = root.path().canonicalize().expect("canonical parent root");
    assert_eq!(
        record
            .verification
            .as_ref()
            .expect("parent verification evidence")
            .worktree_path,
        canonical_root
    );
}

#[tokio::test]
async fn missing_pending_worktree_fails_closed_until_commit_is_integrated() {
    let root = git_repository();
    let worktree = Worktree::create(root.path(), "sub-missing").expect("worktree");
    std::fs::write(worktree.path.join("missing.txt"), "not integrated\n").expect("result");
    git(&worktree.path, &["add", "missing.txt"]);
    git(
        &worktree.path,
        &["commit", "-qm", "missing worktree fixture"],
    );
    let merge_commit = git_stdout(&worktree.path, &["rev-parse", "HEAD"]);
    let parent_head = git_stdout(root.path(), &["rev-parse", "HEAD"]);
    Worktree::adopt_branch_revision(root.path(), "sub-missing", &merge_commit)
        .expect("adopt committed branch revision");
    Worktree::remove(root.path(), "sub-missing").expect("remove unintegrated worktree");
    write_subagent_record(
        root.path(),
        &SubagentRecord {
            id: "sub-missing".to_string(),
            parent_session_id: Some("parent-missing".to_string()),
            child_session_id: "child-missing".to_string(),
            prompt: "fixture".to_string(),
            status: "merge_pending".to_string(),
            execution_generation: None,
            owner_lease: None,
            worktree_path: worktree.path.clone(),
            branch: worktree.branch.clone(),
            branch_oid: Some(merge_commit.clone()),
            result: Some(json!({
                "subagent_result": {"summary": "done"},
                "verification_command": "true",
                "merge_commit": merge_commit,
                "parent_head_before": parent_head,
                "active_merge_base": null,
                "merge_stdout": null,
            })),
            error: None,
            verification: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        },
    )
    .expect("pending record");
    let store = SessionStore::for_project(root.path()).expect("session store");
    store.create_session_with_id("parent-missing");
    let mut executor = merge_executor(root.path(), store);

    let result = execute_merge(
        &mut executor,
        root.path(),
        "parent-missing",
        "sub-missing",
        "true",
    )
    .await;

    assert!(!result.success);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("not integrated") && error.contains("unavailable")),
        "{:?}",
        result.error
    );
    assert_eq!(git_stdout(root.path(), &["rev-parse", "HEAD"]), parent_head);
    assert!(!root.path().join("missing.txt").exists());
    assert_eq!(
        get_subagent_record(root.path(), "sub-missing")
            .expect("pending record remains")
            .status,
        "merge_pending"
    );
}

#[tokio::test]
async fn branch_cleanup_failure_retains_merge_evidence_and_retries_after_lock_release() {
    let root = git_repository();
    let worktree = Worktree::create(root.path(), "sub-cleanup").expect("worktree");
    std::fs::write(worktree.path.join("cleanup.txt"), "integrated\n").expect("result");
    git(&worktree.path, &["add", "cleanup.txt"]);
    git(&worktree.path, &["commit", "-qm", "cleanup fixture"]);
    completed_record(root.path(), "sub-cleanup", &worktree);
    let branch_lock = root
        .path()
        .join(".git/refs/heads/nib/subagent/sub-cleanup.lock");
    std::fs::write(&branch_lock, b"locked\n").expect("branch lock");
    let store = SessionStore::for_project(root.path()).expect("session store");
    store.create_session_with_id("parent-cleanup");
    let mut executor = merge_executor(root.path(), store);

    let failed = execute_merge(
        &mut executor,
        root.path(),
        "parent-cleanup",
        "sub-cleanup",
        "test -f cleanup.txt",
    )
    .await;

    assert!(!failed.success);
    assert!(root.path().join("cleanup.txt").is_file());
    assert!(!worktree.path.exists());
    let pending = get_subagent_record(root.path(), "sub-cleanup").expect("pending record");
    assert_eq!(pending.status, "merge_pending");
    assert!(
        pending.error.as_deref().is_some_and(|error| error
            .contains("worktree cleanup failed after merge")
            && error.contains("sub-cleanup.lock")),
        "{:?}",
        pending.error
    );
    assert!(pending.result.as_ref().unwrap()["merge_stdout"]
        .as_str()
        .is_some_and(|stdout| !stdout.is_empty()));

    std::fs::remove_file(&branch_lock).expect("release branch lock");
    let retried = execute_merge(
        &mut executor,
        root.path(),
        "parent-cleanup",
        "sub-cleanup",
        "test -f cleanup.txt",
    )
    .await;

    assert!(retried.success, "{:?}", retried.error);
    assert_eq!(
        get_subagent_record(root.path(), "sub-cleanup")
            .expect("merged record")
            .status,
        "merged"
    );
}

#[tokio::test]
async fn merges_for_different_subagent_ids_share_one_repository_lock() {
    let root = git_repository();
    let first_worktree = Worktree::create(root.path(), "sub-lock-a").expect("first worktree");
    let second_worktree = Worktree::create(root.path(), "sub-lock-b").expect("second worktree");
    std::fs::write(first_worktree.path.join("lock-a.txt"), "first\n").expect("first result");
    std::fs::write(second_worktree.path.join("lock-b.txt"), "second\n").expect("second result");
    completed_record(root.path(), "sub-lock-a", &first_worktree);
    completed_record(root.path(), "sub-lock-b", &second_worktree);
    let merge_lock_path = root.path().join(".nib/subagents/.merge.lock");
    let merge_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(merge_lock_path)
        .expect("repository merge lock");
    merge_lock.lock().expect("hold repository merge lock");
    let store = SessionStore::for_project(root.path()).expect("session store");
    store.create_session_with_id("parent-lock-a");
    store.create_session_with_id("parent-lock-b");
    let mut first_executor = merge_executor(root.path(), store.clone());
    let mut second_executor = merge_executor(root.path(), store.clone());
    let mut merges = Box::pin(async {
        tokio::join!(
            execute_merge(
                &mut first_executor,
                root.path(),
                "parent-lock-a",
                "sub-lock-a",
                "test -f lock-a.txt",
            ),
            execute_merge(
                &mut second_executor,
                root.path(),
                "parent-lock-b",
                "sub-lock-b",
                "test -f lock-b.txt",
            )
        )
    });

    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        results = &mut merges => panic!("merges bypassed repository lock: {results:?}"),
    }
    assert!(!root.path().join("lock-a.txt").exists());
    assert!(!root.path().join("lock-b.txt").exists());

    drop(merge_lock);
    let (first, second) = tokio::time::timeout(Duration::from_secs(90), merges)
        .await
        .expect("serialized merges must remain bounded");
    assert!(first.success, "{:?}", first.error);
    assert!(second.success, "{:?}", second.error);
    for id in ["sub-lock-a", "sub-lock-b"] {
        let record = get_subagent_record(root.path(), id).expect("merged record");
        assert_eq!(record.status, "merged", "unexpected record for {id}");
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join("lock-a.txt")).unwrap(),
        "first\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("lock-b.txt")).unwrap(),
        "second\n"
    );
}

#[tokio::test]
async fn cancelled_repository_lock_wait_is_prompt_and_preserves_record() {
    let root = git_repository();
    let worktree =
        Worktree::create(root.path(), "sub-lock-cancel").expect("cancelled lock worktree");
    std::fs::write(worktree.path.join("cancelled.txt"), "child\n").expect("child result");
    completed_record(root.path(), "sub-lock-cancel", &worktree);
    let record_path = root.path().join(".nib/subagents/sub-lock-cancel.json");
    let record_before = std::fs::read(&record_path).expect("record before cancellation");

    let merge_lock_path = root.path().join(".nib/subagents/.merge.lock");
    let merge_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(merge_lock_path)
        .expect("repository merge lock");
    merge_lock.lock().expect("hold repository merge lock");

    let store = SessionStore::for_project(root.path()).expect("session store");
    store.create_session_with_id("parent-lock-cancel");
    let cancellation = CancellationSignal::new();
    let mut executor = merge_executor(root.path(), store).with_cancellation(cancellation.clone());
    let mut pending_merge = Box::pin(execute_merge(
        &mut executor,
        root.path(),
        "parent-lock-cancel",
        "sub-lock-cancel",
        "test -f cancelled.txt",
    ));

    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        result = &mut pending_merge => panic!("merge bypassed held lock: {result:?}"),
    }
    assert!(cancellation.cancel());
    let result = tokio::time::timeout(Duration::from_secs(5), pending_merge)
        .await
        .expect("cancelled lock wait must return promptly");
    assert!(!result.success);
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("repository merge lock acquisition was cancelled")),
        "{:?}",
        result.error
    );
    assert_eq!(
        std::fs::read(&record_path).expect("record after cancellation"),
        record_before,
        "cancelled lock acquisition mutated the authoritative record"
    );
    assert!(worktree.path.exists());
    assert!(!root.path().join("cancelled.txt").exists());

    drop(merge_lock);
    let retry_store = SessionStore::for_project(root.path()).expect("retry session store");
    retry_store.create_session_with_id("parent-lock-cancel-retry");
    let mut retry_executor = merge_executor(root.path(), retry_store);
    let retried = execute_merge(
        &mut retry_executor,
        root.path(),
        "parent-lock-cancel-retry",
        "sub-lock-cancel",
        "test -f cancelled.txt",
    )
    .await;
    assert!(retried.success, "{:?}", retried.error);
    assert_eq!(
        std::fs::read_to_string(root.path().join("cancelled.txt")).expect("merged artifact"),
        "child\n"
    );
    let record = get_subagent_record(root.path(), "sub-lock-cancel").expect("merged record");
    assert_eq!(record.status, "merged");
}

#[cfg(all(unix, debug_assertions))]
#[tokio::test]
async fn cancelled_active_merge_preserves_user_changes_and_retries_owned_state() {
    let root = git_repository();
    let worktree = Worktree::create(root.path(), "sub-cancel-merge").expect("worktree");
    std::fs::write(worktree.path.join("child-result.txt"), "child version\n")
        .expect("child change");
    git(&worktree.path, &["add", "child-result.txt"]);
    git(&worktree.path, &["commit", "-qm", "child merge fixture"]);
    let child_commit = git_stdout(&worktree.path, &["rev-parse", "HEAD"]);
    completed_record(root.path(), "sub-cancel-merge", &worktree);
    std::fs::write(root.path().join("parent-result.txt"), "parent version\n")
        .expect("parent change");
    git(root.path(), &["add", "parent-result.txt"]);
    git(root.path(), &["commit", "-qm", "parent merge fixture"]);
    let premerge_head = git_stdout(root.path(), &["rev-parse", "HEAD"]);

    let interruption = install_merge_interruption_test_barrier(root.path(), "sub-cancel-merge")
        .expect("merge interruption barrier");
    let store = SessionStore::for_project(root.path()).expect("session store");
    store.create_session_with_id("parent-cancel-merge");
    let mut executor = merge_executor(root.path(), store.clone());

    {
        let mut active_merge = Box::pin(execute_merge(
            &mut executor,
            root.path(),
            "parent-cancel-merge",
            "sub-cancel-merge",
            "test \"$(cat child-result.txt)\" = \"child version\"",
        ));
        tokio::select! {
            result = &mut active_merge => panic!("merge completed before cancellation: {result:?}"),
            reached = tokio::time::timeout(
                Duration::from_secs(10),
                interruption.wait_until_interrupted(),
            ) => reached.expect("merge reaches interruption barrier").expect("merge interruption fixture"),
        }
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    let interrupted =
        get_subagent_record(root.path(), "sub-cancel-merge").expect("interrupted merge record");
    assert_eq!(interrupted.status, "merge_pending");
    assert_eq!(
        interrupted.result.as_ref().unwrap()["active_merge_base"],
        premerge_head
    );
    assert_eq!(
        git_stdout(root.path(), &["rev-parse", "MERGE_HEAD"]),
        child_commit
    );
    let index_lock = root.path().join(".git/index.lock");
    assert!(
        index_lock.is_file(),
        "cancelled Git merge left its index lock"
    );

    std::fs::write(root.path().join("user-draft.txt"), "preserve me\n").expect("user draft");
    let mut blocked_executor = merge_executor(root.path(), store.clone());
    let blocked = execute_merge(
        &mut blocked_executor,
        root.path(),
        "parent-cancel-merge",
        "sub-cancel-merge",
        "test \"$(cat child-result.txt)\" = \"child version\"",
    )
    .await;
    assert!(!blocked.success);
    assert!(
        blocked.error.as_deref().is_some_and(
            |error| error.contains("not proven") && error.contains("refusing to abort")
        ),
        "{:?}",
        blocked.error
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("user-draft.txt")).unwrap(),
        "preserve me\n"
    );
    assert_eq!(
        git_stdout(root.path(), &["rev-parse", "MERGE_HEAD"]),
        child_commit
    );

    std::fs::remove_file(root.path().join("user-draft.txt")).expect("remove user draft");
    let mut locked_executor = merge_executor(root.path(), store.clone());
    let locked = execute_merge(
        &mut locked_executor,
        root.path(),
        "parent-cancel-merge",
        "sub-cancel-merge",
        "test \"$(cat child-result.txt)\" = \"child version\"",
    )
    .await;
    assert!(!locked.success);
    assert!(
        locked
            .error
            .as_deref()
            .is_some_and(|error| error.contains("did not remove Git index.lock")
                && error.contains("remove the stale lock manually")),
        "{:?}",
        locked.error
    );
    assert!(
        index_lock.is_file(),
        "nib must preserve an ambiguous index lock"
    );
    assert_eq!(
        git_stdout(root.path(), &["rev-parse", "MERGE_HEAD"]),
        child_commit
    );

    std::fs::remove_file(&index_lock).expect("manual stale index lock cleanup");
    let mut retry_executor = merge_executor(root.path(), store);
    let retried = execute_merge(
        &mut retry_executor,
        root.path(),
        "parent-cancel-merge",
        "sub-cancel-merge",
        "test \"$(cat child-result.txt)\" = \"child version\"",
    )
    .await;

    assert!(retried.success, "{:?}", retried.error);
    assert_eq!(
        get_subagent_record(root.path(), "sub-cancel-merge")
            .expect("merged record")
            .status,
        "merged"
    );
    let merge_head = Command::new("git")
        .current_dir(root.path())
        .args(["rev-parse", "-q", "--verify", "MERGE_HEAD"])
        .output()
        .expect("merge head probe");
    assert!(!merge_head.status.success());
    assert_eq!(
        std::fs::read_to_string(root.path().join("child-result.txt")).unwrap(),
        "child version\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("parent-result.txt")).unwrap(),
        "parent version\n"
    );
}
