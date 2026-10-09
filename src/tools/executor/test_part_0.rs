use super::*;

#[tokio::test]
async fn approval_context_is_redacted_bounded_and_preserves_decision_behavior() {
    let root = tempfile::tempdir().expect("root");
    let capture = Arc::new(ContextCapturingApprovalHandler::default());
    let base64_secret = base64_secret_variant(b"provider-private-sentinel", false);
    let executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
        .with_approval_handler(capture.clone())
        .with_sensitive_values(["provider-private-sentinel".to_string()]);
    let call = ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "run_terminal".to_string(),
        arguments: json!({
            "command": format!("safe\u{202e}spoof {base64_secret} provider%2Dprivate%2Dsentinel {}LONG_ARGUMENT_SENTINEL", "x".repeat(5_000)),
            "token": "sk-privateapproval123456",
            "control": "\u{1b}[2J\nraw-json-sentinel",
        }),
        session_id: Some("approval-session".to_string()),
        project_root: Some(root.path().to_path_buf()),
    };
    let effective =
        executor.effective_execution_config(PermissionLevel::Destructive, ToolRisk::Destructive);
    let target = PathBuf::from(format!(
        "/tmp/provider-private-sentinel/\u{202e}spoof/\u{1b}[2J\n{}LONG_TARGET_SENTINEL",
        "x".repeat(500)
    ));
    let decision = executor
        .handle_approval(
            &call,
            PermissionLevel::Destructive,
            ToolRisk::Destructive,
            true,
            true,
            &target,
            &effective,
            Some("approval-session"),
        )
        .await;
    assert!(!decision.granted);
    assert_eq!(decision.source, "redaction");
    assert!(capture.context.lock().expect("context lock").is_none());
    let context = executor.approval_context(
        &call,
        PermissionLevel::Destructive,
        ToolRisk::Destructive,
        &target,
        &effective,
        true,
        Some("approval-session"),
        "effective tool metadata and risk classification require interactive approval",
    );
    let rendered = context.render();
    assert!(context.display_subject.contains("[REDACTED]"));
    assert!(!context
        .display_subject
        .contains("provider-private-sentinel"));
    assert!(!context.display_subject.contains(&base64_secret));
    assert!(!context
        .display_subject
        .contains("provider%2Dprivate%2Dsentinel"));
    assert!(context.display_subject.len() <= 120);
    assert!(!context.display_subject.chars().any(char::is_control));
    assert!(!context.display_subject.contains('\u{202e}'));
    assert!(context.lines().len() > MAX_APPROVAL_LINES);
    assert!(context
        .lines()
        .iter()
        .all(|line| line.len() <= MAX_APPROVAL_LINE_BYTES));
    assert!(rendered.len() < MAX_APPROVAL_DETAIL_PAGE_BYTES + 2_048);
    assert!(rendered.contains("Action: run_terminal command="));
    assert!(rendered.contains("destructive / destructive"));
    assert!(rendered.contains("Network: restricted"));
    assert!(rendered.contains("session-owned managed worktree will be created or reused"));
    assert!(!rendered.contains("provider-private-sentinel"));
    assert!(!rendered.contains("provider%2Dprivate"));
    assert!(!rendered.contains(&base64_secret));
    assert!(!rendered.contains("sk-privateapproval"));
    assert!(!rendered.contains("raw-json-sentinel"));
    assert!(!rendered.contains('\u{202e}'));
    assert!(rendered.contains("LONG_ARGUMENT_SENTINEL"));
    assert!(!rendered.contains("LONG_TARGET_SENTINEL"));
    assert!(context
        .lines()
        .iter()
        .all(|line| !line.chars().any(char::is_control)));
}

#[tokio::test]
async fn showable_command_preserves_approval_handler_decision() {
    let root = tempfile::tempdir().expect("root");
    let capture = Arc::new(ContextCapturingApprovalHandler::default());
    let executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
        .with_approval_handler(capture.clone());
    let safe_call = ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "run_terminal".to_string(),
        arguments: json!({"command": "task test:interactive"}),
        session_id: Some("approval-session".to_string()),
        project_root: Some(root.path().to_path_buf()),
    };
    let effective =
        executor.effective_execution_config(PermissionLevel::Destructive, ToolRisk::Destructive);
    let decision = executor
        .handle_approval(
            &safe_call,
            PermissionLevel::Destructive,
            ToolRisk::Destructive,
            true,
            true,
            root.path(),
            &effective,
            Some("approval-session"),
        )
        .await;
    assert!(decision.granted);
    assert_eq!(decision.source, "user");
    assert!(capture.context.lock().expect("context lock").is_some());
}

#[tokio::test]
async fn contextual_method_defaults_to_legacy_handler_for_compatibility() {
    struct LegacyGrant;
    #[async_trait::async_trait]
    impl ApprovalHandler for LegacyGrant {
        async fn handle_approval(
            &self,
            _call: &ToolCall,
            _level: PermissionLevel,
        ) -> ApprovalDecision {
            ApprovalDecision::granted_user()
        }
    }
    let call = ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "apply_patch".to_string(),
        arguments: json!({"private": "not presented by compatibility context"}),
        session_id: None,
        project_root: None,
    };
    let context = ApprovalContext::compatibility(&call, PermissionLevel::Destructive);
    let decision = LegacyGrant
        .handle_approval_with_context(&call, PermissionLevel::Destructive, &context)
        .await;
    assert!(decision.granted);
    assert_eq!(decision.source, "user");
}

#[test]
fn patch_approval_action_names_mode_and_targets_without_patch_body() {
    let root = tempfile::tempdir().expect("root");
    let executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
        .with_sensitive_values(["configured-patch-secret".to_string()]);
    let call = ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "apply_patch".to_string(),
        arguments: json!({
            "dry_run": false,
            "patch": "diff --git a/src/old.rs b/src/new.rs\n--- a/src/old.rs\n+++ b/src/new.rs\n@@ -1 +1 @@\n-configured-patch-secret\n+sk-privatepatch123456\ndiff --git a/docs/a.md b/docs/a.md\n--- a/docs/a.md\n+++ b/docs/a.md\n"
        }),
        session_id: Some("patch-session".to_string()),
        project_root: Some(root.path().to_path_buf()),
    };
    let context = executor.approval_context(
        &call,
        PermissionLevel::Safe,
        ToolRisk::Destructive,
        root.path(),
        &executor.execution_config,
        true,
        Some("patch-session"),
        "patch requires approval",
    );
    assert!(context.action.contains("apply_patch mode=apply"));
    assert!(context.action.contains("files=2"));
    assert!(context.action.contains("src/new.rs"));
    assert!(context.action.contains("docs/a.md"));
    assert!(!context.render().contains("configured-patch-secret"));
    assert!(!context.render().contains("sk-privatepatch"));
    let details = context.details.join("\n");
    assert!(details.contains("diff --git a/src/old.rs b/src/new.rs"));
    assert!(details.contains("diff --git a/docs/a.md b/docs/a.md"));
    assert!(details.contains("[REDACTED]"));
    assert!(!details.contains("configured-patch-secret"));
    assert!(!details.contains("sk-privatepatch"));
}

#[test]
fn patch_approval_display_preserves_unique_target_count_and_omission() {
    let patch = [
        "docs/e.md",
        "docs/b.md",
        "docs/a.md",
        "docs/d.md",
        "docs/c.md",
        "docs/a.md",
    ]
    .into_iter()
    .map(|path| format!("*** Update File: {path}\n"))
    .collect::<String>();
    let call = ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "apply_patch".to_string(),
        arguments: json!({"dry_run": false, "patch": patch}),
        session_id: None,
        project_root: None,
    };

    let context = ApprovalContext::compatibility(&call, PermissionLevel::Destructive);

    assert!(context.action.contains("files=5"), "{}", context.action);
    assert_eq!(
        context.display_subject,
        "5 files (+1 more): docs/a.md,docs/b.md,docs/c.md,docs/d.md"
    );
    assert!(!context.display_subject.contains("docs/e.md"));
}

#[test]
fn approval_details_page_complete_content_without_splitting_utf8() {
    let command = format!(
        "printf '{}' END_SENTINEL",
        "界".repeat(MAX_APPROVAL_DETAIL_PAGE_BYTES / 3 + 1_024)
    );
    let call = ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "run_terminal".to_string(),
        arguments: json!({
            "command": command,
            "cwd": "/tmp/work",
            "background": false,
        }),
        session_id: None,
        project_root: None,
    };

    let context = ApprovalContext::compatibility(&call, PermissionLevel::Destructive);
    assert!(context.details.len() > 1);
    assert!(context.details[0].contains("[approval details page 1/"));
    assert!(context
        .details
        .iter()
        .all(|page| page.contains("no content omitted") && page.is_char_boundary(page.len())));
    let reconstructed = context
        .details
        .iter()
        .map(|page| page.split_once('\n').expect("page header").1)
        .collect::<String>();
    assert_eq!(
        reconstructed,
        format!(
            "Command: {}\nWorking directory: /tmp/work\nBackground: false",
            call.arguments["command"].as_str().expect("command")
        )
    );
}

#[test]
fn approval_details_cover_memory_delegation_and_network_scope() {
    for (tool_name, arguments, expected) in [
        (
            "manage_memory",
            json!({
                "action": "set",
                "namespace": "environment",
                "key": "release-channel",
                "value": "development"
            }),
            ["environment", "release-channel", "development"].as_slice(),
        ),
        (
            "spawn_subagent",
            json!({"prompt": "inspect the release", "max_steps": 7}),
            ["inspect the release", "max_steps"].as_slice(),
        ),
        (
            "read_url_content",
            json!({"url": "https://example.invalid/release", "max_chars": 4000}),
            ["https://example.invalid/release", "max_chars"].as_slice(),
        ),
    ] {
        let call = ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: tool_name.to_string(),
            arguments,
            session_id: None,
            project_root: None,
        };
        let details = ApprovalContext::compatibility(&call, PermissionLevel::Safe)
            .details
            .join("\n");
        for value in expected {
            assert!(details.contains(value), "{tool_name}: {details}");
        }
    }
}

#[test]
fn plan_approval_action_identifies_plan_without_leaking_or_overrunning_goal() {
    let root = tempfile::tempdir().expect("root");
    let executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
        .with_sensitive_values(["private-plan-secret".to_string()]);
    let call = ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "approve_plan".to_string(),
        arguments: json!({
            "plan_id": "plan-123",
            "goal": format!("inspect private-plan-secret \u{1b}[2J {}PLAN_GOAL_SENTINEL", "x".repeat(500)),
            "steps": ["inspect", "change", "verify"],
        }),
        session_id: Some("plan-session".to_string()),
        project_root: Some(root.path().to_path_buf()),
    };
    let context = executor.approval_context(
        &call,
        PermissionLevel::Plan,
        ToolRisk::RequiresApproval,
        root.path(),
        &executor.execution_config,
        false,
        Some("plan-session"),
        "approve the persisted plan",
    );
    assert!(context.action.contains("approve_plan plan_id=plan-123"));
    assert!(context.action.contains("steps=3"));
    assert!(context.action.contains("goal=inspect [REDACTED]"));
    assert_eq!(
        context.details,
        vec![
            "1. inspect".to_string(),
            "2. change".to_string(),
            "3. verify".to_string(),
        ]
    );
    let rendered = context.render();
    assert!(rendered.contains("1. inspect"));
    assert!(rendered.contains("2. change"));
    assert!(rendered.contains("3. verify"));
    assert!(!context.action.contains("private-plan-secret"));
    assert!(!context.action.contains("PLAN_GOAL_SENTINEL"));
    assert!(!context.action.chars().any(char::is_control));
}

#[test]
fn prepared_guard_surfaces_and_audits_durable_compensation_failure() {
    let directory = tempfile::tempdir().expect("tempdir");
    let sessions_dir = directory.path().join("state/sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions");
    let session_store = crate::session::SessionStore::at_dir(sessions_dir.clone());
    session_store.create_session_with_id("origin");
    let store = crate::daemons::workload::DurableTaskStore::at_daemon_dir(
        directory.path().join("state/daemons"),
    )
    .expect("durable store");
    let id = format!("guard-compensation-{}", uuid::Uuid::new_v4().simple());
    store
        .prepare_terminal(crate::daemons::workload::DurableTerminalRequest {
            id: id.clone(),
            command: "printf ok".to_string(),
            cwd: directory.path().to_path_buf(),
            project_root: directory.path().to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir,
            session_id: "origin".to_string(),
            execution: crate::config::ExecutionConfig::default(),
            timeout_secs: 10,
            max_output_bytes: 1024,
        })
        .expect("prepare task");
    crate::daemons::task::TASK_MANAGER
        .register_durable_task(id.clone(), store.clone())
        .expect("register durable task");
    let record_path = store.daemon_dir().join("tasks").join(format!("{id}.json"));
    std::fs::remove_file(&record_path).expect("remove record");
    std::fs::create_dir(&record_path).expect("inject compensation failure");
    let mut guard = PreparedTaskGuard {
        task_id: Some(id.clone()),
    };

    let error = guard
        .fail("executor audit failed")
        .expect_err("guard must surface durable compensation failure");
    assert!(error.contains("daemon audit"), "{error}");
    let records =
        crate::daemons::task::DaemonAuditLog::at_path(store.daemon_dir().join("audit.jsonl"))
            .read_all()
            .expect("compensation audit");
    assert!(records.iter().any(|record| {
        record.action == "prepared_task_compensation"
            && record.target.as_deref() == Some(id.as_str())
            && record.outcome == "compensation_failed"
    }));
}

#[test]
fn parses_explicit_agents_policy_rules() {
    let directory = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        directory.path().join("AGENTS.md"),
        "- nib-policy: deny run_terminal rm -rf\n",
    )
    .expect("write");
    let rules = load_instruction_policy_rules(directory.path());
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].effect, PolicyEffect::Deny);
    assert_eq!(rules[0].argument_contains.as_deref(), Some("rm -rf"));
}

#[test]
fn oversized_instruction_policy_fails_closed() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("AGENTS.md");
    let file = std::fs::File::create(&path).expect("instruction fixture");
    file.set_len(MAX_INSTRUCTION_POLICY_BYTES + 1)
        .expect("oversized fixture");

    let rules = load_instruction_policy_rules(directory.path());
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].effect, PolicyEffect::Deny);
    assert_eq!(rules[0].tool_name, "*");
    assert!(rules[0].reason.contains("exceeds"));

    let mut config = ExecutionConfig::default();
    let error = apply_instruction_execution_tightening(directory.path(), &mut config)
        .expect_err("oversized instruction files fail closed");
    assert!(error.contains("exceeds"));
    assert_eq!(config.provider, "bwrap");
    assert_eq!(config.boundaries.network, "disabled");
}

#[test]
fn agents_directives_can_only_tighten_sandbox_boundaries() {
    let directory = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        directory.path().join("AGENTS.md"),
        "- nib-boundary: disable-network\n",
    )
    .expect("write");
    let executor = ToolExecutor::new(
        directory.path().to_path_buf(),
        ExecutionConfig {
            provider: "internal".to_string(),
            default_profile: "internal".to_string(),
            boundaries: crate::config::BoundaryConfig {
                network: "enabled".to_string(),
                ..Default::default()
            },
            ..Default::default()
        },
    );

    assert_eq!(executor.execution_config.provider, "bwrap");
    assert_eq!(executor.execution_config.default_profile, "restricted");
    assert_eq!(executor.execution_config.boundaries.network, "disabled");
}

#[test]
fn agents_can_select_a_configured_tightening_boundary_profile() {
    let directory = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        directory.path().join("AGENTS.md"),
        "- nib-boundary: profile offline-build\n",
    )
    .expect("write");
    let mut config = ExecutionConfig {
        provider: "internal".to_string(),
        default_profile: "internal".to_string(),
        boundaries: crate::config::BoundaryConfig {
            allow_write: vec!["build".to_string(), "cache".to_string()],
            network: "enabled".to_string(),
        },
        ..ExecutionConfig::default()
    };
    config.boundary_profiles.insert(
        "offline-build".to_string(),
        crate::config::BoundaryConfig {
            allow_write: vec!["build".to_string()],
            network: "restricted".to_string(),
        },
    );

    apply_instruction_execution_tightening(directory.path(), &mut config)
        .expect("profile tightens the configured boundary");

    assert_eq!(config.provider, "hybrid");
    assert_eq!(config.default_profile, "offline-build");
    assert_eq!(config.boundaries.network, "restricted");
    assert_eq!(config.boundaries.allow_write, vec!["build"]);
}

#[test]
fn malformed_or_conflicting_agents_profile_directives_fail_closed() {
    for contents in [
        "- nib-boundary: profile\n",
        "- nib-boundary: profile first\n- nib-boundary: profile second\n",
    ] {
        let directory = tempfile::tempdir().expect("tempdir");
        std::fs::write(directory.path().join("AGENTS.md"), contents).expect("write");
        let mut config = ExecutionConfig::default();
        let error = apply_instruction_execution_tightening(directory.path(), &mut config)
            .expect_err("invalid profile directive must fail");
        assert!(
            error.contains("without a name") || error.contains("conflicting boundary profiles"),
            "{error}"
        );
    }
}

#[test]
fn elevated_permissions_tighten_an_internal_execution_envelope() {
    let directory = tempfile::tempdir().expect("tempdir");
    let executor = ToolExecutor::new(
        directory.path().to_path_buf(),
        ExecutionConfig {
            provider: "internal".to_string(),
            default_profile: "internal".to_string(),
            boundaries: crate::config::BoundaryConfig {
                network: "enabled".to_string(),
                ..Default::default()
            },
            ..Default::default()
        },
    );

    let effective =
        executor.effective_execution_config(PermissionLevel::Destructive, ToolRisk::Destructive);
    assert_eq!(effective.provider, "hybrid");
    assert_eq!(effective.default_profile, "restricted");
    assert_eq!(effective.boundaries.network, "restricted");
}

#[test]
fn classifier_requires_available_isolation_for_cargo_git_and_task_commands() {
    let root = tempfile::tempdir().expect("root");
    let executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default());
    let terminal_call = |command: &str| ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "run_terminal".to_string(),
        arguments: json!({"command": command}),
        session_id: None,
        project_root: Some(root.path().to_path_buf()),
    };

    for command in ["cargo check", "git status --short", "task --list"] {
        assert!(!executor.classifier_auto_approval_allowed_with_bwrap(
            &terminal_call(command),
            PermissionLevel::Destructive,
            ToolRisk::Safe,
            false,
        ));
        assert!(executor.classifier_auto_approval_allowed_with_bwrap(
            &terminal_call(command),
            PermissionLevel::Destructive,
            ToolRisk::Safe,
            true,
        ));
    }
    assert!(executor.classifier_auto_approval_allowed_with_bwrap(
        &terminal_call("ls ."),
        PermissionLevel::Destructive,
        ToolRisk::Safe,
        false,
    ));
}

#[tokio::test]
async fn disabled_network_boundary_denies_before_dispatch_and_is_audited() {
    let root = tempfile::tempdir().expect("root");
    let store = SessionStore::new(root.path());
    let session = store.create_session();
    let mut executor = ToolExecutor::new(
        root.path().to_path_buf(),
        ExecutionConfig {
            boundaries: crate::config::BoundaryConfig {
                network: "disabled".to_string(),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .with_session_store(store.clone())
    .with_auto_approve(true);

    let result = executor
        .execute(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "search_web".to_string(),
                arguments: json!({"query": "must not be dispatched"}),
                session_id: Some(session.id.clone()),
                project_root: Some(root.path().to_path_buf()),
            },
            Some(&session.id),
        )
        .await;

    assert!(!result.success);
    assert!(result
        .error
        .as_deref()
        .is_some_and(|error| error.contains("network access is disabled")));
    assert_eq!(result.approval_source.as_deref(), Some("policy"));
    let audited = store.load(&session.id).expect("audited session");
    let call = audited.tool_calls.last().expect("tool record");
    assert_eq!(call.result.as_ref().unwrap()["permission_level"], "network");
    assert_eq!(call.result.as_ref().unwrap()["risk"], "network");
    assert_eq!(
        call.boundaries
            .as_ref()
            .expect("effective boundary")
            .network,
        "disabled"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn non_utf8_session_audit_destination_fails_closed_without_partial_delegation() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let root = tempfile::tempdir().expect("root");
    let workspace = root
        .path()
        .join(OsString::from_vec(b"workspace-\xff".to_vec()));
    let sessions_dir = workspace
        .join(".nib")
        .join("profiles")
        .join(OsString::from_vec(b"profile-\xfe".to_vec()))
        .join(OsString::from_vec(b"sessions-\xfd".to_vec()));
    std::fs::create_dir_all(&sessions_dir).expect("non-UTF-8 session directory");
    let store = SessionStore::at_dir(sessions_dir);
    let session = store.create_session_with_id("non-utf8-audit-session");
    let mut executor = ToolExecutor::new(workspace.clone(), ExecutionConfig::default())
        .with_session_store(store.clone())
        .with_auto_approve(true);

    for tool_name in ["spawn_subagent", "invoke_subagent"] {
        let result = executor
            .execute(
                ToolCall {
                    invocation_id: crate::tools::ToolInvocationId::new(),
                    tool_name: tool_name.to_string(),
                    arguments: json!({"prompt": "must fail before delegation dispatch"}),
                    session_id: Some(session.id.clone()),
                    project_root: Some(workspace.clone()),
                },
                Some(&session.id),
            )
            .await;

        assert!(!result.success, "{tool_name} unexpectedly dispatched");
        assert!(
            result.error.as_deref().is_some_and(|error| error.contains(
                "audit destination cannot be represented without changing its filesystem identity"
            )),
            "unexpected {tool_name} error: {:?}",
            result.error
        );
        let audited = store.load(&session.id).expect("audited failure session");
        let call = audited.tool_calls.last().expect("audited tool failure");
        assert_eq!(call.tool_name.as_deref(), Some(tool_name));
        assert!(call.error.as_deref().is_some_and(|error| error.contains(
            "audit destination cannot be represented without changing its filesystem identity"
        )));
    }

    for path in [
        workspace.join(".nib/subagents"),
        workspace.join(".nib/subagent-owner-leases"),
        workspace.join(".nib/worktrees/subagents"),
    ] {
        assert!(
            !path.exists(),
            "failed path serialization created partial delegation state at {}",
            path.display()
        );
    }
}

#[test]
fn redacts_structured_and_inline_secrets() {
    let value = redact_value(json!({
        "api_key": "secret",
        "output": "using sk-123456789",
    }));
    assert_eq!(value["api_key"], "[REDACTED]");
    assert!(!value["output"].as_str().unwrap().contains("sk-123456789"));
}

#[tokio::test]
async fn executor_results_and_audit_use_encoded_control_safe_projection() {
    let root = tempfile::tempdir().expect("root");
    let store = SessionStore::new(root.path());
    let session = store.create_session_with_id("encoded-tool-audit");
    let secret = "provider/env-secret".to_string();
    let mut executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
        .with_session_store(store.clone())
        .with_sensitive_values([secret.clone(), "read-only".to_string()])
        .with_auto_approve(true);

    let result = executor
        .execute_question_form(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "ask_question".to_string(),
                arguments: json!({
                    "question": format!(
                        "{secret} provider\\/env-secret cHJvdmlkZXIvZW52LXNlY3JldA== \u{1b}[2J"
                    ),
                    "options": ["provider%2Fenv-secret\r"],
                    "answer": "cHJvdmlkZXIvZW52LXNlY3JldA==\t",
                }),
                session_id: Some(session.id.clone()),
                project_root: Some(root.path().to_path_buf()),
            },
            Some(&session.id),
        )
        .await;

    assert!(result.success, "{:?}", result.error);
    let persisted = store.load(&session.id).expect("tool audit session");
    let public_surface = serde_json::to_string(&json!({
        "result": result,
        "session": persisted,
    }))
    .expect("serialize tool public surfaces");
    for forbidden in [
        secret.as_str(),
        r"provider\/env-secret",
        "provider%2Fenv-secret",
        "cHJvdmlkZXIvZW52LXNlY3JldA==",
        "read-only",
        r"\u001b",
    ] {
        assert!(
            !public_surface.contains(forbidden),
            "tool public surface contained {forbidden:?}"
        );
    }
}

#[test]
fn tool_audit_projects_every_auxiliary_metadata_field() {
    const SECRET: &str = "audit/metadata-secret";
    const JSON_SECRET: &str = r"audit\/metadata-secret";
    const PERCENT_SECRET: &str = "audit%2Fmetadata-secret";
    const BASE64_SECRET: &str = "YXVkaXQvbWV0YWRhdGEtc2VjcmV0";
    let root = tempfile::tempdir().expect("root");
    let store = SessionStore::new(root.path());
    let session = store.create_session_with_id("auxiliary-metadata-audit");
    let environment = HashMap::from([(
        format!("ENV_{SECRET}\u{1b}[2J"),
        "non-sensitive-value".to_string(),
    )]);
    let executor = ToolExecutor::new(
        root.path().to_path_buf(),
        ExecutionConfig {
            provider: SECRET.to_string(),
            default_profile: JSON_SECRET.to_string(),
            boundaries: crate::config::BoundaryConfig {
                allow_write: vec![BASE64_SECRET.to_string()],
                network: PERCENT_SECRET.to_string(),
            },
            ..ExecutionConfig::default()
        },
    )
    .with_session_store(store.clone())
    .with_environment(&environment)
    .with_sensitive_values([SECRET.to_string()]);
    let call = ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "run_terminal".to_string(),
        arguments: json!({"command": "true"}),
        session_id: Some(session.id.clone()),
        project_root: Some(root.path().to_path_buf()),
    };
    let result = ToolResult {
        invocation_id: call.invocation_id,
        tool_name: call.tool_name.clone(),
        success: false,
        output: Some(json!({
            "provider": PERCENT_SECRET,
            "sandbox_profile": JSON_SECRET,
            "bwrap_args": [format!("{BASE64_SECRET}\u{1b}[2J")],
            "boundaries": {
                "allow_write": [SECRET],
                "network": PERCENT_SECRET,
            },
            "arbitrary_metadata": format!("{SECRET}\u{202e}"),
        })),
        error: Some(format!("{JSON_SECRET}\u{1b}[2J")),
        duration_seconds: 0.0,
        approval_granted: true,
        approval_source: Some(SECRET.to_string()),
    };
    let approval = ApprovalDecision {
        granted: true,
        source: PERCENT_SECRET.to_string(),
        note: Some(format!("{BASE64_SECRET}\u{202e}")),
        remember_command: None,
    };

    executor
        .record(
            &call,
            &result,
            &approval,
            Some(&session.id),
            Some(Path::new("/tmp/audit/metadata-secret")),
            PermissionLevel::Destructive,
            ToolRisk::Destructive,
            Some(format!("plan-{SECRET}\u{1b}")),
        )
        .expect("record redacted audit metadata");

    let persisted = store.load(&session.id).expect("persisted audit");
    let public = serde_json::to_string(&persisted).expect("serialize audit");
    for forbidden in [
        SECRET,
        JSON_SECRET,
        PERCENT_SECRET,
        BASE64_SECRET,
        r"\u001b",
        r"\u202e",
    ] {
        assert!(
            !public.contains(forbidden),
            "audit contained {forbidden:?}: {public}"
        );
    }
    assert!(public.contains("[REDACTED]"), "{public}");
}

#[tokio::test]
async fn embedded_generic_secret_in_tool_name_is_redacted_from_audit() {
    let directory = tempfile::tempdir().expect("audit project");
    let store = SessionStore::new(directory.path());
    store.create_session_with_id("metadata-audit");
    let mut executor =
        ToolExecutor::new(directory.path().to_path_buf(), ExecutionConfig::default())
            .with_session_store(store.clone());
    let secret = "sk-secretvalue123";

    let result = executor
        .execute(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: format!("fixture::prefix_{secret}"),
                arguments: json!({}),
                session_id: None,
                project_root: Some(directory.path().to_path_buf()),
            },
            Some("metadata-audit"),
        )
        .await;

    assert!(!result.success);
    let session = store.load("metadata-audit").expect("audited session");
    let serialized = serde_json::to_string(&session).expect("serialize audited session");
    assert!(
        !serialized.contains(secret),
        "secret escaped audit: {serialized}"
    );
    assert!(serialized.contains("[REDACTED]"), "{serialized}");
}

#[test]
fn redacts_sensitive_environment_values_without_hiding_benign_values() {
    let environment = HashMap::from([
        (
            "DEPLOY_TOKEN".to_string(),
            "opaque-profile-value".to_string(),
        ),
        ("COLOR".to_string(), "green".to_string()),
    ]);
    let redacted = redact_value_with_environment(
        json!({"stdout": "opaque-profile-value green"}),
        &environment,
    );

    assert_eq!(redacted["stdout"], "[REDACTED] green");

    let root = tempfile::tempdir().expect("root");
    let executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
        .with_sensitive_values(["provider-credential-without-prefix".to_string()]);
    assert_eq!(
        executor.redact_text("provider-credential-without-prefix"),
        "[REDACTED]"
    );
}

#[test]
fn provider_redaction_is_symmetric_across_percent_decoding_stages() {
    assert_eq!(base64_secret_variant(b"f", false), "Zg==");
    assert_eq!(base64_secret_variant(b"fo", false), "Zm8=");
    assert_eq!(base64_secret_variant(b"foo", false), "Zm9v");
    assert_eq!(
        redact_text_with_encoded_sensitive_values("request-Zm9v", ["foo".to_string()]),
        "request-[REDACTED]"
    );
    assert_eq!(
        redact_text_with_encoded_sensitive_values(
            r#"before-active\/credential-after"#,
            ["active/credential".to_string()]
        ),
        "before-[REDACTED]-after"
    );
    for (text, secret) in [
        ("model-prefix-env%2Fonly-suffix", "env/only"),
        ("model-prefix-env/only-suffix", "env%2Fonly"),
        ("model-prefix-env%252Fonly-suffix", "env/only"),
    ] {
        let redacted = redact_text_with_encoded_sensitive_values(text, [secret.to_string()]);
        assert_eq!(redacted, "model-prefix-[REDACTED]-suffix");
        assert!(!redacted.contains("env"));
    }

    let redacted = redact_value_with_encoded_sensitive_values(
        json!({"error": "provider echoed env%252Fonly"}),
        [" env/only ".to_string()],
    );
    assert_eq!(redacted["error"], "provider echoed [REDACTED]");

    let adversarial = format!("prefix-%{}41-suffix", "25".repeat(32));
    assert_eq!(
        redact_text_with_encoded_sensitive_values(&adversarial, ["unrelated-secret".to_string()]),
        "[REDACTED]"
    );

    let secret = "provider-\u{0fff}-secret";
    let standard = base64_secret_variant(secret.as_bytes(), false);
    let url_safe = base64_secret_variant(secret.as_bytes(), true);
    assert_ne!(standard, url_safe);
    for encoded in [
        standard.clone(),
        standard.trim_end_matches('=').to_string(),
        url_safe.clone(),
        url_safe.trim_end_matches('=').to_string(),
    ] {
        assert_eq!(
            redact_text_with_encoded_sensitive_values(
                &format!("before-{encoded}-after"),
                [secret.to_string()]
            ),
            "before-[REDACTED]-after"
        );
    }
}

#[test]
fn redaction_preserves_payload_whitespace() {
    assert_eq!(
        redact_text("first line\nsecond\tline\n"),
        "first line\nsecond\tline\n"
    );
}

#[test]
fn generic_schema_validation_reports_paths_and_constraints() {
    let schema = json!({
        "type": "object",
        "properties": {
            "request": {
                "type": "object",
                "properties": {
                    "count": {"type": "integer", "minimum": 1}
                },
                "required": ["count"],
                "additionalProperties": false
            }
        },
        "required": ["request"],
        "additionalProperties": false
    });

    validate_tool_arguments("server::nested", &schema, &json!({"request": {"count": 2}}))
        .expect("valid nested MCP arguments");
    let error = validate_tool_arguments(
        "server::nested",
        &schema,
        &json!({"request": {"count": 0, "extra": true}}),
    )
    .expect_err("invalid nested arguments");
    assert!(error.contains("server::nested"));
    assert!(error.contains("/request/count") || error.contains("minimum"));
    assert!(error.contains("additionalProperties"));

    let internal_hook_call = ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "run_terminal".to_string(),
        arguments: json!({
            "command": "true",
            "hook_source": "fixture",
            "hook_for": "read_file"
        }),
        session_id: None,
        project_root: None,
    };
    let hook_arguments = schema_validation_arguments(&internal_hook_call);
    validate_tool_arguments(
        "run_terminal",
        &get_tool_metadata("run_terminal").unwrap().input_schema,
        &hook_arguments,
    )
    .expect("executor-owned hook context is removed before validation");
}

#[tokio::test]
async fn executor_rejects_invalid_arguments_before_dispatch() {
    let root = tempfile::tempdir().expect("root");
    std::fs::write(root.path().join("file.txt"), "content").expect("fixture");
    let mut executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default());

    let result = executor
        .execute(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "read_file".to_string(),
                arguments: json!({"path": "file.txt", "max_bytes": "unbounded"}),
                session_id: None,
                project_root: Some(root.path().to_path_buf()),
            },
            None,
        )
        .await;

    assert!(!result.success);
    let error = result.error.expect("validation error");
    assert!(error.contains("invalid arguments for tool 'read_file'"));
    assert!(error.contains("max_bytes"));
    assert_eq!(result.approval_source.as_deref(), Some("policy"));
}

#[tokio::test]
async fn executor_schema_diagnostics_never_reflect_boundary_straddling_credentials() {
    let root = tempfile::tempdir().expect("root");
    let store = SessionStore::new(root.path());
    let session = store.create_session_with_id("schema-diagnostic-redaction");
    let secret = format!("schema/boundary/{}", "s".repeat(512));
    let environment = HashMap::from([("DEPLOY_TOKEN".to_string(), secret.clone())]);
    let mut executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
        .with_session_store(store.clone())
        .with_environment(&environment);

    let result = executor
        .execute(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "read_file".to_string(),
                arguments: json!({
                    "path": format!("{}{}-tail", "p".repeat(8_000), secret)
                }),
                session_id: Some(session.id.clone()),
                project_root: Some(root.path().to_path_buf()),
            },
            Some(&session.id),
        )
        .await;

    assert!(!result.success);
    let persisted = store.load(&session.id).expect("schema audit session");
    let public = serde_json::to_string(&json!({
        "result": result,
        "session": persisted,
    }))
    .expect("serialize public schema surfaces");
    assert!(public.contains("/path"), "{public}");
    assert!(public.contains("maxLength"), "{public}");
    assert!(
        !public.contains(&secret[..128]),
        "credential prefix survived schema diagnostic truncation: {public}"
    );
}

#[tokio::test]
async fn executor_rejects_oversized_tool_inputs_before_approval_or_dispatch() {
    let root = tempfile::tempdir().expect("root");
    let mut executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default());

    let result = executor
        .execute(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "run_terminal".to_string(),
                arguments: json!({"command": "x".repeat(65_537)}),
                session_id: None,
                project_root: Some(root.path().to_path_buf()),
            },
            None,
        )
        .await;

    assert!(!result.success);
    assert!(result
        .error
        .as_deref()
        .is_some_and(|error| error.contains("invalid arguments")));
    assert_eq!(result.approval_source.as_deref(), Some("policy"));
}

#[tokio::test]
async fn terminal_output_sender_is_bounded_and_redacted() {
    let root = tempfile::tempdir().expect("root");
    let invocation_id = crate::tools::ToolInvocationId::new();
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let environment = HashMap::from([(
        "DEPLOY_TOKEN".to_string(),
        "profile-secret-value".to_string(),
    )]);
    let executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
        .with_environment(&environment)
        .with_terminal_output_sender(sender);
    let callback = executor
        .redacted_terminal_output_callback()
        .expect("terminal callback");

    callback(core::TerminalOutputEvent {
        invocation_id,
        tool_name: "run_terminal".to_string(),
        stream: core::TerminalOutputStream::Stdout,
        chunk: format!("sk-123456789 profile-secret-value{}", "x".repeat(64)).into_bytes(),
        background_task_id: None,
        eof: false,
    });
    callback(core::TerminalOutputEvent {
        invocation_id,
        tool_name: "run_terminal".to_string(),
        stream: core::TerminalOutputStream::Stdout,
        chunk: b"dropped when full".to_vec(),
        background_task_id: None,
        eof: false,
    });

    let event = receiver.recv().await.expect("stream event");
    assert_eq!(event.invocation_id, invocation_id);
    let output = String::from_utf8(event.chunk).expect("redacted UTF-8");
    assert!(output.starts_with("[REDACTED] [REDACTED]"));
    assert!(!output.contains("sk-123456789"));
    assert!(!output.contains("profile-secret-value"));
    assert!(receiver.try_recv().is_err(), "full channel must not grow");
}

#[test]
fn terminal_stream_redaction_hides_secrets_split_across_chunks() {
    let root = tempfile::tempdir().expect("root");
    let invocation_id = crate::tools::ToolInvocationId::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let callback: core::TerminalOutputCallback = Arc::new(move |event| {
        captured.lock().unwrap().push(event);
    });
    let executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
        .with_sensitive_values(["provider-credential-without-prefix".to_string()])
        .with_terminal_output_callback(callback);
    let redacted = executor
        .redacted_terminal_output_callback()
        .expect("redacted callback");

    let chunks: &[&[u8]] = &[
        b"before provider-cre",
        b"dential-without-prefix and prefix_sk-123",
        b"456789 after and encoded cHJvdmlkZXItY3JlZGVudGlhbC13aXRob3V0LXByZWZpeA== \x1b[31m ",
        &[0xF0, 0x9F],
        &[0x98, 0x80],
    ];
    for chunk in chunks {
        redacted(core::TerminalOutputEvent {
            invocation_id,
            tool_name: "run_terminal".to_string(),
            stream: core::TerminalOutputStream::Stdout,
            chunk: chunk.to_vec(),
            background_task_id: None,
            eof: false,
        });
    }
    redacted(core::TerminalOutputEvent {
        invocation_id,
        tool_name: "run_terminal".to_string(),
        stream: core::TerminalOutputStream::Stdout,
        chunk: Vec::new(),
        background_task_id: None,
        eof: true,
    });

    let output = events
        .lock()
        .unwrap()
        .iter()
        .flat_map(|event| event.chunk.iter().copied())
        .collect::<Vec<_>>();
    let output = String::from_utf8(output).expect("redacted stream is valid UTF-8");
    assert_eq!(
        output,
        format!(
            "before [REDACTED] and prefix_[REDACTED] after and encoded [REDACTED] �[31m {}",
            '\u{1f600}'
        )
    );
    assert!(!output.contains("provider-credential-without-prefix"));
    assert!(!output.contains("cHJvdmlkZXItY3JlZGVudGlhbC13aXRob3V0LXByZWZpeA=="));
    assert!(!output.contains('\u{1b}'));
    assert!(!output.contains("sk-123456789"));
}

#[test]
fn terminal_stream_redaction_hides_percent_encoded_secrets_across_chunks() {
    let root = tempfile::tempdir().expect("root");
    let invocation_id = crate::tools::ToolInvocationId::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let callback: core::TerminalOutputCallback = Arc::new(move |event| {
        captured.lock().unwrap().push(event);
    });
    let secret = "A ".repeat(100);
    let percent_secret = "A%20".repeat(100);
    let nested_secret = "A%2520".repeat(100);
    let executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
        .with_sensitive_values([secret.clone()])
        .with_terminal_output_callback(callback);
    let redacted = executor
        .redacted_terminal_output_callback()
        .expect("redacted callback");

    let input = format!("before {percent_secret} middle {nested_secret} after url=a%20b");
    for chunk in input.as_bytes().chunks(137) {
        redacted(core::TerminalOutputEvent {
            invocation_id,
            tool_name: "run_terminal".to_string(),
            stream: core::TerminalOutputStream::Stdout,
            chunk: chunk.to_vec(),
            background_task_id: None,
            eof: false,
        });
    }
    redacted(core::TerminalOutputEvent {
        invocation_id,
        tool_name: "run_terminal".to_string(),
        stream: core::TerminalOutputStream::Stdout,
        chunk: Vec::new(),
        background_task_id: None,
        eof: true,
    });

    let output = events
        .lock()
        .unwrap()
        .iter()
        .flat_map(|event| event.chunk.iter().copied())
        .collect::<Vec<_>>();
    let output = String::from_utf8(output).expect("redacted stream is valid UTF-8");
    assert_eq!(
        output,
        "before [REDACTED] middle [REDACTED] after url=a%20b"
    );
    assert!(!output.contains(&secret));
    assert!(!output.contains(&percent_secret));
    assert!(!output.contains(&nested_secret));
}

#[test]
fn terminal_stream_redaction_decodes_percent_stages_symmetrically() {
    let secret = "credential%2Fmarker";
    let secrets = normalized_encoded_sensitive_values([secret.to_string()]);
    let mut redactor = TerminalStreamRedactor::new(&secrets);
    let input = format!(
        "raw {secret}{} decoded credential/marker{} safe url=a%20b",
        "x".repeat(64),
        "y".repeat(64),
    );
    let mut output = Vec::new();
    for chunk in input.as_bytes().chunks(7) {
        output.extend(redactor.push(chunk, false));
    }
    output.extend(redactor.push(&[], true));

    let output = String::from_utf8(output).expect("redacted stream is valid UTF-8");
    assert_eq!(
        output,
        format!(
            "raw [REDACTED]{} decoded [REDACTED]{} safe url=a%20b",
            "x".repeat(64),
            "y".repeat(64),
        )
    );
    assert!(!output.contains(secret));
    assert!(!output.contains("credential/marker"));
}

#[tokio::test]
async fn cancelling_during_an_after_tool_hook_fails_the_prepared_task() {
    let root = tempfile::tempdir().expect("root");
    std::fs::write(root.path().join("README.md"), "fixture\n").expect("fixture");
    for arguments in [
        vec!["init", "--quiet"],
        vec!["config", "user.email", "nib-tests@example.invalid"],
        vec!["config", "user.name", "nib tests"],
        vec!["add", "README.md"],
        vec!["commit", "--quiet", "-m", "initial"],
    ] {
        let status = std::process::Command::new("git")
            .args(arguments)
            .current_dir(root.path())
            .status()
            .expect("git fixture command");
        assert!(status.success());
    }
    let mut config = crate::config::NibConfig::default();
    config.profiles.default = "selected".to_string();
    config.profiles.active = vec![crate::config::ProfileConfig {
        id: "selected".to_string(),
        root: PathBuf::from("."),
        ..crate::config::ProfileConfig::default()
    }];
    crate::config::save_nib_config_full(root.path(), &mut config).expect("profile config");
    let store = SessionStore::for_project(root.path()).expect("session store");
    store.create_session_with_id("hook-cancel");
    let task_store =
        crate::daemons::workload::DurableTaskStore::from_sessions_dir(store.sessions_dir())
            .expect("durable task store");
    let mut executor = ToolExecutor::new(
        root.path().to_path_buf(),
        ExecutionConfig {
            provider: "internal".to_string(),
            default_profile: "internal".to_string(),
            plan_mode: false,
            ..ExecutionConfig::default()
        },
    )
    .with_auto_approve(true)
    .with_session_store(store)
    .with_deferred_background_start(true)
    .with_after_tool_hooks([AfterToolHook {
        source: "blocking-hook".to_string(),
        tool_name: "schedule".to_string(),
        command: "sleep 30".to_string(),
    }]);
    let project_root = root.path().to_path_buf();
    let run = tokio::spawn(async move {
        executor
            .execute(
                ToolCall {
                    invocation_id: crate::tools::ToolInvocationId::new(),
                    tool_name: "schedule".to_string(),
                    arguments: json!({
                        "prompt": "later",
                        "duration_secs": 3_600,
                    }),
                    session_id: Some("hook-cancel".to_string()),
                    project_root: Some(project_root),
                },
                Some("hook-cancel"),
            )
            .await
    });

    let prepared = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(task) = task_store
                .list()
                .expect("list durable tasks")
                .into_iter()
                .find(|task| task.kind == "schedule")
            {
                break task;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("schedule was prepared before the hook blocked");
    assert_eq!(prepared.status, "prepared", "{prepared:?}");

    run.abort();
    let _ = run.await;
    let failed = task_store
        .get(&prepared.id)
        .expect("load prepared task")
        .expect("prepared task remains auditable");
    assert_eq!(failed.status, "failed");
    assert!(failed
        .error
        .as_deref()
        .is_some_and(|error| error.contains("executor reconciliation")));
}

#[tokio::test]
async fn schedule_uses_executor_owned_session_context() {
    let root = tempfile::tempdir().expect("root");
    let mut config = crate::config::NibConfig::default();
    config.profiles.default = "selected".to_string();
    config.profiles.active = vec![crate::config::ProfileConfig {
        id: "selected".to_string(),
        root: PathBuf::from("."),
        ..crate::config::ProfileConfig::default()
    }];
    crate::config::save_nib_config_full(root.path(), &mut config).expect("profile config");
    let sessions_dir = root
        .path()
        .join(".nib")
        .join("profiles")
        .join("selected")
        .join("sessions");
    let store = SessionStore::at_dir(sessions_dir.clone());
    store.create_session_with_id("origin");
    let mut executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
        .with_session_store(store.clone());

    let result = executor
        .execute(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "schedule".to_string(),
                arguments: json!({
                    "prompt": "later",
                    "duration_secs": 3600,
                    "_session_id": "spoofed",
                    "_sessions_dir": root.path().join("spoofed"),
                }),
                session_id: Some("origin".to_string()),
                project_root: Some(root.path().to_path_buf()),
            },
            Some("origin"),
        )
        .await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output.as_ref().unwrap()["session_id"], "origin");
    assert!(!root.path().join("spoofed").exists());
    let timer_id = result.output.as_ref().unwrap()["task_id"].as_str().unwrap();
    crate::daemons::task::TASK_MANAGER
        .cancel(timer_id)
        .expect("cancel fixture timer");
    let session = store.load("origin").expect("origin session");
    assert!(session
        .events
        .iter()
        .any(|event| event.kind == "timer_scheduled"));
}

#[tokio::test]
async fn schedule_rejects_untrusted_reserved_context_without_a_session() {
    let root = tempfile::tempdir().expect("root");
    let mut executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default());
    let result = executor
        .execute(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "schedule".to_string(),
                arguments: json!({
                    "prompt": "later",
                    "duration_secs": 60,
                    "_session_id": "spoofed",
                    "_sessions_dir": root.path(),
                }),
                session_id: None,
                project_root: Some(root.path().to_path_buf()),
            },
            None,
        )
        .await;
    assert!(!result.success);
    assert!(result.error.as_deref().unwrap().contains("_session_id"));
    let store = SessionStore::for_project(root.path()).expect("implicit audit store");
    let session_ids = store.list_result().expect("implicit audit sessions");
    assert_eq!(session_ids.len(), 1);
    let session = store
        .load_result(&session_ids[0])
        .expect("load implicit audit")
        .expect("implicit audit session");
    assert!(session
        .events
        .iter()
        .any(|event| event.kind == "implicit_audit_session"));
    let record = session.tool_calls.last().expect("schedule denial audit");
    assert_eq!(record.tool_name.as_deref(), Some("schedule"));
    assert_eq!(record.result.as_ref().unwrap()["success"], false);
}

/// Records prompts and grants them; `interactive` controls `can_prompt`.
struct PromptCounter {
    interactive: bool,
    prompts: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl ApprovalHandler for PromptCounter {
    fn can_prompt(&self) -> bool {
        self.interactive
    }

    async fn handle_approval(&self, _call: &ToolCall, _level: PermissionLevel) -> ApprovalDecision {
        self.prompts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ApprovalDecision::granted_user()
    }
}

async fn decide_in_mode(
    mode: ApprovalMode,
    interactive: bool,
    tool: &str,
    arguments: serde_json::Value,
    level: PermissionLevel,
) -> (ApprovalDecision, usize) {
    let root = tempfile::tempdir().expect("root");
    let handler = Arc::new(PromptCounter {
        interactive,
        prompts: std::sync::atomic::AtomicUsize::new(0),
    });
    let executor = ToolExecutor::new(root.path().to_path_buf(), ExecutionConfig::default())
        .with_approval_handler(handler.clone())
        .with_approval_mode(mode);
    let call = ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: tool.to_string(),
        arguments,
        session_id: Some("mode-session".to_string()),
        project_root: Some(root.path().to_path_buf()),
    };
    let risk = crate::tools::classifier::classify_tool_call(&call);
    let effective = executor.effective_execution_config(level, risk);
    let decision = executor
        .handle_approval(
            &call,
            level,
            risk,
            true,
            true,
            root.path(),
            &effective,
            Some("mode-session"),
        )
        .await;
    let prompts = handler.prompts.load(std::sync::atomic::Ordering::SeqCst);
    (decision, prompts)
}

fn patch_arguments() -> serde_json::Value {
    json!({"patch": "*** Begin Patch\n*** Add File: notes.txt\n+hi\n*** End Patch\n"})
}

/// T080 phase 2: each permission mode's decision for an edit, a command that
/// needs approval and a read-only command.
#[tokio::test]
async fn permission_modes_decide_edits_commands_and_reads() {
    let edit = || ("apply_patch", patch_arguments(), PermissionLevel::Safe);
    let command = || {
        (
            "run_terminal",
            json!({"command": "mkdir build-output", "affected_paths": ["."]}),
            PermissionLevel::Destructive,
        )
    };
    let read = || {
        (
            "run_terminal",
            json!({"command": "git status; git log -1", "affected_paths": ["."]}),
            PermissionLevel::Destructive,
        )
    };

    // ask: edits and commands prompt; reads do not.
    for (tool, args, level) in [edit(), command()] {
        let (decision, prompts) =
            decide_in_mode(ApprovalMode::Manual, true, tool, args, level).await;
        assert!(decision.granted && prompts == 1, "ask {tool}: {decision:?}");
    }
    let (tool, args, level) = read();
    let (decision, prompts) = decide_in_mode(ApprovalMode::Manual, true, tool, args, level).await;
    assert!(decision.granted && prompts == 0, "ask read: {decision:?}");

    // accept-edits: edits apply without a prompt; commands still prompt.
    let (tool, args, level) = edit();
    let (decision, prompts) = decide_in_mode(ApprovalMode::Smart, true, tool, args, level).await;
    assert!(
        decision.granted && prompts == 0 && decision.source == "mode",
        "{decision:?}"
    );
    let (tool, args, level) = command();
    let (decision, prompts) = decide_in_mode(ApprovalMode::Smart, true, tool, args, level).await;
    assert!(decision.granted && prompts == 1, "{decision:?}");

    // plan: changes are refused without prompting; reads still run.
    for (tool, args, level) in [edit(), command()] {
        let (decision, prompts) = decide_in_mode(ApprovalMode::Plan, true, tool, args, level).await;
        assert!(
            !decision.granted && prompts == 0,
            "plan {tool}: {decision:?}"
        );
        assert!(decision
            .note
            .as_deref()
            .unwrap_or_default()
            .contains("plan mode"));
    }
    let (tool, args, level) = read();
    let (decision, _) = decide_in_mode(ApprovalMode::Plan, true, tool, args, level).await;
    assert!(decision.granted, "plan read: {decision:?}");

    // policy: unmatched actions prompt when a person can answer, and are
    // denied without a prompt in headless runs.
    let (tool, args, level) = command();
    let (decision, prompts) = decide_in_mode(ApprovalMode::Policy, true, tool, args, level).await;
    assert!(decision.granted && prompts == 1, "{decision:?}");
    let (tool, args, level) = command();
    let (decision, prompts) = decide_in_mode(ApprovalMode::Policy, false, tool, args, level).await;
    assert!(!decision.granted && prompts == 0, "{decision:?}");

    // auto: no prompts.
    let (tool, args, level) = command();
    let (decision, prompts) = decide_in_mode(ApprovalMode::Off, true, tool, args, level).await;
    assert!(decision.granted && prompts == 0, "{decision:?}");
}

#[test]
fn permission_mode_names_parse_label_and_cycle() {
    for (name, mode) in [
        ("ask", ApprovalMode::Manual),
        ("manual", ApprovalMode::Manual),
        ("accept-edits", ApprovalMode::Smart),
        ("smart", ApprovalMode::Manual),
        ("plan", ApprovalMode::Plan),
        ("auto", ApprovalMode::Off),
        ("off", ApprovalMode::Off),
        ("policy", ApprovalMode::Policy),
    ] {
        assert_eq!(permission_mode_from_name(name), Some(mode), "{name}");
    }
    assert_eq!(permission_mode_from_name("yolo"), None);
    let mut mode = ApprovalMode::Manual;
    let mut seen = Vec::new();
    for _ in 0..3 {
        mode = next_cycled_permission_mode(mode);
        seen.push(permission_mode_label(mode));
    }
    assert_eq!(seen, ["accept-edits", "plan", "ask"]);
    assert_eq!(
        next_cycled_permission_mode(ApprovalMode::Off),
        ApprovalMode::Manual
    );
}

fn mode_executor(
    root: &std::path::Path,
    mode: ApprovalMode,
    interactive: bool,
) -> (ToolExecutor, Arc<PromptCounter>) {
    let handler = Arc::new(PromptCounter {
        interactive,
        prompts: std::sync::atomic::AtomicUsize::new(0),
    });
    let executor = ToolExecutor::new(root.to_path_buf(), ExecutionConfig::default())
        .with_approval_handler(handler.clone())
        .with_approval_mode(mode);
    (executor, handler)
}

async fn decide(
    executor: &ToolExecutor,
    root: &std::path::Path,
    call: &ToolCall,
) -> ApprovalDecision {
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
            Some("mode-session"),
        )
        .await
}

fn terminal_call(root: &std::path::Path, command: &str) -> ToolCall {
    ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "run_terminal".to_string(),
        arguments: json!({"command": command, "affected_paths": ["."]}),
        session_id: Some("mode-session".to_string()),
        project_root: Some(root.to_path_buf()),
    }
}

/// T080 2a review: plan mode refuses changes before allow rules, remembered
/// grants, `--yes` and classifier auto-approval, and still lets questions run.
#[tokio::test]
async fn plan_mode_wins_over_grants_and_allows_questions() {
    let root = tempfile::tempdir().expect("root");
    let command = terminal_call(root.path(), "mkdir build-output");

    let allow = PolicyRule {
        effect: PolicyEffect::Allow,
        tool_name: "run_terminal".to_string(),
        argument_contains: None,
        reason: "fixture allows terminal".to_string(),
    };
    let (manual, _) = mode_executor(root.path(), ApprovalMode::Manual, true);
    let manual = manual.with_policy_rules([allow.clone()]);
    assert!(decide(&manual, root.path(), &command).await.granted);
    let (plan, prompts) = mode_executor(root.path(), ApprovalMode::Plan, true);
    let plan = plan.with_policy_rules([allow]);
    assert!(!decide(&plan, root.path(), &command).await.granted);
    assert_eq!(prompts.prompts.load(std::sync::atomic::Ordering::SeqCst), 0);

    crate::interaction_card::remember_invocation(
        root.path(),
        &crate::interaction_card::terminal_invocation(&command).expect("invocation"),
    )
    .expect("remember");
    let (manual, prompts) = mode_executor(root.path(), ApprovalMode::Manual, true);
    let remembered = decide(&manual, root.path(), &command).await;
    assert!(
        remembered.granted && remembered.source == "command_prefix",
        "{remembered:?}"
    );
    assert_eq!(prompts.prompts.load(std::sync::atomic::Ordering::SeqCst), 0);
    let (plan, _) = mode_executor(root.path(), ApprovalMode::Plan, true);
    assert!(!decide(&plan, root.path(), &command).await.granted);

    let (plan, _) = mode_executor(root.path(), ApprovalMode::Plan, true);
    let plan = plan.with_auto_approve(true);
    assert!(!decide(&plan, root.path(), &command).await.granted);
    assert!(
        !decide(
            &plan,
            root.path(),
            &terminal_call(root.path(), "cargo check")
        )
        .await
        .granted
    );

    let question = ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "ask_question".to_string(),
        arguments: json!({"question": "Which target?", "options": ["alpha", "beta"]}),
        session_id: Some("mode-session".to_string()),
        project_root: Some(root.path().to_path_buf()),
    };
    let (plan, _) = mode_executor(root.path(), ApprovalMode::Plan, true);
    assert!(decide(&plan, root.path(), &question).await.granted);
}

/// T080 2a review: the approval pre-check agrees with `handle_approval` in
/// every mode, so no spurious approval event is emitted.
#[test]
fn approval_precheck_matches_each_mode() {
    let root = tempfile::tempdir().expect("root");
    let command = terminal_call(root.path(), "mkdir build-output");
    let patch = ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "apply_patch".to_string(),
        arguments: patch_arguments(),
        session_id: Some("mode-session".to_string()),
        project_root: Some(root.path().to_path_buf()),
    };
    for (mode, interactive, command_prompts, patch_prompts) in [
        (ApprovalMode::Manual, true, true, true),
        (ApprovalMode::Smart, true, true, false),
        (ApprovalMode::Plan, true, false, false),
        (ApprovalMode::Off, true, false, false),
        (ApprovalMode::Policy, true, true, true),
        (ApprovalMode::Policy, false, false, false),
    ] {
        let (executor, _) = mode_executor(root.path(), mode, interactive);
        assert_eq!(
            executor.requires_interactive_approval(&command),
            command_prompts,
            "{mode:?} command"
        );
        assert_eq!(
            executor.requires_interactive_approval(&patch),
            patch_prompts,
            "{mode:?} patch"
        );
    }
    let (plan, _) = mode_executor(root.path(), ApprovalMode::Plan, true);
    let plan = plan.with_policy_rules([PolicyRule {
        effect: PolicyEffect::RequireApproval,
        tool_name: "run_terminal".to_string(),
        argument_contains: None,
        reason: "fixture requires approval".to_string(),
    }]);
    assert!(!plan.requires_interactive_approval(&command));
}

fn git_tool_call(root: &std::path::Path, tool: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: tool.to_string(),
        arguments,
        session_id: Some("mode-session".to_string()),
        project_root: Some(root.to_path_buf()),
    }
}

/// T080 phase 2b: a push always asks, even in auto mode or with --yes,
/// unless an explicit allow rule exists; headless runs cannot push; plan mode
/// refuses commits and pushes; commits follow the mode like other changes.
#[tokio::test]
async fn git_tools_follow_modes_and_push_always_asks() {
    let root = tempfile::tempdir().expect("root");
    let push = git_tool_call(root.path(), "git_push", json!({}));
    let commit = git_tool_call(root.path(), "git_commit", json!({"message": "x"}));

    for mode in [ApprovalMode::Off, ApprovalMode::Manual, ApprovalMode::Smart] {
        let (executor, prompts) = mode_executor(root.path(), mode, true);
        let executor = executor.with_auto_approve(true);
        assert!(
            decide(&executor, root.path(), &push).await.granted,
            "{mode:?}"
        );
        assert_eq!(
            prompts.prompts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "{mode:?} push must prompt"
        );
        assert!(executor.requires_interactive_approval(&push), "{mode:?}");
    }
    let (headless, _) = mode_executor(root.path(), ApprovalMode::Off, false);
    assert!(!decide(&headless, root.path(), &push).await.granted);
    assert!(!headless.requires_interactive_approval(&push));

    // Allow rules can come from workspace instruction files an agent could
    // write, so even an explicit allow rule does not skip the push prompt.
    let (allowed, prompts) = mode_executor(root.path(), ApprovalMode::Off, true);
    let allowed = allowed.with_policy_rules([PolicyRule {
        effect: PolicyEffect::Allow,
        tool_name: "*".to_string(),
        argument_contains: None,
        reason: "planted allow-all rule".to_string(),
    }]);
    assert!(decide(&allowed, root.path(), &push).await.granted);
    assert_eq!(prompts.prompts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(allowed.requires_interactive_approval(&push));

    let (plan, _) = mode_executor(root.path(), ApprovalMode::Plan, true);
    assert!(!decide(&plan, root.path(), &push).await.granted);
    assert!(!decide(&plan, root.path(), &commit).await.granted);

    let (ask, prompts) = mode_executor(root.path(), ApprovalMode::Manual, true);
    assert!(decide(&ask, root.path(), &commit).await.granted);
    assert_eq!(prompts.prompts.load(std::sync::atomic::Ordering::SeqCst), 1);
    let (auto, prompts) = mode_executor(root.path(), ApprovalMode::Off, true);
    assert!(decide(&auto, root.path(), &commit).await.granted);
    assert_eq!(prompts.prompts.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// T080 phase 2b: a terminal commit or push is redirected to the Git tools
/// before any approval or sandbox work.
#[tokio::test]
async fn terminal_git_writes_are_redirected_to_git_tools() {
    let root = tempfile::tempdir().expect("root");
    let (mut executor, prompts) = mode_executor(root.path(), ApprovalMode::Off, true);
    let result = executor
        .execute(terminal_call(root.path(), "git commit -am wip"), None)
        .await;
    assert!(!result.success);
    let error = result.error.unwrap_or_default();
    assert!(error.contains("use the git_commit tool"), "{error}");
    assert_eq!(prompts.prompts.load(std::sync::atomic::Ordering::SeqCst), 0);
}

fn fixture_git(cwd: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("fixture git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// T080 phase 2b review R1: the commit preview describes the checkout the
/// commit runs in (the session worktree), not the main checkout.
#[tokio::test]
#[serial_test::serial]
async fn git_commit_preview_describes_the_session_worktree() {
    let root = tempfile::tempdir().expect("root");
    let root = root.path().canonicalize().expect("canonical root");
    fixture_git(&root, &["init", "--quiet", "--initial-branch", "main"]);
    fixture_git(&root, &["config", "user.name", "Fixture"]);
    fixture_git(&root, &["config", "user.email", "fixture@example.invalid"]);
    std::fs::write(root.join("README.md"), "start\n").unwrap();
    std::fs::write(root.join(".gitignore"), ".nib/\n").unwrap();
    fixture_git(&root, &["add", "."]);
    fixture_git(&root, &["commit", "--quiet", "-m", "start"]);

    let capture = Arc::new(ContextCapturingApprovalHandler::default());
    let mut executor = ToolExecutor::new(root.clone(), ExecutionConfig::default())
        .with_approval_handler(capture.clone());
    let worktree = executor
        .ensure_worktree(true, &root, Some("preview-session"))
        .await
        .expect("session worktree")
        .expect("worktree path");
    std::fs::write(worktree.join("feature.txt"), "session change\n").unwrap();
    std::fs::write(root.join("main-only.txt"), "main checkout change\n").unwrap();

    let commit = ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "git_commit".to_string(),
        arguments: json!({"message": "Add feature"}),
        session_id: Some("preview-session".to_string()),
        project_root: Some(root.clone()),
    };
    let context = executor.approval_context(
        &commit,
        PermissionLevel::Destructive,
        ToolRisk::Destructive,
        &root,
        &executor.execution_config,
        true,
        Some("preview-session"),
        "commit requires approval",
    );
    executor
        .prompt_approval(
            &commit,
            PermissionLevel::Destructive,
            context,
            Some("preview-session"),
        )
        .await;
    let details = capture
        .context
        .lock()
        .expect("context lock")
        .clone()
        .expect("captured context")
        .details
        .join("\n");
    assert!(details.contains("feature.txt"), "{details}");
    assert!(!details.contains("main-only.txt"), "{details}");
    assert!(details.contains("Branch: nib/session/"), "{details}");

    let fresh = ToolCall {
        session_id: Some("fresh-session".to_string()),
        ..commit.clone()
    };
    let context = executor.approval_context(
        &fresh,
        PermissionLevel::Destructive,
        ToolRisk::Destructive,
        &root,
        &executor.execution_config,
        true,
        Some("fresh-session"),
        "commit requires approval",
    );
    executor
        .prompt_approval(
            &fresh,
            PermissionLevel::Destructive,
            context,
            Some("fresh-session"),
        )
        .await;
    let details = capture
        .context
        .lock()
        .expect("context lock")
        .clone()
        .expect("captured context")
        .details
        .join("\n");
    assert!(details.contains("no worktree yet"), "{details}");
}
