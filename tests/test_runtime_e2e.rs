use async_trait::async_trait;
use chrono::Utc;
use nib::agent::r#loop::{run_agent_loop, AgentLoopConfig};
use nib::agent::state::AgentState;
use nib::config::{
    save_nib_config_full, DaemonsConfig, ExecutionConfig, LlmConfig, McpServerEntry, NibConfig,
    ProfileConfig, ProfilesConfig, ProviderEntry,
};
use nib::context::assemble_context;
use nib::context::compression::{maybe_compress_session, CompressionReport};
use nib::context::format_context_for_prompt;
use nib::context::skills::load_relevant_skills;
use nib::integrations::mcp::McpManager;
use nib::llm::{LlmClient, LlmResponse, StreamEvent};
use nib::profile::ProfileRegistry;
use nib::session::memory::MemoryStore;
use nib::session::{Plan, PlanStep, SessionError, SessionStore};
#[cfg(target_os = "linux")]
use nib::tools::delegation::get_subagent_record;
use nib::tools::delegation::{write_subagent_record, SubagentRecord};
use nib::tools::executor::ApprovalHandler;
use nib::tools::models::{ApprovalDecision, PermissionLevel, ToolCall};
use nib::tools::ToolExecutor;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use tempfile::{tempdir, TempDir};

fn mock_llm_config(context_length: usize) -> LlmConfig {
    LlmConfig {
        active_provider: Some("mock".to_string()),
        providers: HashMap::from([(
            "mock".to_string(),
            ProviderEntry {
                model: "mock-model".to_string(),
                api_key: None,
                api_keys: Vec::new(),
                base_url: None,
            },
        )]),
        context_length,
    }
}

fn mock_runtime_config() -> NibConfig {
    NibConfig {
        llm: mock_llm_config(128_000),
        daemons: DaemonsConfig {
            cron_enabled: false,
            curator_enabled: false,
            ..Default::default()
        },
        ..Default::default()
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
        &["config", "user.email", "nib-e2e@example.invalid"],
    );
    git(directory.path(), &["config", "user.name", "nib e2e"]);
    git(directory.path(), &["config", "core.autocrlf", "false"]);
    std::fs::write(directory.path().join(".gitignore"), ".nib/\n").expect("gitignore");
    std::fs::write(
        directory.path().join("AGENTS.md"),
        "Runtime fixture rule: preserve the physical verification record.\n",
    )
    .expect("agents fixture");
    std::fs::write(directory.path().join("note.txt"), "old\n").expect("note fixture");
    std::fs::create_dir_all(directory.path().join("src")).expect("source directory");
    std::fs::write(
        directory.path().join("Cargo.toml"),
        "[package]\nname = \"nib-runtime-e2e-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("cargo fixture");
    std::fs::write(
        directory.path().join("src/lib.rs"),
        "pub fn answer() -> u32 {\n    41\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn answer_is_verified() {\n        assert_eq!(answer(), 42);\n    }\n}\n",
    )
    .expect("rust fixture");
    git(
        directory.path(),
        &[
            "add",
            ".gitignore",
            "AGENTS.md",
            "Cargo.toml",
            "note.txt",
            "src/lib.rs",
        ],
    );
    git(directory.path(), &["commit", "-qm", "initial fixture"]);
    directory
}

fn tool_call(root: &Path, name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        tool_name: name.to_string(),
        arguments,
        session_id: None,
        project_root: Some(root.to_path_buf()),
    }
}

fn assert_trace_contains_in_order(trace: &[String], expected: &[&str]) {
    let mut cursor = 0;
    for state in trace {
        if cursor < expected.len() && state == expected[cursor] {
            cursor += 1;
        }
    }
    assert_eq!(
        cursor,
        expected.len(),
        "trace did not contain {expected:?} in order: {trace:?}"
    );
}

#[tokio::test]
async fn runtime_sequence_selects_profile_context_and_skill_then_reconciles_audited_tools() {
    let root = git_repository();
    std::fs::write(
        root.path().join(".env.nib"),
        "NIB_PROFILE_FACT=profile-value\n",
    )
    .expect("profile env");
    let skill_directory = root.path().join(".nib/skills/runtime-proof");
    std::fs::create_dir_all(&skill_directory).expect("skill directory");
    std::fs::write(
        skill_directory.join("SKILL.md"),
        r#"---
name: runtime-proof
description: Verify runtime profile facts
tags: [explore, runtime]
hooks:
  after_tool:
    - tool: list_directory
      command: 'test "$NIB_PROFILE_FACT" = profile-value'
---
Observe the approved plan and verify each tool result.
"#,
    )
    .expect("skill fixture");

    let mut config = mock_runtime_config();
    config.profiles = ProfilesConfig {
        default: "workspace".to_string(),
        active: vec![ProfileConfig {
            id: "workspace".to_string(),
            root: ".".into(),
            env_file: Some(".env.nib".into()),
            active_skills: vec!["runtime-proof".to_string()],
            skill_paths: vec![".nib/skills".into()],
            state_dir: Some(".nib/profiles/workspace".into()),
        }],
    };
    save_nib_config_full(root.path(), &mut config).expect("runtime config");

    let profiles = ProfileRegistry::load(root.path(), &config.profiles).expect("profiles");
    let profile = profiles.default_profile();
    profile.ensure_state_dirs().expect("profile state");
    profile
        .memory_store()
        .set_environment("verification_command", "task test")
        .expect("environment memory");
    profile
        .memory_store()
        .set_user("response_style", "concise")
        .expect("user memory");

    let goal = "explore the runtime profile sequence";
    let agents_context = format_context_for_prompt(root.path(), Some(goal));
    assert!(agents_context.contains("Runtime fixture rule"));
    let skill_context = load_relevant_skills(root.path(), Some(goal));
    assert!(skill_context.contains("runtime-proof"));
    assert!(skill_context.contains("verify each tool result"));

    let store = SessionStore::for_project(root.path()).expect("profile session store");
    let session = store.create_session_with_id("sequence-session");
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(512);
    let summary = run_agent_loop(
        root.path().to_path_buf(),
        &session.id,
        goal,
        AgentLoopConfig {
            max_steps: 8,
            auto_approve: true,
            stream_tx: Some(stream_tx),
            ..Default::default()
        },
    )
    .await
    .expect("runtime completes");

    assert_eq!(summary.final_state, AgentState::Done);
    assert_eq!(summary.outcome, "completed");
    assert!(!summary.bound_reached);
    assert_eq!(summary.tool_call_count, 1);
    assert_trace_contains_in_order(
        &summary.trace,
        &[
            "idle",
            "planning",
            "plan_approval",
            "build_context",
            "compression",
            "inspect_llm",
            "update_memory",
            "user_approval",
            "tool_execute",
            "build_context",
            "compression",
            "inspect_llm",
            "update_memory",
            "reconciliation",
            "done",
        ],
    );

    let persisted = store.load(&session.id).expect("persisted runtime");
    persisted
        .validate_message_sequence()
        .expect("indexed role-safe transcript");
    assert_eq!(persisted.messages[0].role, "user");
    assert_eq!(persisted.messages[0].content, goal);
    assert!(persisted
        .messages
        .iter()
        .any(|message| message.role == "tool"));
    let plan = persisted.plan.as_ref().expect("persisted plan");
    assert!(plan.approved);
    assert!(plan.is_complete());
    assert_eq!(persisted.active_skills, ["runtime-proof"]);
    assert!(persisted.skill_usage.iter().any(|usage| {
        usage.skill_name == "runtime-proof"
            && usage.reason.as_deref() == Some("profile active skill")
    }));

    let hook_record = persisted
        .tool_calls
        .iter()
        .find(|record| record.tool_name.as_deref() == Some("run_terminal"))
        .expect("skill hook dispatch was audited");
    assert_eq!(hook_record.arguments["hook_source"], "runtime-proof");
    assert_eq!(hook_record.result.as_ref().unwrap()["success"], true);
    assert!(hook_record.result.as_ref().unwrap()["environment_keys"]
        .as_array()
        .unwrap()
        .iter()
        .any(|key| key == "NIB_PROFILE_FACT"));
    assert!(hook_record.worktree_path.is_some());

    let list_record = persisted
        .tool_calls
        .iter()
        .find(|record| record.tool_name.as_deref() == Some("list_directory"))
        .expect("model tool dispatch was audited");
    assert_eq!(list_record.result.as_ref().unwrap()["success"], true);
    assert_eq!(
        list_record.result.as_ref().unwrap()["output"]["post_hooks"][0]["success"],
        true
    );

    let kinds = persisted
        .events
        .iter()
        .map(|event| event.kind.as_str())
        .collect::<Vec<_>>();
    for required in [
        "plan_generated",
        "approval_required",
        "plan_approved",
        "tool_started",
        "tool_completed",
        "reconciliation",
    ] {
        assert!(
            kinds.contains(&required),
            "missing runtime event {required}"
        );
    }
    assert_eq!(
        persisted
            .events
            .iter()
            .rfind(|event| event.kind == "reconciliation")
            .unwrap()
            .details["outcome"],
        "completed"
    );

    let mut live_events = Vec::new();
    while let Ok(event) = stream_rx.try_recv() {
        live_events.push(event);
    }
    let started = live_events
        .iter()
        .position(|event| matches!(event, StreamEvent::ToolStarted { .. }))
        .expect("live tool start");
    let completed = live_events
        .iter()
        .position(|event| matches!(event, StreamEvent::ToolCompleted { .. }))
        .expect("live tool completion");
    let reconciled = live_events
        .iter()
        .rposition(|event| matches!(event, StreamEvent::Reconciled { .. }))
        .expect("live reconciliation");
    assert!(started < completed && completed < reconciled);

    let restarted_memory = profile.memory_store();
    assert_eq!(
        restarted_memory
            .environment("verification_command")
            .as_deref(),
        Some("task test")
    );
    assert_eq!(
        restarted_memory.user("response_style").as_deref(),
        Some("concise")
    );
}

#[tokio::test]
async fn full_agent_loop_compresses_edits_and_runs_real_cargo_tests_in_one_worktree() {
    let root = git_repository();
    let mut config = mock_runtime_config();
    config.llm.context_length = 16_000;
    config.compression.threshold = 0.05;
    config.compression.target_ratio = 0.02;
    save_nib_config_full(root.path(), &mut config).expect("runtime config");

    let store = SessionStore::for_project(root.path()).expect("session store");
    let session = store.create_session_with_id("full-coding-loop");
    for index in 0..6 {
        store
            .try_append_message(
                &session.id,
                "user",
                &format!(
                    "historic request {index}: {}",
                    "preserve verified runtime context ".repeat(48)
                ),
            )
            .expect("historic user message");
        store
            .try_append_message(
                &session.id,
                "assistant",
                &format!(
                    "historic response {index}: {}",
                    "record the exact coding outcome ".repeat(48)
                ),
            )
            .expect("historic assistant message");
    }
    let raw_history = store.load(&session.id).expect("historic session").messages;
    let goal = "runtime coding e2e: update answer and run cargo test";

    let summary = run_agent_loop(
        root.path().to_path_buf(),
        &session.id,
        goal,
        AgentLoopConfig {
            max_steps: 8,
            auto_approve: true,
            ..Default::default()
        },
    )
    .await
    .expect("full coding loop");

    assert_eq!(summary.final_state, AgentState::Done);
    assert_eq!(summary.outcome, "completed");
    assert_eq!(summary.tool_call_count, 2);
    assert_trace_contains_in_order(
        &summary.trace,
        &[
            "planning",
            "plan_approval",
            "compression",
            "inspect_llm",
            "tool_execute",
            "compression",
            "inspect_llm",
            "reconciliation",
            "done",
        ],
    );

    let persisted = store.load(&session.id).expect("persisted coding loop");
    assert_eq!(
        &persisted.messages[..raw_history.len()],
        raw_history.as_slice(),
        "compression must retain the raw transcript"
    );
    assert!(persisted.summary.is_some());
    assert!(persisted
        .events
        .iter()
        .any(|event| event.kind == "compression"));
    let plan = persisted.plan.as_ref().expect("persisted plan");
    assert!(plan.id.starts_with("plan-"));
    assert_eq!(plan.goal, goal);
    assert!(plan.is_complete());
    assert_eq!(
        persisted
            .events
            .iter()
            .rfind(|event| event.kind == "reconciliation")
            .expect("reconciliation audit")
            .details["outcome"],
        "completed"
    );

    let patch = persisted
        .tool_calls
        .iter()
        .find(|record| record.tool_name.as_deref() == Some("apply_patch"))
        .expect("patch audit");
    let cargo_test = persisted
        .tool_calls
        .iter()
        .find(|record| {
            record.tool_name.as_deref() == Some("run_terminal")
                && record.arguments["command"]
                    == "mkdir -p .tmp && TMPDIR=\"$PWD/.tmp\" cargo test --quiet"
        })
        .expect("cargo test audit");
    assert_eq!(patch.result.as_ref().unwrap()["success"], true);
    assert_eq!(
        cargo_test.result.as_ref().unwrap()["success"],
        true,
        "cargo test record: {cargo_test:#?}"
    );
    assert_eq!(patch.worktree_path, cargo_test.worktree_path);
    assert_eq!(patch.plan_id.as_deref(), Some(plan.id.as_str()));
    assert_eq!(cargo_test.plan_id.as_deref(), Some(plan.id.as_str()));

    let worktree = Path::new(patch.worktree_path.as_deref().expect("session worktree"));
    assert!(std::fs::read_to_string(worktree.join("src/lib.rs"))
        .expect("changed worktree source")
        .contains("    42"));
    assert!(std::fs::read_to_string(root.path().join("src/lib.rs"))
        .expect("unchanged main source")
        .contains("    41"));
}

struct PlanThenDenyTool {
    calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl ApprovalHandler for PlanThenDenyTool {
    async fn handle_approval(&self, call: &ToolCall, _level: PermissionLevel) -> ApprovalDecision {
        self.calls.lock().unwrap().push(call.tool_name.clone());
        if call.tool_name == "approve_plan" {
            ApprovalDecision::granted_user()
        } else {
            ApprovalDecision::denied()
        }
    }
}

#[tokio::test]
async fn selected_skill_can_require_tool_approval_and_denial_has_no_side_effect() {
    let root = tempdir().expect("tempdir");
    let skill_directory = root.path().join(".nib/skills/gated-read");
    std::fs::create_dir_all(&skill_directory).expect("skill directory");
    std::fs::write(
        skill_directory.join("SKILL.md"),
        r#"---
name: gated-read
description: Require explicit approval for repository exploration
tags: [explore]
constraints:
  require_approval_tools: [list_directory]
---
Do not inspect the repository without an explicit decision.
"#,
    )
    .expect("skill fixture");
    let mut config = mock_runtime_config();
    save_nib_config_full(root.path(), &mut config).expect("runtime config");
    let store = SessionStore::for_project(root.path()).expect("session store");
    let session = store.create_session_with_id("denied-session");
    let calls = Arc::new(Mutex::new(Vec::new()));

    let summary = run_agent_loop(
        root.path().to_path_buf(),
        &session.id,
        "explore the repository",
        AgentLoopConfig {
            max_steps: 5,
            approval_handler: Some(Arc::new(PlanThenDenyTool {
                calls: Arc::clone(&calls),
            })),
            ..Default::default()
        },
    )
    .await
    .expect("denied run reconciles");

    assert_eq!(summary.outcome, "tool_execution_failed");
    assert_eq!(summary.tool_call_count, 1);
    assert!(!summary.bound_reached);
    assert_eq!(
        *calls.lock().unwrap(),
        ["approve_plan".to_string(), "list_directory".to_string()]
    );
    assert!(!root.path().join(".nib/worktrees").exists());

    let persisted = store.load(&session.id).expect("denial audit");
    persisted
        .validate_message_sequence()
        .expect("valid transcript");
    assert_eq!(persisted.active_skills, ["gated-read"]);
    assert_eq!(persisted.tool_calls.len(), 1);
    let audit = &persisted.tool_calls[0];
    assert_eq!(audit.tool_name.as_deref(), Some("list_directory"));
    assert_eq!(audit.result.as_ref().unwrap()["success"], false);
    assert_eq!(
        audit.result.as_ref().unwrap()["approval"]["source"],
        "denied"
    );
    assert_eq!(persisted.plan.as_ref().unwrap().steps[0].status, "Blocked");
    assert_eq!(
        persisted
            .events
            .iter()
            .find(|event| event.kind == "reconciliation")
            .unwrap()
            .details["outcome"],
        "tool_execution_failed"
    );
}

#[tokio::test]
async fn approved_patch_physically_changes_only_the_session_worktree_and_is_verified() {
    let root = git_repository();
    let store = SessionStore::new(root.path());
    let mut session = store.create_session_with_id("physical-edit");
    let mut plan = Plan::new(
        "change and verify note.txt",
        vec![PlanStep {
            description: "change and verify note.txt".to_string(),
            status: "Pending".to_string(),
            outcome: None,
            attempts: 0,
            updated_at: None,
        }],
    );
    plan.approve();
    let expected_plan_id = plan.id.clone();
    session.plan = Some(plan);
    store.save(&mut session).expect("approved plan");

    let mut executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
        .with_auto_approve(true)
        .with_session_store(store.clone());
    let patch = "diff --git a/note.txt b/note.txt\n--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let applied = executor
        .execute(
            tool_call(
                root.path(),
                "apply_patch",
                json!({"patch": patch, "dry_run": false}),
            ),
            Some(&session.id),
        )
        .await;
    assert!(applied.success, "{:?}", applied.error);
    assert_eq!(applied.output.as_ref().unwrap()["applied"], true);

    let verified = executor
        .execute(
            tool_call(
                root.path(),
                "run_terminal",
                json!({"command": "cat note.txt"}),
            ),
            Some(&session.id),
        )
        .await;
    assert!(verified.success, "{:?}", verified.error);
    assert_eq!(verified.output.as_ref().unwrap()["stdout"], "new\n");
    assert_eq!(
        std::fs::read_to_string(root.path().join("note.txt")).expect("original note"),
        "old\n",
        "the main worktree must remain untouched"
    );

    let persisted = store.load(&session.id).expect("tool audit");
    assert_eq!(persisted.tool_calls.len(), 2);
    let edit_worktree = persisted.tool_calls[0]
        .worktree_path
        .as_deref()
        .expect("edit worktree");
    let verification_worktree = persisted.tool_calls[1]
        .worktree_path
        .as_deref()
        .expect("verification worktree");
    assert_eq!(edit_worktree, verification_worktree);
    assert_eq!(
        std::fs::read_to_string(Path::new(edit_worktree).join("note.txt")).expect("isolated note"),
        "new\n"
    );
    assert!(persisted.tool_calls.iter().all(|record| {
        record.plan_id.as_deref() == Some(expected_plan_id.as_str())
            && record.result.as_ref().unwrap()["success"] == true
    }));
    assert_eq!(
        persisted
            .events
            .iter()
            .filter(|event| event.kind == "tool_attempted")
            .count(),
        2
    );
}

#[derive(Default)]
struct RecordingCompressor {
    prompts: Mutex<Vec<Vec<Value>>>,
}

#[async_trait]
impl LlmClient for RecordingCompressor {
    async fn complete(
        &self,
        messages: &[Value],
        _tools: Option<&[Value]>,
        _temperature: f64,
    ) -> Result<LlmResponse, String> {
        self.prompts.lock().unwrap().push(messages.to_vec());
        Ok(LlmResponse::text(
            "Retain the verified path, decisions, and current progress.",
        ))
    }
}

#[tokio::test]
async fn compression_is_measured_audited_and_keeps_the_raw_transcript() {
    let root = tempdir().expect("tempdir");
    let store = SessionStore::new(root.path());
    let session = store.create_session_with_id("compression-session");
    for index in 0..8 {
        store
            .try_append_message(
                &session.id,
                "user",
                &format!("request {index}: {}", "historic user fact ".repeat(24)),
            )
            .expect("user history");
        store
            .try_append_message(
                &session.id,
                "assistant",
                &format!("response {index}: {}", "verified code progress ".repeat(24)),
            )
            .expect("assistant history");
    }
    let before = store.load(&session.id).expect("raw history");
    let raw_messages = before.messages.clone();

    let mut config = mock_runtime_config();
    config.llm.context_length = 400;
    config.compression.threshold = 0.50;
    config.compression.target_ratio = 0.20;
    let compressor = Arc::new(RecordingCompressor::default());
    let llm: Arc<dyn LlmClient> = compressor.clone();
    let report: CompressionReport = maybe_compress_session(&store, &session.id, &llm, &config)
        .await
        .expect("compression succeeds")
        .expect("threshold exceeded");

    assert!(report.before_tokens > 200);
    assert!(report.after_tokens < report.before_tokens);
    assert!(report.after_tokens <= report.target_tokens);
    assert_eq!(report.target_tokens, 80);
    assert!(report.summarized_through > report.summarized_from);

    let persisted = store.load(&session.id).expect("compressed session");
    assert_eq!(
        persisted.messages, raw_messages,
        "raw audit history is immutable"
    );
    assert!(persisted.summary.is_some());
    assert_eq!(
        persisted.summary_index,
        report.summarized_through + 1,
        "summary_index points at the first raw message after the inclusive summarized range"
    );
    let event = persisted
        .events
        .iter()
        .find(|event| event.kind == "compression")
        .expect("compression event");
    assert_eq!(event.details["before_tokens"], report.before_tokens);
    assert_eq!(event.details["after_tokens"], report.after_tokens);
    assert_eq!(event.details["target_tokens"], report.target_tokens);
    assert_eq!(event.details["raw_message_count"], raw_messages.len());

    let prompts = compressor.prompts.lock().unwrap();
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0][0]["content"]
        .as_str()
        .unwrap()
        .contains("context compression engine"));
    assert!(prompts[0][1]["content"]
        .as_str()
        .unwrap()
        .contains("message[0] user"));
}

#[test]
fn role_violation_is_rejected_while_memory_and_sessions_survive_restart() {
    let root = tempdir().expect("tempdir");
    let store = SessionStore::new(root.path());
    store.create_session_with_id("session-one");
    store
        .try_append_message("session-one", "user", "first prompt")
        .expect("first user");
    store
        .try_append_message("session-one", "assistant", "first answer")
        .expect("first assistant");
    store.create_session_with_id("session-two");
    store
        .try_append_message("session-two", "user", "second prompt")
        .expect("second user");
    store
        .try_append_message("session-two", "assistant", "second answer")
        .expect("second assistant");

    let violation = store
        .try_append_message("session-two", "assistant", "invalid duplicate role")
        .expect_err("consecutive assistant roles must be rejected");
    assert!(matches!(violation, SessionError::RoleViolation { .. }));
    let unchanged = store.append_message("session-two", "assistant", "audited violation");
    assert_eq!(unchanged.messages.len(), 2);
    assert_eq!(unchanged.events.last().unwrap().kind, "role_violation");

    let memory = MemoryStore::new(root.path());
    memory
        .set_environment("canonical_gate", "task check")
        .expect("environment memory");
    memory
        .set_user("review_style", "findings-first")
        .expect("user memory");

    let restarted_sessions = SessionStore::new(root.path());
    assert_eq!(
        restarted_sessions.list(),
        ["session-one".to_string(), "session-two".to_string()]
    );
    restarted_sessions
        .load("session-one")
        .unwrap()
        .validate_message_sequence()
        .expect("session one survives");
    restarted_sessions
        .load("session-two")
        .unwrap()
        .validate_message_sequence()
        .expect("session two survives");
    restarted_sessions
        .try_append_message("session-one", "user", "next persisted turn")
        .expect("cross-run continuation");

    let restarted_memory = MemoryStore::new(root.path());
    assert_eq!(
        restarted_memory.environment("canonical_gate").as_deref(),
        Some("task check")
    );
    assert_eq!(
        restarted_memory.user("review_style").as_deref(),
        Some("findings-first")
    );
    assert_eq!(restarted_memory.user("canonical_gate"), None);
    assert_eq!(restarted_memory.environment("review_style"), None);
}

#[tokio::test]
async fn configured_turn_bound_reconciles_without_dispatching_a_tool() {
    let root = tempdir().expect("tempdir");
    let mut config = mock_runtime_config();
    save_nib_config_full(root.path(), &mut config).expect("runtime config");
    let store = SessionStore::for_project(root.path()).expect("session store");
    let session = store.create_session_with_id("bounded-session");

    let summary = run_agent_loop(
        root.path().to_path_buf(),
        &session.id,
        "explore within one model turn",
        AgentLoopConfig {
            max_steps: 1,
            auto_approve: true,
            ..Default::default()
        },
    )
    .await
    .expect("bounded run reconciles");

    assert_eq!(summary.final_state, AgentState::Done);
    assert_eq!(summary.steps_taken, 1);
    assert_eq!(summary.outcome, "turn_limit_reached");
    assert!(summary.bound_reached);
    assert_eq!(summary.tool_call_count, 0);
    assert_trace_contains_in_order(
        &summary.trace,
        &[
            "idle",
            "planning",
            "plan_approval",
            "build_context",
            "compression",
            "reconciliation",
            "done",
        ],
    );
    let persisted = store.load(&session.id).expect("bounded audit");
    assert!(persisted.plan.as_ref().unwrap().approved);
    assert!(persisted.tool_calls.is_empty());
    assert_eq!(
        persisted
            .events
            .iter()
            .find(|event| event.kind == "reconciliation")
            .unwrap()
            .details["outcome"],
        "turn_limit_reached"
    );
}

#[tokio::test]
async fn assembled_context_and_scoped_core_tools_return_real_artifacts_and_errors() {
    let root = tempdir().expect("tempdir");
    std::fs::write(
        root.path().join("AGENTS.md"),
        "Context fixture rule: run the scoped checks.\n",
    )
    .expect("agents fixture");
    std::fs::write(root.path().join("README.md"), "needle outside rust\n").expect("readme fixture");
    std::fs::create_dir_all(root.path().join("docs/standards")).expect("standards directory");
    std::fs::write(
        root.path().join("docs/standards/runtime.md"),
        "Project documentation fixture: preserve library boundaries.\n",
    )
    .expect("project documentation fixture");
    std::fs::create_dir_all(root.path().join("src/nested")).expect("source fixture");
    std::fs::write(
        root.path().join("src/main.rs"),
        "alpha\nbeta\ngamma\ndelta\n",
    )
    .expect("main fixture");
    std::fs::write(
        root.path().join("src/nested/lib.rs"),
        "fn verified_needle() {}\n",
    )
    .expect("nested fixture");
    std::fs::write(root.path().join("src/.hidden"), "hidden fixture\n").expect("hidden fixture");

    let skill_directory = root.path().join(".nib/skills/context-proof");
    std::fs::create_dir_all(&skill_directory).expect("skill directory");
    std::fs::write(
        skill_directory.join("SKILL.md"),
        r#"---
name: context-proof
description: Inspect Rust context with scoped tools
tags: [rust, inspect]
---
Use bounded reads and report the concrete artifact path.
"#,
    )
    .expect("skill fixture");
    let memory = MemoryStore::new(root.path());
    memory
        .set_environment("test_gate", "task test")
        .expect("environment memory");
    memory
        .set_user("answer_style", "concise")
        .expect("user memory");

    let context = assemble_context(root.path(), Some("inspect the Rust context"));
    for expected in [
        "Context fixture rule",
        "Project Standards and Library Documentation",
        "preserve library boundaries",
        "Environment Facts",
        "test_gate: task test",
        "User Preferences",
        "answer_style: concise",
        "Skill: context-proof",
        "Use bounded reads",
    ] {
        assert!(
            context.contains(expected),
            "missing context fragment {expected}"
        );
    }

    let store = SessionStore::new(root.path());
    store.create_session_with_id("core-tools");
    let mut executor = ToolExecutor::new(
        root.path().to_path_buf(),
        ExecutionConfig {
            plan_mode: false,
            ..ExecutionConfig::default()
        },
    )
    .with_session_store(store.clone());

    let read = executor
        .execute(
            tool_call(
                root.path(),
                "read_file",
                json!({"path": "src/main.rs", "start_line": 1, "end_line": 3}),
            ),
            Some("core-tools"),
        )
        .await;
    assert!(read.success, "{:?}", read.error);
    let read_output = read.output.as_ref().unwrap();
    assert_eq!(read_output["content"], "beta\ngamma");
    assert_eq!(read_output["start_line"], 1);
    assert_eq!(read_output["end_line"], 3);
    assert_eq!(read_output["total_lines"], 4);
    assert_eq!(read_output["truncated"], true);

    let invalid_range = executor
        .execute(
            tool_call(
                root.path(),
                "read_file",
                json!({"path": "src/main.rs", "start_line": 3, "end_line": 1}),
            ),
            Some("core-tools"),
        )
        .await;
    assert!(!invalid_range.success);
    assert!(invalid_range.error.as_deref().unwrap().contains("end_line"));

    let listed = executor
        .execute(
            tool_call(
                root.path(),
                "list_directory",
                json!({"path": "src", "recursive": true, "max_depth": 4}),
            ),
            Some("core-tools"),
        )
        .await;
    assert!(listed.success, "{:?}", listed.error);
    let listed_paths = listed.output.as_ref().unwrap()["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["path"].as_str())
        .collect::<Vec<_>>();
    assert!(listed_paths.contains(&"main.rs"));
    assert!(listed_paths.contains(&"nested/lib.rs"));
    assert!(!listed_paths.contains(&".hidden"));

    let hidden = executor
        .execute(
            tool_call(
                root.path(),
                "list_directory",
                json!({"path": "src", "include_hidden": true}),
            ),
            Some("core-tools"),
        )
        .await;
    assert!(hidden.success, "{:?}", hidden.error);
    assert!(hidden.output.as_ref().unwrap()["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["path"] == ".hidden"));

    let not_directory = executor
        .execute(
            tool_call(
                root.path(),
                "list_directory",
                json!({"path": "src/main.rs"}),
            ),
            Some("core-tools"),
        )
        .await;
    assert!(!not_directory.success);
    assert!(not_directory
        .error
        .as_deref()
        .unwrap()
        .contains("Not a directory"));

    let searched = executor
        .execute(
            tool_call(
                root.path(),
                "grep",
                json!({
                    "pattern": "needle",
                    "path": ".",
                    "glob": "**/*.rs",
                    "max_results": 1
                }),
            ),
            Some("core-tools"),
        )
        .await;
    assert!(searched.success, "{:?}", searched.error);
    let matches = searched.output.as_ref().unwrap()["matches"]
        .as_array()
        .unwrap();
    assert_eq!(matches.len(), 1);
    assert!(matches[0]["file"]
        .as_str()
        .unwrap()
        .ends_with("src/nested/lib.rs"));
    assert_eq!(matches[0]["line"], 1);
    assert_eq!(searched.output.as_ref().unwrap()["truncated"], true);

    let invalid_regex = executor
        .execute(
            tool_call(root.path(), "grep", json!({"pattern": "[", "path": "."})),
            Some("core-tools"),
        )
        .await;
    assert!(!invalid_regex.success);
    assert!(invalid_regex
        .error
        .as_deref()
        .unwrap()
        .contains("invalid regex"));

    let plan = executor
        .execute(
            tool_call(
                root.path(),
                "write_plan",
                json!({"content": "1. Inspect\n2. Verify\n"}),
            ),
            Some("core-tools"),
        )
        .await;
    assert!(plan.success, "{:?}", plan.error);
    let plan_path = plan.output.as_ref().unwrap()["path"].as_str().unwrap();
    assert_eq!(
        std::fs::read_to_string(plan_path).expect("physical plan artifact"),
        "1. Inspect\n2. Verify\n"
    );
    assert_eq!(plan.output.as_ref().unwrap()["status"], "saved");

    let empty_plan = executor
        .execute(
            tool_call(root.path(), "write_plan", json!({"content": ""})),
            Some("core-tools"),
        )
        .await;
    assert!(!empty_plan.success);
    let empty_plan_error = empty_plan.error.as_deref().unwrap();
    assert!(empty_plan_error.contains("invalid arguments for tool 'write_plan'"));
    assert!(
        empty_plan_error.contains("minLength")
            || empty_plan_error.contains("minimum")
            || empty_plan_error.contains("shorter than"),
        "{empty_plan_error}"
    );
    assert_eq!(empty_plan.approval_source.as_deref(), Some("policy"));

    let audit = store.load("core-tools").expect("core tool audit");
    assert_eq!(audit.tool_calls.len(), 9);
    assert_eq!(
        audit
            .events
            .iter()
            .filter(|event| event.kind == "tool_attempted")
            .count(),
        9
    );
    assert!(audit
        .tool_calls
        .iter()
        .all(|record| record.result.as_ref().is_some()));
}

struct DenyAll;

#[async_trait]
impl ApprovalHandler for DenyAll {
    async fn handle_approval(&self, _call: &ToolCall, _level: PermissionLevel) -> ApprovalDecision {
        ApprovalDecision::denied()
    }
}

#[tokio::test]
async fn mcp_delegation_is_permission_gated_dispatched_and_audited_over_stdio() {
    let root = git_repository();
    let now = Utc::now();
    write_subagent_record(
        root.path(),
        &SubagentRecord {
            id: "delegated-one".to_string(),
            parent_session_id: Some("parent-session".to_string()),
            child_session_id: "child-session".to_string(),
            prompt: "verify delegated output".to_string(),
            status: "completed".to_string(),
            execution_generation: None,
            owner_lease: None,
            worktree_path: root.path().to_path_buf(),
            branch: "nib/subagent/delegated-one".to_string(),
            branch_oid: None,
            result: Some(json!({"summary": "verified"})),
            error: None,
            verification: None,
            created_at: now,
            updated_at: now,
        },
    )
    .expect("delegated record");

    let servers = HashMap::from([(
        "fixture".to_string(),
        McpServerEntry {
            command: env!("CARGO_BIN_EXE_nib").to_string(),
            args: vec!["mcp-server".to_string()],
            cwd: Some(root.path().to_path_buf()),
            request_timeout_secs: 5,
            ..McpServerEntry::default()
        },
    )]);
    let manager = Arc::new(
        McpManager::new(&servers, &[])
            .await
            .expect("stdio MCP manager"),
    );
    let advertised = manager.list_tools().await.expect("MCP tools");
    assert!(advertised
        .iter()
        .any(|tool| tool["name"] == "fixture::nib_get_status"));

    let store = SessionStore::new(root.path());
    store.create_session_with_id("mcp-audit");
    let mut denied_executor = ToolExecutor::new(
        root.path().to_path_buf(),
        ExecutionConfig {
            plan_mode: false,
            ..ExecutionConfig::default()
        },
    )
    .with_session_store(store.clone())
    .with_mcp_manager(Arc::clone(&manager))
    .with_approval_handler(Arc::new(DenyAll));
    let denied = denied_executor
        .execute(
            tool_call(
                root.path(),
                "fixture::nib_get_status",
                json!({"session_id": "child-session"}),
            ),
            Some("mcp-audit"),
        )
        .await;
    assert!(!denied.success);
    assert_eq!(denied.approval_source.as_deref(), Some("denied"));

    let mut allowed_executor = ToolExecutor::new(
        root.path().to_path_buf(),
        ExecutionConfig {
            plan_mode: false,
            ..ExecutionConfig::default()
        },
    )
    .with_session_store(store.clone())
    .with_mcp_manager(manager)
    .with_auto_approve(true);
    let delegated = allowed_executor
        .execute(
            tool_call(
                root.path(),
                "fixture::nib_get_status",
                json!({"session_id": "child-session"}),
            ),
            Some("mcp-audit"),
        )
        .await;
    assert!(delegated.success, "{:?}", delegated.error);
    assert_eq!(
        delegated.output.as_ref().unwrap()["structuredContent"]["id"],
        "delegated-one"
    );
    assert_eq!(
        delegated.output.as_ref().unwrap()["structuredContent"]["status"],
        "completed"
    );

    let started = allowed_executor
        .execute(
            tool_call(
                root.path(),
                "fixture::nib_run",
                json!({"goal": "inspect the delegated fixture", "max_steps": 1}),
            ),
            Some("mcp-audit"),
        )
        .await;
    assert!(started.success, "{:?}", started.error);
    #[cfg(target_os = "linux")]
    {
        let started_content = &started.output.as_ref().unwrap()["structuredContent"];
        assert_eq!(started_content["status"], "started");
        let delegated_id = started_content["subagent_id"]
            .as_str()
            .expect("delegated run id");
        let linked =
            get_subagent_record(root.path(), delegated_id).expect("linked delegation record");
        assert_eq!(linked.child_session_id, delegated_id);
        assert_eq!(linked.prompt, "inspect the delegated fixture");
        assert!(linked.worktree_path.is_dir());
    }
    #[cfg(not(target_os = "linux"))]
    {
        let response = started.output.as_ref().expect("MCP rejection response");
        assert_eq!(response["isError"], true);
        assert!(response["structuredContent"].is_null());
        let error = response["content"][0]["text"]
            .as_str()
            .expect("MCP rejection text");
        assert!(
            error.contains("production")
                && (error.contains("unavailable") || error.contains("unsupported")),
            "{error}"
        );
    }

    let audit = store.load("mcp-audit").expect("MCP audit");
    assert_eq!(audit.tool_calls.len(), 3);
    assert_eq!(
        audit.tool_calls[0].result.as_ref().unwrap()["risk"],
        "network"
    );
    assert_eq!(
        audit.tool_calls[0].result.as_ref().unwrap()["approval"]["granted"],
        false
    );
    assert_eq!(
        audit.tool_calls[1].result.as_ref().unwrap()["success"],
        true
    );
    assert_eq!(
        audit.tool_calls[1].result.as_ref().unwrap()["approval"]["source"],
        "user"
    );
    assert_eq!(
        audit.tool_calls[2].result.as_ref().unwrap()["success"],
        true
    );
    assert_eq!(
        audit.tool_calls[2].tool_name.as_deref(),
        Some("fixture::nib_run")
    );
}
