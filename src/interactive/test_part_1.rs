use super::*;

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn new_commands_are_parsed_and_runtime_commands_have_typed_effects() {
    let project = tempdir().expect("project");
    let mut config = NibConfig::default();
    config
        .llm
        .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
    save_nib_config_full(project.path(), &mut config).expect("config");
    let store = SessionStore::for_project(project.path()).expect("store");
    let session_id = resolve_session(&store, None)
        .expect("session")
        .session_id()
        .to_string();
    store
        .try_append_message(&session_id, "user", "goal")
        .expect("user");
    store
        .try_append_message(&session_id, "assistant", "done")
        .expect("assistant");

    for command in [
        "/status",
        "/context",
        "/context details",
        "/permissions",
        "/review",
        "/diff",
        "/compact",
        "/new",
        "/resume",
        "/fork",
        "/rename wrap-fix",
        "/copy",
        "/ps",
        "/stop",
        "/stop exact-task",
        "/continue plan-1",
        "/help",
    ] {
        parse_interactive_command(command).unwrap_or_else(|error| panic!("{command}: {error}"));
    }

    let InteractiveEffect::Output(status) = execute_interactive_command_in_state(
        InteractiveCommand::Status,
        project.path(),
        "focused",
        &store,
        &session_id,
        "running",
    )
    .expect("status") else {
        panic!("status output");
    };
    assert!(status.contains("running"));
    assert!(status.contains(&session_id));
    assert!(status.contains("profile focused"));

    let InteractiveEffect::Output(context) = execute_interactive_command(
        InteractiveCommand::Context { details: true },
        project.path(),
        &store,
        &session_id,
    )
    .expect("context details") else {
        panic!("context output");
    };
    assert!(
        context.contains("ctx ~") || context.contains("ctx ?") || context.contains("Context ~"),
        "{context}"
    );
    assert!(context.contains("History: 2 message(s)"));
    assert!(context.contains("Latest run: no resource evidence yet"));
    assert!(
        context.contains("unavailable historical accounting")
            || context.contains("Request snapshot"),
        "{context}"
    );

    let effect = execute_interactive_command(
        InteractiveCommand::Compact,
        project.path(),
        &store,
        &session_id,
    )
    .expect("compact");
    assert_eq!(effect, InteractiveEffect::Compact);

    let InteractiveEffect::Output(tasks) =
        execute_interactive_command(InteractiveCommand::Ps, project.path(), &store, &session_id)
            .expect("ps")
    else {
        panic!("ps output");
    };
    assert!(tasks.contains("Session-owned background work"));
    assert!(tasks.contains("(none)"));

    let InteractiveEffect::Output(stop) = execute_interactive_command(
        InteractiveCommand::Stop { task_id: None },
        project.path(),
        &store,
        &session_id,
    )
    .expect("stop") else {
        panic!("stop output");
    };
    assert!(stop.contains("running background work"));
    assert!(stop.contains("(none)"));
    assert!(stop.contains("/stop <task-id>"));

    let InteractiveEffect::Output(copied) = execute_interactive_command(
        InteractiveCommand::Copy,
        project.path(),
        &store,
        &session_id,
    )
    .expect("copy") else {
        panic!("copy output");
    };
    assert_eq!(copied, "done");

    let InteractiveEffect::SessionChanged {
        session_id: forked, ..
    } = execute_interactive_command(
        InteractiveCommand::Fork,
        project.path(),
        &store,
        &session_id,
    )
    .expect("fork")
    else {
        panic!("fork session");
    };
    let forked_session = store
        .load_result(&forked)
        .expect("load fork")
        .expect("fork exists");
    assert_eq!(
        forked_session.forked_from.as_deref(),
        Some(session_id.as_str())
    );
    let source = store
        .load_result(&session_id)
        .expect("reload source")
        .expect("source");
    assert!(source.forked_from.is_none());

    assert!(
        unicode_display_width("漢字") > unicode_display_width("ab")
            || unicode_display_width("漢字") == 4
    );
    assert_eq!(bottom_scroll_for_wrap("ab", 1, 1), 1);
    assert_eq!(wrapped_line_count(&"a".repeat(200), 80), 3);
}

#[test]
fn display_rows_preserve_graphemes_at_wrap_boundaries() {
    for grapheme in ["☺\u{fe0f}", "1\u{fe0f}\u{20e3}", "👩\u{200d}💻", "👍🏽"] {
        assert_eq!(unicode_display_width(grapheme), 2);
        let text = format!("> a{grapheme}");
        assert_eq!(
            wrapped_display_rows(&text, 4),
            vec!["> a".to_string(), grapheme.to_string()],
            "grapheme={grapheme:?}"
        );
    }
    assert_eq!(wrapped_display_rows("> ae\u{301}", 4), ["> ae\u{301}"]);
}

#[test]
fn chrome_truncation_preserves_complete_graphemes() {
    for grapheme in ["☺\u{fe0f}", "👩\u{200d}💻"] {
        let text = format!("a{grapheme}xy");
        assert_eq!(truncate_display_cells(&text, 3), "a…");
        assert_eq!(truncate_display_cells(&text, 4), format!("a{grapheme}…"));
    }
    assert_eq!(truncate_display_cells("e\u{301}xyz", 2), "e\u{301}…");
}

#[test]
fn background_projection_is_session_scoped_bounded_and_command_free() {
    let project = tempdir().expect("project");
    let mut config = NibConfig::default();
    config
        .llm
        .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
    save_nib_config_full(project.path(), &mut config).expect("config");
    let sessions = SessionStore::for_project(project.path()).expect("sessions");
    let session = sessions.create_session_with_id("owned-session");
    sessions.create_session_with_id("foreign-session");
    let tasks = crate::daemons::workload::DurableTaskStore::for_project(project.path())
        .expect("task store");
    for index in 0..=MAX_INTERACTIVE_BACKGROUND_TASKS {
        let id = format!("bg-{index:03}");
        tasks
            .prepare_terminal(crate::daemons::workload::DurableTerminalRequest {
                id,
                command: format!("private-command-{index}"),
                cwd: project.path().to_path_buf(),
                project_root: project.path().to_path_buf(),
                profile_id: "default".to_string(),
                sessions_dir: sessions.sessions_dir().to_path_buf(),
                session_id: session.id.clone(),
                execution: crate::config::ExecutionConfig::default(),
                timeout_secs: 10,
                max_output_bytes: 1_024,
            })
            .expect("prepare owned task");
    }
    tasks
        .prepare_terminal(crate::daemons::workload::DurableTerminalRequest {
            id: "foreign-background".to_string(),
            command: "foreign-private-command".to_string(),
            cwd: project.path().to_path_buf(),
            project_root: project.path().to_path_buf(),
            profile_id: "default".to_string(),
            sessions_dir: sessions.sessions_dir().to_path_buf(),
            session_id: "foreign-session".to_string(),
            execution: crate::config::ExecutionConfig::default(),
            timeout_secs: 10,
            max_output_bytes: 1_024,
        })
        .expect("prepare foreign task");

    let output = format_session_background_tasks(&sessions, &session.id, false)
        .expect("background projection");
    assert_eq!(
        output
            .lines()
            .filter(|line| line.trim_start().starts_with("- bg-"))
            .count(),
        MAX_INTERACTIVE_BACKGROUND_TASKS
    );
    assert!(output.contains("1 additional tasks omitted"));
    assert!(!output.contains("foreign-background"));
    assert!(!output.contains("private-command"));
    assert!(!output.contains("worker_pid"));
}

#[test]
fn background_commands_remain_bound_to_the_profile_captured_at_startup() {
    let project = tempdir().expect("project");
    let mut config = NibConfig::default();
    config.profiles.default = "profile-a".to_string();
    config.profiles.active = vec![
        crate::config::ProfileConfig {
            id: "profile-a".to_string(),
            root: Path::new(".").to_path_buf(),
            state_dir: Some(Path::new(".nib/profiles/profile-a").to_path_buf()),
            ..Default::default()
        },
        crate::config::ProfileConfig {
            id: "profile-b".to_string(),
            root: Path::new(".").to_path_buf(),
            state_dir: Some(Path::new(".nib/profiles/profile-b").to_path_buf()),
            ..Default::default()
        },
    ];
    save_nib_config_full(project.path(), &mut config).expect("initial profiles");
    let captured = resolve_interactive_profile_scope(project.path()).expect("captured profile");
    assert_eq!(captured.profile_id(), "profile-a");
    let store_a = captured.into_session_store();
    let session_id = "coincident-session";
    store_a.create_session_with_id(session_id);

    config.profiles.default = "profile-b".to_string();
    save_nib_config_full(project.path(), &mut config).expect("changed default profile");
    let store_b = SessionStore::for_project(project.path()).expect("new default profile");
    store_b.create_session_with_id(session_id);

    for (store, profile_id, task_id, command) in [
        (&store_a, "profile-a", "profile-a-task", "private-a-command"),
        (&store_b, "profile-b", "profile-b-task", "private-b-command"),
    ] {
        crate::daemons::workload::DurableTaskStore::from_sessions_dir(store.sessions_dir())
            .expect("profile task store")
            .prepare_terminal(crate::daemons::workload::DurableTerminalRequest {
                id: task_id.to_string(),
                command: command.to_string(),
                cwd: project.path().to_path_buf(),
                project_root: project.path().to_path_buf(),
                profile_id: profile_id.to_string(),
                sessions_dir: store.sessions_dir().to_path_buf(),
                session_id: session_id.to_string(),
                execution: crate::config::ExecutionConfig::default(),
                timeout_secs: 10,
                max_output_bytes: 1_024,
            })
            .expect("prepare profile task");
    }

    let InteractiveEffect::Output(output) =
        execute_interactive_command(InteractiveCommand::Ps, project.path(), &store_a, session_id)
            .expect("profile-bound task listing")
    else {
        panic!("background output");
    };
    assert!(output.contains("profile-a-task"));
    assert!(!output.contains("profile-b-task"));
    assert!(!output.contains("private-a-command"));
    assert!(!output.contains("private-b-command"));
}

#[test]
fn permissions_use_instruction_tightening_and_invalid_directives_fail_closed() {
    let project = tempdir().expect("project");
    let mut config = NibConfig::default();
    config
        .llm
        .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
    save_nib_config_full(project.path(), &mut config).expect("config");
    std::fs::write(
        project.path().join("AGENTS.md"),
        "nib-boundary: disable-network\n",
    )
    .expect("instruction boundary");

    let tightened = format_permissions(project.path()).expect("tightened posture");
    assert!(tightened.contains("Configured approval preset: manual"));
    assert!(tightened.contains("provider: bwrap"));
    assert!(tightened.contains("network: disabled"));
    assert!(tightened.contains("tightened by project instructions"));
    assert!(tightened.contains("managed-owned-worktree gate: required"));

    std::fs::write(project.path().join("AGENTS.md"), "nib-boundary: profile\n")
        .expect("invalid instruction boundary");
    let failed_closed = format_permissions(project.path()).expect("fail-closed posture");
    assert!(failed_closed.contains("provider: bwrap"));
    assert!(failed_closed.contains("network: disabled"));
    assert!(failed_closed.contains("INVALID project directive"));
    assert!(failed_closed.contains("FAILS CLOSED"));
}

#[test]
fn permission_selection_recomputes_posture_and_warns_in_text_when_off() {
    let project = tempdir().expect("project");
    let mut config = NibConfig::default();
    config
        .llm
        .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
    save_nib_config_full(project.path(), &mut config).expect("config");

    let output = set_approval_mode(project.path(), "off").expect("select off");
    assert!(output.contains("Configured approval preset set to 'off'"));
    assert!(output.contains("Configured approval preset: off"));
    assert!(output.contains("approval behavior: off"));
    assert!(output.contains("WARNING: BROADER/OFF"));
    assert!(output.contains("never overrides them"));
}

#[test]
fn status_reports_resolved_transport_bounded_context_and_control_safe_values() {
    let project = tempdir().expect("project");
    let mut config = NibConfig::default();
    config.llm.context_length = 32;
    let secret = "status/credential";
    config.llm.add_or_update_provider(
        "mock".to_string(),
        "mock\u{1b}[31m\nINJECTED-prefix-status\\/credential-c3RhdHVzL2NyZWRlbnRpYWw=".to_string(),
        None,
    );
    config.llm.providers.insert(
        "openai".to_string(),
        ProviderEntry {
            model: "inactive-model".to_string(),
            api_key: Some(secret.to_string()),
            ..ProviderEntry::default()
        },
    );
    save_nib_config_full(project.path(), &mut config).expect("config");
    let store = SessionStore::for_project(project.path()).expect("store");
    let session = store.try_create_session().expect("session");
    store
        .try_append_message(&session.id, "user", &"context ".repeat(200))
        .expect("persist context");
    store
        .update_session(&session.id, |session| {
            session.display_name = Some("name\u{1b}[2J\nINJECTED_NAME".to_string());
            session.tool_calls.push(crate::session::ToolCallRecord {
                worktree_path: Some("/tmp/stale-historical-worktree".to_string()),
                ..Default::default()
            });
            Ok(())
        })
        .expect("unsafe legacy name fixture");

    let output = format_session_status(
        project.path(),
        "status-profile",
        &store,
        &session.id,
        "idle",
    )
    .expect("status");
    assert!(output.contains("profile status-profile"), "{output}");
    assert!(output.contains("worktree -"), "{output}");
    assert!(!output.contains("stale-historical-worktree"), "{output}");
    assert!(output.contains("transport local"), "{output}");
    assert!(output.contains("context ~"), "{output}");
    assert!(output.contains("/32"), "{output}");
    assert!(output.contains("Configured approval preset: manual"));
    assert!(output.contains("platform sandbox:"));
    assert!(output.contains("Verification: none"));
    assert!(!output.contains('\u{1b}'));
    assert!(!output.contains("\nINJECTED"));
    assert!(!output.contains("\nINJECTED_NAME"));
    assert!(!output.contains(secret));
    assert!(!output.contains(r"status\/credential"));
    assert!(!output.contains("c3RhdHVzL2NyZWRlbnRpYWw"));
    assert!(output.contains("[REDACTED]"));
    assert!(output.len() < 4_096, "status output must remain bounded");
}

#[test]
fn status_and_plan_projection_show_verification_state_and_authority() {
    let project = tempdir().expect("project");
    let mut config = NibConfig::default();
    config
        .llm
        .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
    save_nib_config_full(project.path(), &mut config).expect("config");
    let store = SessionStore::for_project(project.path()).expect("store");
    let mut session = store.try_create_session().expect("session");
    let mut obligation = crate::session::VerificationObligation::pending_tool(
        "required-project-gate",
        "run the project gate",
        vec![".".to_string()],
        "run_terminal",
        serde_json::json!({"command": "task verify", "affected_paths": ["."]}),
        crate::session::VerificationExpectedOutcome::Success,
    )
    .expect("verification contract");
    obligation.authority = crate::session::VerificationAuthority::Project;
    session.plan = Some(crate::session::Plan::new(
        "verify the change",
        vec![crate::session::PlanStep {
            description: "verify".to_string(),
            status: "Pending".to_string(),
            outcome: None,
            attempts: 0,
            updated_at: None,
            verification_obligations: vec![obligation],
            content_generation: 0,
        }],
    ));
    store.save(&mut session).expect("verification session");

    let status = format_session_status(
        project.path(),
        "verification-profile",
        &store,
        &session.id,
        "idle",
    )
    .expect("status");
    assert!(status.contains("required-project-gate | pending | project"));
    assert!(status.contains("Planned 1 to-do"));
    assert!(status.contains("○ verify"));
    let plan = session.plan.as_ref().expect("plan");
    let activity = plan_activity(plan, &[]);
    assert_eq!(
        activity,
        plan_progress_activity(&plan_progress_from_plan(plan, &[]), &[])
    );
    assert!(activity.title.contains("1 verification pending"));
    assert!(activity
        .body
        .contains("verify required-project-gate [pending; project]"));
}

#[test]
fn tui_chrome_is_compact_while_status_keeps_full_diagnostics() {
    let project = tempdir().expect("project");
    let mut config = NibConfig::default();
    config.llm.context_length = 1_000;
    config.execution.provider = "internal".to_string();
    config
        .llm
        .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
    save_nib_config_full(project.path(), &mut config).expect("config");
    let store = SessionStore::for_project(project.path()).expect("store");
    let session = store.try_create_session().expect("session");
    store
        .try_append_message(&session.id, "user", "inspect the TUI")
        .expect("message");
    let persisted = store.load(&session.id).expect("persisted session");

    let chrome = format_tui_interaction_chrome(project.path(), Some(&persisted), &session.id)
        .expect("compact chrome");
    assert_eq!(chrome.folder, folder_label(project.path()));
    assert!(
        chrome.branch == "-" || chrome.branch.contains("master") || chrome.branch.contains("main"),
        "{}",
        chrome.branch
    );
    assert!(!chrome.folder.contains(&session.id), "{}", chrome.folder);
    assert!(!chrome.branch.contains(&session.id), "{}", chrome.branch);
    assert_eq!(chrome.model, "mock-model");
    assert_eq!(chrome.approval, "manual");
    assert!(!chrome.model.contains("transport"));

    config
        .llm
        .providers
        .get_mut("mock")
        .expect("mock provider")
        .model = "model-with-a-deliberately-long-identifier".to_string();
    save_nib_config_full(project.path(), &mut config).expect("long model config");
    let resumed = format_tui_interaction_chrome(project.path(), Some(&persisted), &session.id)
        .expect("width-aware chrome");
    assert_eq!(resumed.folder, folder_label(project.path()));
    assert_eq!(resumed.approval, "manual");
    assert!(
        resumed
            .model
            .contains("model-with-a-deliberately-long-identifier"),
        "{}",
        resumed.model
    );

    let detailed = format_session_status(project.path(), "default", &store, &session.id, "idle")
        .expect("detailed status");
    assert!(detailed.contains(&session.id), "{detailed}");
    assert!(detailed.contains("transport local"), "{detailed}");
    assert!(
        detailed.contains("Effective execution posture"),
        "{detailed}"
    );
}

#[test]
fn session_resolution_keeps_tui_origin_truthful_and_missing_notice_short() {
    let missing = SessionResolution::RequestedMissing {
        requested: "requested-session-123456789".to_string(),
        created: "created-session-987654321".to_string(),
    };
    assert_eq!(missing.tui_origin(), "new");
    let notice = missing.tui_notice().expect("missing session notice");
    assert!(notice.contains("requeste"));
    assert!(notice.contains("created-"));
    assert!(!notice.contains("requested-session-123456789"));
    assert!(!notice.contains("created-session-987654321"));

    assert_eq!(
        SessionResolution::Resumed("session-a".to_string()).tui_origin(),
        "resumed"
    );
    assert!(SessionResolution::Created("session-b".to_string())
        .tui_notice()
        .is_none());
}

#[test]
fn tui_chrome_never_exposes_a_managed_worktree_session_uuid() {
    let repository = git_repository();
    let mut config = NibConfig::default();
    config
        .llm
        .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
    save_nib_config_full(repository.path(), &mut config).expect("config");
    let store = SessionStore::for_project(repository.path()).expect("store");
    let session = store.try_create_session().expect("session");
    let mut manager =
        crate::integrations::worktree::WorktreeManager::new(repository.path().to_path_buf());
    let path = manager
        .create_for_session(&session.id)
        .expect("managed worktree");
    assert!(path.to_string_lossy().contains(&session.id));

    let persisted = store.load(&session.id).expect("persisted session");
    let chrome = format_tui_interaction_chrome(repository.path(), Some(&persisted), &session.id)
        .expect("managed-worktree chrome");
    assert!(!chrome.folder.contains(&session.id), "{}", chrome.folder);
    assert!(!chrome.branch.contains(&session.id), "{}", chrome.branch);
    assert!(!chrome.model.contains(&session.id), "{}", chrome.model);
    assert_eq!(chrome.branch, "session");
}

#[test]
fn tui_chrome_shows_model_approval_branch_worktree_and_folder() {
    let repository = git_repository();
    let mut config = NibConfig::default();
    config
        .llm
        .add_or_update_provider("mock".to_string(), "mock-model".to_string(), None);
    save_nib_config_full(repository.path(), &mut config).expect("config");
    let store = SessionStore::for_project(repository.path()).expect("store");
    let session = store.try_create_session().expect("session");
    let persisted = store.load(&session.id).expect("persisted session");
    let chrome = format_tui_interaction_chrome(repository.path(), Some(&persisted), &session.id)
        .expect("chrome");
    assert_eq!(chrome.folder, folder_label(repository.path()));
    assert!(
        chrome.branch.contains("master") || chrome.branch.contains("main"),
        "{}",
        chrome.branch
    );
    assert_eq!(chrome.model, "mock-model");
    assert_eq!(chrome.approval, "manual");
    assert!(!chrome.context.is_empty());
    assert!(chrome.context.starts_with("ctx"), "{}", chrome.context);
}

#[test]
fn model_commands_rename_and_session_history_share_sensitive_projection() {
    let project = tempdir().expect("project");
    let secret = "surface/credential";
    let encoded = "c3VyZmFjZS9jcmVkZW50aWFs";
    let mut config = NibConfig::default();
    config.llm.add_or_update_provider(
        "mock".to_string(),
        format!("prefix {secret} surface\\/credential {encoded} \u{1b}[2J"),
        None,
    );
    config.llm.providers.insert(
        "inactive-openai".to_string(),
        ProviderEntry {
            model: "safe-inactive-model".to_string(),
            api_key: Some(secret.to_string()),
            ..ProviderEntry::default()
        },
    );
    save_nib_config_full(project.path(), &mut config).expect("sensitive config");
    let store = SessionStore::for_project(project.path()).expect("session store");
    let session = store.create_session();
    let unsafe_history =
        format!("raw={secret} json=surface\\/credential b64={encoded} \u{1b}[31m\u{202e}");
    store
        .try_append_message(&session.id, "user", &unsafe_history)
        .expect("legacy unsafe message");

    let InteractiveEffect::Output(providers) = execute_interactive_command(
        InteractiveCommand::Providers,
        project.path(),
        &store,
        &session.id,
    )
    .expect("provider output") else {
        panic!("provider output effect")
    };
    for forbidden in [
        secret,
        r"surface\/credential",
        encoded,
        "\u{1b}",
        "\u{202e}",
    ] {
        assert!(
            !providers.contains(forbidden),
            "provider output: {providers:?}"
        );
    }

    let confirmation =
        set_active_model(project.path(), r"surface\/credential").expect("model confirmation");
    assert!(!confirmation.contains(secret));
    assert!(!confirmation.contains(r"surface\/credential"));

    let InteractiveEffect::Output(rename) = execute_interactive_command(
        InteractiveCommand::Rename {
            name: format!("{secret} {encoded} \u{1b}[2J"),
        },
        project.path(),
        &store,
        &session.id,
    )
    .expect("rename output") else {
        panic!("rename output effect")
    };
    assert!(!rename.contains(secret));
    assert!(!rename.contains(encoded));
    assert!(!rename.contains('\u{1b}'));

    let candidate =
        interactive_session_candidate(&store, &session.id, &session.id).expect("safe preview");
    let persisted = store.load(&session.id).expect("session");
    let activities = project_session_activities(&persisted, store.public_sensitive_values());
    let public_history = format!("{}\n{activities:?}", candidate.preview);
    for forbidden in [
        secret,
        r"surface\/credential",
        encoded,
        "\u{1b}",
        "\u{202e}",
    ] {
        assert!(
            !public_history.contains(forbidden),
            "public history: {public_history:?}"
        );
    }
}

#[test]
fn first_user_goal_names_an_unnamed_session_once() {
    let project = tempdir().expect("project");
    let store = SessionStore::for_project(project.path()).expect("session store");
    let session = store.create_session();
    let assigned = maybe_assign_session_display_name(
        &store,
        &session.id,
        "  Inspect the failing wrap tests\nmore detail",
    )
    .expect("auto-name");
    assert_eq!(assigned.as_deref(), Some("Inspect the failing wrap tests"));
    assert_eq!(
        maybe_assign_session_display_name(&store, &session.id, "later goal")
            .expect("existing name is kept"),
        None
    );
    assert_eq!(
        store
            .load(&session.id)
            .expect("named session")
            .display_name
            .as_deref(),
        Some("Inspect the failing wrap tests")
    );
    assert_eq!(
        session_title_from_conversation("Continue with approved plan step: x", &[]),
        None
    );
    assert_eq!(session_title_from_conversation("   \n  ", &[]), None);
}

#[test]
fn activity_projection_redacts_before_the_first_body_bound() {
    let directory = tempdir().expect("session directory");
    let mut session = SessionStore::at_dir(directory.path().join("sessions"))
        .try_create_session()
        .expect("session");
    let secret = format!("boundary/activity/{}", "s".repeat(256));
    let json_encoded = secret.replace('/', r"\/");
    let straddles_body_bound = |value: &str| {
        format!(
            "{}{}-safe-tail",
            "p".repeat(MAX_ACTIVITY_BODY_BYTES - value.len() / 2),
            value
        )
    };

    session.messages.push(crate::session::SessionMessage {
        index: 0,
        role: "user".to_string(),
        content: straddles_body_bound(&secret),
        timestamp: None,
        attachments: Vec::new(),
    });
    session.summary = Some(straddles_body_bound(&json_encoded));
    session.summary_index = 1;
    session.plan = Some(crate::session::Plan::new(
        "boundary projection",
        [straddles_body_bound(&secret), "follow up".to_string()]
            .into_iter()
            .enumerate()
            .map(|(index, description)| crate::session::PlanStep {
                description,
                status: if index == 0 { "InProgress" } else { "Pending" }.to_string(),
                outcome: None,
                attempts: 1,
                updated_at: None,
                verification_obligations: Vec::new(),
                content_generation: 0,
            })
            .collect(),
    ));

    let sensitive_values = vec![secret.clone()];
    let projected = project_session_activities(&session, &sensitive_values);
    let mut live = Vec::new();
    let mut live_state = None;
    apply_stream_event(
        &mut live,
        StreamEvent::Content(straddles_body_bound(&json_encoded)),
        &mut live_state,
        &sensitive_values,
    );
    let public = format!("{projected:?}\n{live:?}");
    for forbidden in [
        &secret[..secret.len() / 2 - 8],
        &json_encoded[..json_encoded.len() / 2 - 8],
    ] {
        assert!(
            !public.contains(forbidden),
            "credential prefix survived redaction-before-bounding: {forbidden:?}"
        );
    }
    assert!(public.matches("[REDACTED]").count() >= 3, "{public:?}");
    assert!(projected
        .iter()
        .find(|activity| activity.kind == ActivityKind::Plan)
        .is_some_and(|activity| {
            !activity.body.contains(&secret)
                && activity.body.contains("[REDACTED]")
                && activity.body.len() <= MAX_ACTIVITY_BODY_BYTES
        }));
    assert!(projected
        .iter()
        .chain(live.iter())
        .all(|activity| activity.body.len() <= MAX_ACTIVITY_BODY_BYTES));
}

#[test]
fn status_context_excludes_the_raw_summarized_prefix_and_never_exceeds_limit() {
    let project = tempdir().expect("project");
    let store = SessionStore::for_project(project.path()).expect("store");
    let session = store.try_create_session().expect("session");
    store
        .try_append_message(&session.id, "user", &"old raw prefix ".repeat(500))
        .expect("prefix");
    store
        .try_append_message(&session.id, "assistant", "current tail")
        .expect("tail");
    store
        .update_session(&session.id, |session| {
            session.summary = Some("bounded summary".to_string());
            session.summary_index = 1;
            Ok(())
        })
        .expect("summary");
    let mut persisted = store
        .load_result(&session.id)
        .expect("load")
        .expect("session");
    let before = persisted_context_usage(Some(&persisted), 32);
    persisted.messages[0].content = "different ignored prefix ".repeat(5_000);
    let after = persisted_context_usage(Some(&persisted), 32);
    assert_eq!(before, after);
    assert!(after <= 32, "{after}");
}

#[test]
fn tool_lifecycle_mutates_one_folded_summary_entry() {
    let mut activities = Vec::new();
    let mut state = None;
    let invocation_id = ToolInvocationId::new();
    apply_stream_event(
        &mut activities,
        StreamEvent::ToolCallChunk {
            invocation_id,
            index: 0,
            name: Some("list_directory".to_string()),
            arguments: Some("{}".to_string()),
        },
        &mut state,
        &[],
    );
    apply_stream_event(
        &mut activities,
        StreamEvent::ToolStarted {
            invocation_id,
            tool_name: "list_directory".to_string(),
        },
        &mut state,
        &[],
    );
    apply_stream_event(
        &mut activities,
        StreamEvent::ToolCompleted {
            invocation_id,
            tool_name: "list_directory".to_string(),
            success: true,
            output: Some(serde_json::json!({
                "entries": [{"path": "README.md"}],
                "entries_scanned": 1,
                "truncated": false
            })),
            error: None,
        },
        &mut state,
        &[],
    );
    assert_eq!(activities.len(), 1);
    assert_eq!(activities[0].kind, ActivityKind::Tool);
    assert_eq!(activities[0].title, "list_directory ok · 1 entries");
    assert!(activities[0].render_line().contains("◆ tool"));
    assert!(!activities[0].render_line().contains("README.md"));
    assert!(!activities[0].render_line().contains("{\"entries\""));
    assert!(activities[0].folded);
    assert!(activities[0].body.contains("README.md"));
    let mut expanded = activities[0].clone();
    expanded.folded = false;
    assert!(expanded.render_line().contains("README.md"));
}

#[test]
fn same_name_tool_calls_keep_distinct_blocks_by_invocation() {
    let first = ToolInvocationId::new();
    let second = ToolInvocationId::new();
    let mut activities = Vec::new();
    let mut state = None;

    for (index, invocation_id, command) in
        [(0, first, "printf first"), (1, second, "printf second")]
    {
        apply_stream_event(
            &mut activities,
            StreamEvent::ToolCallChunk {
                invocation_id,
                index,
                name: Some("run_terminal".to_string()),
                arguments: Some(serde_json::json!({"command": command}).to_string()),
            },
            &mut state,
            &[],
        );
    }

    apply_stream_event(
        &mut activities,
        StreamEvent::ToolStarted {
            invocation_id: second,
            tool_name: "run_terminal".to_string(),
        },
        &mut state,
        &[],
    );
    apply_stream_event(
        &mut activities,
        StreamEvent::TerminalOutput {
            invocation_id: second,
            tool_name: "run_terminal".to_string(),
            stream: "stdout".to_string(),
            chunk: "second-stream\n".to_string(),
            background_task_id: None,
        },
        &mut state,
        &[],
    );
    apply_stream_event(
        &mut activities,
        StreamEvent::ToolCompleted {
            invocation_id: first,
            tool_name: "run_terminal".to_string(),
            success: true,
            output: Some(serde_json::json!({"exit_code": 0, "stdout": "first-result"})),
            error: None,
        },
        &mut state,
        &[],
    );

    assert_eq!(activities.len(), 2);
    assert_eq!(activities[0].tool_invocation_id, Some(first));
    assert_eq!(activities[1].tool_invocation_id, Some(second));
    assert_eq!(
        activities[0].title,
        "run_terminal ok · printf first · exit 0"
    );
    assert_eq!(activities[0].body, "printf first\nfirst-result");
    assert_eq!(activities[1].title, "run_terminal running · printf second");
    assert_eq!(activities[1].body, "printf second\nsecond-stream");
}

#[test]
fn tool_titles_keep_argument_hints_across_lifecycle() {
    let mut activities = Vec::new();
    let mut state = None;
    let invocation_id = ToolInvocationId::new();
    apply_stream_event(
        &mut activities,
        StreamEvent::ToolCallChunk {
            invocation_id,
            index: 0,
            name: Some("read_file".to_string()),
            arguments: Some(r#"{"path":"src/tui/mod.rs"}"#.to_string()),
        },
        &mut state,
        &[],
    );
    assert_eq!(activities[0].title, "read_file requested · src/tui/mod.rs");
    apply_stream_event(
        &mut activities,
        StreamEvent::ToolStarted {
            invocation_id,
            tool_name: "read_file".to_string(),
        },
        &mut state,
        &[],
    );
    assert_eq!(activities.len(), 1);
    assert_eq!(activities[0].title, "read_file running · src/tui/mod.rs");
    apply_stream_event(
        &mut activities,
        StreamEvent::ToolCompleted {
            invocation_id,
            tool_name: "read_file".to_string(),
            success: true,
            output: Some(serde_json::json!({"content": "a\nb\nc\n"})),
            error: None,
        },
        &mut state,
        &[],
    );
    assert_eq!(
        activities[0].title,
        "read_file ok · src/tui/mod.rs · 3 lines"
    );
    assert!(activities[0].folded);
    assert!(activities[0].body.contains("a\\nb\\nc\\n"));
    assert!(!activities[0].render_line().contains("a\\nb\\nc\\n"));
    assert!(activities[0].display_text().contains("◆ tool"));
}

#[test]
fn common_read_tools_keep_bounded_expandable_result_detail() {
    let secret = "private-search-value";
    let output = serde_json::json!({
        "matches": [{"path": "src/lib.rs", "line": 4, "text": secret}],
        "padding": "x".repeat(MAX_ACTIVITY_BODY_BYTES * 2)
    });
    let (status, summary, detail) = summarize_tool_result("grep", true, Some(&output), None);
    assert_eq!(status, "ok");
    assert_eq!(summary, "1 matches");
    let body = bounded_activity_body(&detail, &[secret.to_string()]);
    assert!(body.contains("[REDACTED]"));
    assert!(!body.contains(secret));
    assert!(body.len() <= MAX_ACTIVITY_BODY_BYTES);
}

#[test]
fn run_terminal_titles_and_bodies_keep_the_full_command() {
    let mut activities = Vec::new();
    let mut state = None;
    let command = "git add workspace/agents/memory/changelog.md workspace/agents/memory/decisions.md && git status --short";
    let arguments = format!(
        r#"{{"command":"{command}","cwd":"/home/e/work/projects/nib","background":false}}"#
    );
    let invocation_id = ToolInvocationId::new();
    apply_stream_event(
        &mut activities,
        StreamEvent::ToolCallChunk {
            invocation_id,
            index: 0,
            name: Some("run_terminal".to_string()),
            arguments: Some(arguments),
        },
        &mut state,
        &[],
    );
    assert!(
        activities[0]
            .title
            .contains("git add workspace/agents/memory/changelog.md"),
        "title should keep the command: {}",
        activities[0].title
    );
    assert!(
        activities[0].body.contains(command),
        "{}",
        activities[0].body
    );
    assert!(
        activities[0].body.contains("in /home/e/work/projects/nib"),
        "{}",
        activities[0].body
    );
    assert!(
        activities[0].folded,
        "running command stays folded; the TUI nests a spinner and keeps the command in the body"
    );
    apply_stream_event(
        &mut activities,
        StreamEvent::TerminalOutput {
            invocation_id,
            tool_name: "run_terminal".to_string(),
            stream: "stdout".to_string(),
            chunk: "M changelog.md\n".to_string(),
            background_task_id: None,
        },
        &mut state,
        &[],
    );
    assert!(
        activities[0].body.contains(command),
        "{}",
        activities[0].body
    );
    assert!(
        activities[0].body.contains("M changelog.md"),
        "{}",
        activities[0].body
    );
    apply_stream_event(
        &mut activities,
        StreamEvent::ToolCompleted {
            invocation_id,
            tool_name: "run_terminal".to_string(),
            success: true,
            output: Some(serde_json::json!({"exit_code": 0, "stdout": "M changelog.md\n"})),
            error: None,
        },
        &mut state,
        &[],
    );
    assert!(
        activities[0].title.contains("exit 0"),
        "{}",
        activities[0].title
    );
    assert!(
        activities[0].body.contains(command),
        "{}",
        activities[0].body
    );
    assert!(
        activities[0].body.contains("M changelog.md"),
        "{}",
        activities[0].body
    );
}

#[test]
fn speech_thought_and_tool_blocks_use_distinct_presentation() {
    let speech = ActivityEntry::new(ActivityKind::Assistant, "", "Here is the answer");
    assert_eq!(speech.display_text(), "nib\n  Here is the answer");
    let user = ActivityEntry::new(ActivityKind::User, "", "inspect wrap");
    assert_eq!(user.display_text(), "you\n  inspect wrap");
    let thought = ActivityEntry::new(ActivityKind::Thinking, "planning", "next files").folded();
    assert_eq!(thought.display_text(), "▸ Thought");
    let expanded = ActivityEntry::new(ActivityKind::Thinking, "Thought for 14s", "next files");
    assert_eq!(expanded.display_text(), "▾ Thought for 14s\n┊ next files");
    let plan = ActivityEntry::new(
        ActivityKind::Plan,
        "Working on 1 to-do",
        "◐ write tests".to_string(),
    );
    assert!(plan.display_text().starts_with("Working on 1 to-do"));
    assert!(plan.display_text().contains("◐ write tests"));
    let tool = ActivityEntry::new(
        ActivityKind::Tool,
        "read_file running · src/lib.rs",
        "line one",
    );
    assert!(tool.display_text().starts_with("◆ tool  read_file"));
    assert!(tool.display_text().contains("│ line one"));
    assert!(!speech.display_text().contains("◆ tool"));
    assert!(!speech.display_text().starts_with("thought"));
}

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn typed_activities_keep_local_work_distinct_from_assistant_speech() {
    let directory = tempdir().expect("dir");
    let mut session = SessionStore::at_dir(directory.path().join("s"))
        .try_create_session()
        .expect("session");
    session.messages.push(crate::session::SessionMessage {
        index: 0,
        role: "user".to_string(),
        content: "inspect wrap".to_string(),
        timestamp: None,
        attachments: Vec::new(),
    });
    session.plan = Some(crate::session::Plan::new(
        "inspect wrap",
        ["write tests", "review result"]
            .into_iter()
            .map(|description| crate::session::PlanStep {
                description: description.to_string(),
                status: if description == "write tests" {
                    "InProgress".to_string()
                } else {
                    "Pending".to_string()
                },
                outcome: None,
                attempts: 1,
                updated_at: None,
                verification_obligations: Vec::new(),
                content_generation: 0,
            })
            .collect(),
    ));
    session.messages.push(crate::session::SessionMessage {
        index: 1,
        role: "user".to_string(),
        content: "Continue with approved plan step: write tests".to_string(),
        timestamp: None,
        attachments: Vec::new(),
    });
    let projected = project_session_activities(&session, &[]);
    assert!(projected
        .iter()
        .any(|entry| entry.kind == ActivityKind::User));
    assert!(projected
        .iter()
        .any(|entry| entry.kind == ActivityKind::Plan));
    let plan = projected
        .iter()
        .find(|entry| entry.kind == ActivityKind::Plan && entry.title.contains("to-do"))
        .expect("plan todo list");
    assert!(plan.body.contains("◐ write tests"), "{}", plan.body);
    assert!(projected.iter().all(|entry| {
        entry.title != "continuing approved step"
            && !(entry.kind == ActivityKind::User
                && entry.body.starts_with("Continue with approved plan step:"))
    }));
    assert!(format_current_plan(Some(&session), &[]).contains("◐ write tests"));
    assert_eq!(ActivityKind::Assistant.role_label(), "nib");

    let mut live = Vec::new();
    let mut state = None;
    apply_stream_event(
        &mut live,
        StreamEvent::StateTransition {
            state: "planning".to_string(),
        },
        &mut state,
        &[],
    );
    assert_eq!(state.as_deref(), Some("planning"));
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].kind, ActivityKind::Thinking);
    assert_eq!(live[0].title, "Thought");
    assert!(live[0].folded);
    assert!(live[0].display_text().starts_with("▸ Thought"));
    apply_stream_event(
        &mut live,
        StreamEvent::PlanGenerated {
            step_count: 2,
            steps: vec!["inspect wrap".to_string(), "write tests".to_string()],
        },
        &mut state,
        &[],
    );
    let mut live_plan = crate::session::Plan::new(
        "inspect wrap",
        ["inspect wrap", "write tests"]
            .into_iter()
            .map(|description| crate::session::PlanStep {
                description: description.to_string(),
                status: "Pending".to_string(),
                outcome: None,
                attempts: 0,
                updated_at: None,
                verification_obligations: Vec::new(),
                content_generation: 0,
            })
            .collect(),
    );
    apply_stream_event(
        &mut live,
        StreamEvent::PlanProgress(plan_progress_from_plan(&live_plan, &[])),
        &mut state,
        &[],
    );
    assert_eq!(live[1].kind, ActivityKind::Plan);
    assert_eq!(live[1].title, "Planned 2 to-dos");
    assert_eq!(live[1].body, "○ inspect wrap\n○ write tests");
    live_plan.approve();
    apply_stream_event(
        &mut live,
        StreamEvent::PlanProgress(plan_progress_from_plan(&live_plan, &[])),
        &mut state,
        &[],
    );
    assert_eq!(live[1].body, "◐ inspect wrap\n○ write tests");
    apply_stream_event(
        &mut live,
        StreamEvent::Content("hello".to_string()),
        &mut state,
        &[],
    );
    apply_stream_event(
        &mut live,
        StreamEvent::ToolStarted {
            invocation_id: ToolInvocationId::new(),
            tool_name: "read_file".to_string(),
        },
        &mut state,
        &[],
    );
    live_plan.complete_current_step("inspected");
    apply_stream_event(
        &mut live,
        StreamEvent::PlanProgress(plan_progress_from_plan(&live_plan, &[])),
        &mut state,
        &[],
    );
    assert_eq!(live[1].body, "✓ inspect wrap\n◐ write tests");
    apply_stream_event(
        &mut live,
        StreamEvent::Reconciled {
            outcome: "step_completed".to_string(),
        },
        &mut state,
        &[],
    );
    live_plan.complete_current_step("tested");
    apply_stream_event(
        &mut live,
        StreamEvent::PlanProgress(plan_progress_from_plan(&live_plan, &[])),
        &mut state,
        &[],
    );
    assert_eq!(live[1].body, "✓ inspect wrap\n✓ write tests");
    apply_stream_event(
        &mut live,
        StreamEvent::Reconciled {
            outcome: "completed".to_string(),
        },
        &mut state,
        &[],
    );
    assert_eq!(live[0].kind, ActivityKind::Thinking);
    assert!(
        live[0].title.starts_with("Thought for "),
        "{}",
        live[0].title
    );
    assert_eq!(live[1].kind, ActivityKind::Plan);
    assert_eq!(live[2].kind, ActivityKind::Assistant);
    assert_eq!(live[3].kind, ActivityKind::Tool);
    assert_eq!(live[4].kind, ActivityKind::Reconcile);
    assert_eq!(
        live.iter()
            .filter(|entry| entry.kind == ActivityKind::Reconcile)
            .count(),
        1
    );
    let mut unreconciled = Vec::new();
    apply_stream_event(
        &mut unreconciled,
        StreamEvent::End("local_error".to_string()),
        &mut state,
        &[],
    );
    assert_eq!(unreconciled[0].kind, ActivityKind::Failure);
    assert_eq!(unreconciled[0].title, "Run stopped");
    assert!(!unreconciled[0].title.contains("local_error"));
    assert!(unreconciled[0].body.contains("/status"));
}

#[test]
fn terminal_failure_closes_unfinished_proposals_and_preserves_completed_tools() {
    let mut activities = Vec::new();
    let mut state = None;
    let completed = ToolInvocationId::new();
    let proposed = ToolInvocationId::new();
    for event in [
        StreamEvent::ToolCallChunk {
            invocation_id: completed,
            index: 0,
            name: Some("read_file".to_string()),
            arguments: Some("{\"path\":\"README.md\"}".to_string()),
        },
        StreamEvent::ToolCompleted {
            invocation_id: completed,
            tool_name: "read_file".to_string(),
            success: true,
            output: Some(serde_json::json!({"content": "ok"})),
            error: None,
        },
        StreamEvent::ToolCallChunk {
            invocation_id: proposed,
            index: 0,
            name: Some("run_terminal".to_string()),
            arguments: Some("{\"command\":\"git status\"}".to_string()),
        },
        StreamEvent::End("worktree_preparation_failed".to_string()),
    ] {
        apply_stream_event(&mut activities, event, &mut state, &[]);
    }
    assert!(activities[0].title.starts_with("read_file ok"));
    assert!(activities[1].title.starts_with("run_terminal stopped"));
    assert!(activities[1].body.contains("did not run"));
    assert_eq!(activities[2].title, "Worktree preparation failed");
    assert!(activities[2].body.contains("nib doctor"));
}

#[test]
fn one_step_plan_stays_out_of_transcript_but_remains_inspectable() {
    let directory = tempdir().expect("dir");
    let mut session = SessionStore::at_dir(directory.path().join("s"))
        .try_create_session()
        .expect("session");
    let mut plan = crate::session::Plan::new(
        "inspect wrap",
        vec![crate::session::PlanStep {
            description: "write tests".to_string(),
            status: "Pending".to_string(),
            outcome: None,
            attempts: 0,
            updated_at: None,
            verification_obligations: Vec::new(),
            content_generation: 0,
        }],
    );
    plan.approve();
    session.plan = Some(plan);
    let projected = project_session_activities(&session, &[]);
    assert!(projected
        .iter()
        .all(|entry| entry.kind != ActivityKind::Plan));
    assert!(format_current_plan(Some(&session), &[]).contains("◐ write tests"));
    let mut live = Vec::new();
    let mut state = None;
    apply_stream_event(
        &mut live,
        StreamEvent::PlanProgress(plan_progress_from_plan(
            session.plan.as_ref().expect("plan"),
            &[],
        )),
        &mut state,
        &[],
    );
    assert!(live.is_empty());
}

#[test]
fn live_multi_step_progress_matches_persisted_stopped_blocked_and_completed_plan() {
    let mut plan = crate::session::Plan::new(
        "check and fix",
        ["check", "fix"]
            .into_iter()
            .map(|description| crate::session::PlanStep {
                description: description.to_string(),
                status: "Pending".to_string(),
                outcome: None,
                attempts: 0,
                updated_at: None,
                verification_obligations: Vec::new(),
                content_generation: 0,
            })
            .collect(),
    );
    plan.approve();
    let mut live = Vec::new();
    let mut state = None;
    for expected in [
        "× check\n○ fix",
        "! check\n○ fix",
        "✓ check\n◐ fix",
        "✓ check\n✓ fix",
    ] {
        match expected {
            "× check\n○ fix" => plan.steps[0].status = "Cancelled".to_string(),
            "! check\n○ fix" => plan.steps[0].status = "Blocked".to_string(),
            "✓ check\n◐ fix" => {
                plan.steps[0].status = "InProgress".to_string();
                plan.complete_current_step("checked");
            }
            _ => plan.complete_current_step("fixed"),
        }
        apply_stream_event(
            &mut live,
            StreamEvent::PlanProgress(plan_progress_from_plan(&plan, &[])),
            &mut state,
            &[],
        );
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].body, expected);
        assert_eq!(live[0], plan_activity(&plan, &[]));
    }
    assert_eq!(live[0].title, "Completed 2 to-dos");
}

#[test]
fn truncated_progress_keeps_saved_status_and_separate_plan_identity() {
    let mut plan = crate::session::Plan::new(
        "long plan",
        [
            "x".repeat(MAX_ACTIVITY_BODY_BYTES * 2),
            "finish".to_string(),
        ]
        .into_iter()
        .map(|description| crate::session::PlanStep {
            description,
            status: "Pending".to_string(),
            outcome: None,
            attempts: 0,
            updated_at: None,
            verification_obligations: Vec::new(),
            content_generation: 0,
        })
        .collect(),
    );
    plan.approve();
    let mut live = Vec::new();
    let mut state = None;
    apply_stream_event(
        &mut live,
        StreamEvent::PlanProgress(plan_progress_from_plan(&plan, &[])),
        &mut state,
        &[],
    );
    assert!(live[0].body.len() <= MAX_ACTIVITY_BODY_BYTES);
    plan.complete_current_step("done");
    apply_stream_event(
        &mut live,
        StreamEvent::PlanProgress(plan_progress_from_plan(&plan, &[])),
        &mut state,
        &[],
    );
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].title, "Working on 2 to-dos");
    let other = crate::session::Plan::new(
        "other",
        ["one", "two"]
            .into_iter()
            .map(|description| crate::session::PlanStep {
                description: description.to_string(),
                status: "Pending".to_string(),
                outcome: None,
                attempts: 0,
                updated_at: None,
                verification_obligations: Vec::new(),
                content_generation: 0,
            })
            .collect(),
    );
    apply_stream_event(
        &mut live,
        StreamEvent::PlanProgress(plan_progress_from_plan(&other, &[])),
        &mut state,
        &[],
    );
    let other_before = live[1].clone();
    plan.complete_current_step("done");
    apply_stream_event(
        &mut live,
        StreamEvent::PlanProgress(plan_progress_from_plan(&plan, &[])),
        &mut state,
        &[],
    );
    assert_eq!(live.len(), 2);
    assert_eq!(live[0].title, "Completed 2 to-dos");
    assert_eq!(live[1], other_before);
}

#[test]
fn plain_progress_summarizes_multi_step_state_only() {
    let mut progress = crate::llm::PlanProgress {
        plan_id: "plan".to_string(),
        current_step_index: 1,
        complete: false,
        steps: ["inspect", "verify"]
            .into_iter()
            .map(|description| crate::llm::PlanProgressStep {
                description: description.to_string(),
                status: "Pending".to_string(),
                verification: Vec::new(),
            })
            .collect(),
    };
    progress.steps[0].status = "Completed".to_string();
    progress.steps[1].status = "InProgress".to_string();
    assert_eq!(
        display_stream_event(StreamEvent::PlanProgress(progress.clone())),
        Some(StreamDisplay::Status(
            "[plan] 1/2 done · working on: verify".to_string()
        ))
    );
    progress.steps.truncate(1);
    assert_eq!(
        display_stream_event(StreamEvent::PlanProgress(progress)),
        None
    );
}

#[test]
fn quiet_lifecycle_boundaries_keep_assistant_turns_separate() {
    for boundary in [
        StreamEvent::StateTransition {
            state: "reconciliation".to_string(),
        },
        StreamEvent::Reconciled {
            outcome: "step_completed".to_string(),
        },
    ] {
        let mut activities = Vec::new();
        let mut state = None;
        for event in [
            StreamEvent::Content("First ".to_string()),
            StreamEvent::Content("response.".to_string()),
            boundary,
        ] {
            apply_stream_event(&mut activities, event, &mut state, &[]);
        }
        assert_eq!(activities.len(), 1, "quiet events add no transcript rows");
        assert_eq!(activities[0].body, "First response.");
        assert!(activities[0].title.is_empty());

        for event in [
            StreamEvent::Content("Second ".to_string()),
            StreamEvent::Content("response.".to_string()),
            StreamEvent::Reconciled {
                outcome: "completed".to_string(),
            },
        ] {
            apply_stream_event(&mut activities, event, &mut state, &[]);
        }
        assert_eq!(
            activities
                .iter()
                .map(|entry| entry.kind)
                .collect::<Vec<_>>(),
            vec![
                ActivityKind::Assistant,
                ActivityKind::Assistant,
                ActivityKind::Reconcile
            ]
        );
        assert_eq!(activities[0].body, "First response.");
        assert_eq!(activities[1].body, "Second response.");
        assert!(activities[1].title.is_empty());
        assert_eq!(activities[2].title, "Run completed");
    }
}

#[test]
fn instruction_context_missing_reloads_with_the_recovery_reason() {
    let activity = project_session_event(
            &SessionEvent {
                index: 0,
                kind: "reconciliation".to_string(),
                details: serde_json::json!({
                    "outcome": "instruction_context_missing",
                    "reason": "Project instructions could not be loaded, so this run stopped before dependent work.\nAGENTS.md exceeds the instruction byte limit.\nRestore readable project instructions within the size limit, or increase llm.context_length, then retry the same plan."
                }),
                timestamp: None,
            },
            &[],
        )
        .expect("missing instructions stay visible");
    assert_eq!(activity.kind, ActivityKind::Failure);
    assert_eq!(activity.title, "project instructions unavailable");
    assert!(activity.body.contains("AGENTS.md exceeds"));
    assert!(activity.body.contains("llm.context_length"));
}

#[test]
fn blocked_and_repeated_tool_outcomes_reload_as_failures() {
    for outcome in ["blocked_step_unresolved", "repeated_tool_failure"] {
        for kind in ["reconciliation", "run_terminal"] {
            let activity = project_session_event(
                &SessionEvent {
                    index: 0,
                    kind: kind.to_string(),
                    details: serde_json::json!({"outcome": outcome}),
                    timestamp: None,
                },
                &[],
            )
            .expect("failed run remains visible after reload");
            assert_eq!(activity.kind, ActivityKind::Failure);
            assert_eq!(activity.title, terminal_outcome_message(outcome).title);
            assert!(!activity.body.is_empty());
        }
    }
}

#[test]
fn routine_lifecycle_projection_is_quiet_and_keeps_reconciliation() {
    let run_id = "0123456789abcdef0123456789abcdef";
    let started = project_session_event(
        &SessionEvent {
            index: 0,
            kind: "run_started".to_string(),
            details: serde_json::json!({"run_id": run_id, "private": "DO_NOT_RENDER"}),
            timestamp: None,
        },
        &[],
    );
    assert!(started.is_none());

    let cancelled = project_session_event(
        &SessionEvent {
            index: 1,
            kind: "run_terminal".to_string(),
            details: serde_json::json!({
                "run_id": run_id,
                "outcome": "cancelled_by_user",
                "error": "DO_NOT_RENDER"
            }),
            timestamp: None,
        },
        &[],
    )
    .expect("unmatched terminal remains visible");
    assert_eq!(cancelled.kind, ActivityKind::Cancellation);
    assert!(!cancelled.render_line().contains("DO_NOT_RENDER"));

    let reconciled = project_session_event(
        &SessionEvent {
            index: 2,
            kind: "reconciliation".to_string(),
            details: serde_json::json!({
                "run_id": run_id,
                "outcome": "cancelled_by_user",
                "error": "DO_NOT_RENDER"
            }),
            timestamp: None,
        },
        &[],
    )
    .expect("reconciliation activity");
    assert_eq!(reconciled.kind, ActivityKind::Cancellation);
    assert_eq!(reconciled.title, "Run cancelled");
    assert!(reconciled.body.contains("Check /status"));
    assert!(!reconciled.render_line().contains(run_id));
    assert!(!reconciled.render_line().contains("DO_NOT_RENDER"));
}

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn persisted_terminal_deduplication_requires_matching_projected_run_evidence() {
    let directory = tempdir().expect("dir");
    let template = SessionStore::at_dir(directory.path().join("s"))
        .try_create_session()
        .expect("session");
    let started = |run_id: &str| ("run_started", serde_json::json!({"run_id": run_id}));
    let reconciled = |outcome: &str| {
        (
            "reconciliation",
            serde_json::json!({"outcome": outcome, "continue": false}),
        )
    };
    let terminal = |run_id: &str, outcome: &str| {
        (
            "run_terminal",
            serde_json::json!({"run_id": run_id, "outcome": outcome}),
        )
    };
    for (events, expected) in [
        (
            vec![started("a"), terminal("a", "local_error")],
            vec!["Run stopped"],
        ),
        (
            vec![started("a"), terminal("a", "worktree_preparation_failed")],
            vec!["Worktree preparation failed"],
        ),
        (
            vec![
                started("a"),
                reconciled("completed"),
                terminal("a", "completed"),
            ],
            vec!["Run completed"],
        ),
        (
            vec![
                started("a"),
                reconciled("completed"),
                terminal("a", "local_error"),
            ],
            vec!["Run completed", "Run stopped"],
        ),
        (
            vec![
                started("a"),
                reconciled("completed"),
                terminal("a", "completed"),
                started("b"),
                terminal("b", "completed"),
            ],
            vec!["Run completed", "Run completed"],
        ),
        (
            vec![
                started("a"),
                reconciled("completed"),
                terminal("b", "completed"),
            ],
            vec!["Run completed", "Run completed"],
        ),
        (
            vec![reconciled("completed"), terminal("a", "completed")],
            vec!["Run completed", "Run completed"],
        ),
        (
            vec![
                started("a"),
                (
                    "reconciliation",
                    serde_json::json!({"outcome": "step_completed", "continue": true}),
                ),
                terminal("a", "step_completed"),
            ],
            vec!["Plan step completed"],
        ),
        (
            vec![
                started("a"),
                (
                    "reconciliation",
                    serde_json::json!({"run_id": "b", "outcome": "completed", "continue": false}),
                ),
                terminal("a", "completed"),
            ],
            vec!["Run completed", "Run completed"],
        ),
    ] {
        let mut session = template.clone();
        session.events = events
            .into_iter()
            .enumerate()
            .map(|(index, (kind, details))| SessionEvent {
                index,
                kind: kind.to_string(),
                details,
                timestamp: None,
            })
            .collect();
        let activities = project_session_activities(&session, &[]);
        assert_eq!(
            activities
                .iter()
                .map(|entry| entry.title.as_str())
                .collect::<Vec<_>>(),
            expected,
            "events={:?}",
            session.events,
        );
        assert!(activities
            .iter()
            .all(|entry| !entry.title.contains("local_error")));
    }
}

#[test]
fn conversation_projection_hides_transport_messages_and_keeps_legacy_safe() {
    let directory = tempdir().expect("dir");
    let mut session = SessionStore::at_dir(directory.path().join("s"))
        .try_create_session()
        .expect("session");
    for (index, (role, content)) in [
        ("user", "request"),
        ("assistant", "answer"),
        (
            "assistant",
            r#"{"content":null,"tool_calls":[{"name":"read_file"}]}"#,
        ),
        ("tool", r#"{"observations":[{"secret":"do-not-render"}]}"#),
        ("system", "local notice"),
        (
            &format!("future\n{}", "x".repeat(256)),
            "PRIVATE_LEGACY_SENTINEL",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        session.messages.push(crate::session::SessionMessage {
            index,
            role: role.to_string(),
            content: content.to_string(),
            timestamp: None,
            attachments: Vec::new(),
        });
    }

    let projected = project_session_activities(&session, &[]);
    assert_eq!(
        projected.iter().map(|entry| entry.kind).collect::<Vec<_>>(),
        vec![
            ActivityKind::User,
            ActivityKind::Assistant,
            ActivityKind::System,
            ActivityKind::System,
        ]
    );
    assert!(projected
        .iter()
        .all(|entry| !entry.render_line().contains("read_file")));
    assert!(projected
        .iter()
        .all(|entry| !entry.render_line().contains("do-not-render")));
    assert_eq!(projected[3].title, "unsupported legacy message role");
    assert_eq!(projected[3].body, "legacy message content omitted");
    assert!(!projected[3]
        .render_line()
        .contains("PRIVATE_LEGACY_SENTINEL"));
    assert!(!projected[3].title.contains('\n'));
}

#[test]
fn authoritative_events_are_ordered_typed_redacted_and_tool_deduplicated() {
    let timestamp = |second: u32| {
        chrono::DateTime::parse_from_rfc3339(&format!("2026-08-23T00:00:{second:02}Z"))
            .expect("timestamp")
            .with_timezone(&Utc)
    };
    let directory = tempdir().expect("dir");
    let mut session = SessionStore::at_dir(directory.path().join("s"))
        .try_create_session()
        .expect("session");
    session.messages.push(crate::session::SessionMessage {
        index: 0,
        role: "user".to_string(),
        content: "inspect".to_string(),
        timestamp: Some(timestamp(1)),
        attachments: Vec::new(),
    });
    session.events = vec![
        SessionEvent {
            index: 0,
            kind: "compression".to_string(),
            details: serde_json::json!({
                "before_tokens": 800,
                "after_tokens": 300,
                "summarized_through": 4,
                "raw_summary": "do-not-render",
            }),
            timestamp: Some(timestamp(2)),
        },
        SessionEvent {
            index: 1,
            kind: "tool_completed".to_string(),
            details: serde_json::json!({
                "tool_name": "read_file",
                "success": false,
                "error": "do-not-render",
            }),
            timestamp: Some(timestamp(3)),
        },
        SessionEvent {
            index: 2,
            kind: "reconciliation".to_string(),
            details: serde_json::json!({
                "outcome": "llm_stream_failed",
                "failure": {
                    "class": "transport",
                    "phase": "stream",
                    "retry": "retryable",
                    "incident_code": "LLM-NETWORK",
                    "message": "do-not-render",
                },
            }),
            timestamp: Some(timestamp(4)),
        },
        SessionEvent {
            index: 3,
            kind: "cancel_requested".to_string(),
            details: serde_json::json!({
                "reason": "cancelled_by_user",
                "state": "Running",
                "raw": "do-not-render",
            }),
            timestamp: Some(timestamp(5)),
        },
    ];
    session.tool_calls.push(crate::session::ToolCallRecord {
        tool_name: Some("read_file".to_string()),
        result: Some(serde_json::json!({"content": "do-not-render"})),
        timestamp: Some(timestamp(3)),
        ..crate::session::ToolCallRecord::default()
    });

    let projected = project_session_activities(&session, &[]);
    assert_eq!(
        projected.iter().map(|entry| entry.kind).collect::<Vec<_>>(),
        vec![
            ActivityKind::User,
            ActivityKind::Compression,
            ActivityKind::Tool,
            ActivityKind::Failure,
            ActivityKind::Cancellation,
        ]
    );
    assert_eq!(
        projected
            .iter()
            .filter(|entry| entry.kind == ActivityKind::Tool)
            .count(),
        1
    );
    let failure = projected
        .iter()
        .find(|entry| entry.kind == ActivityKind::Failure)
        .expect("failure evidence");
    assert!(failure.body.contains("The session was saved"));
    assert!(failure
        .body
        .contains("class=transport · phase=stream · retry=retryable · incident_code=LLM-NETWORK"));
    assert!(projected
        .iter()
        .all(|entry| !entry.render_line().contains("do-not-render")));
}

#[test]
fn legacy_role_projection_is_stable_across_json_roundtrip() {
    let legacy = r#"{
  "id": "legacy-ledger",
  "messages": [
    {"index": 0, "role": "system", "content": "legacy notice"},
    {"index": 1, "role": "future_agent_role", "content": "PRIVATE_LEGACY_SENTINEL"}
  ],
  "events": [
    {"index": 0, "kind": "legacy_event", "details": {"raw": "not projected"}}
  ]
}"#;
    let first: Session = serde_json::from_str(legacy).expect("legacy session");
    let encoded = serde_json::to_string(&first).expect("roundtrip encoding");
    let second: Session = serde_json::from_str(&encoded).expect("roundtrip session");

    let first_projection = project_session_activities(&first, &[]);
    let second_projection = project_session_activities(&second, &[]);
    assert_eq!(first_projection, second_projection);
    assert_eq!(first_projection[0].kind, ActivityKind::System);
    assert_eq!(first_projection[1].kind, ActivityKind::System);
    assert_eq!(first_projection[1].title, "unsupported legacy message role");
    assert_eq!(first_projection[1].body, "legacy message content omitted");
    assert!(!first_projection[1]
        .render_line()
        .contains("PRIVATE_LEGACY_SENTINEL"));
    assert_eq!(first_projection.len(), 2);
}

#[test]
fn path_completions_stay_inside_the_project_and_ignore_dot_entries() {
    let project = tempdir().expect("project");
    std::fs::create_dir_all(project.path().join("src")).expect("src");
    std::fs::write(project.path().join("src/main.rs"), "fn main() {}").expect("file");
    std::fs::write(project.path().join(".secret"), "nope").expect("dotfile");
    let matches = path_completions(project.path(), "see @src/m");
    assert!(
        matches
            .iter()
            .any(|item| item.insertion.ends_with("@src/main.rs")),
        "{matches:?}"
    );
    assert!(matches
        .iter()
        .all(|item| !item.insertion.contains(".secret")));
}

#[test]
fn path_attachments_are_structured_and_stay_inside_the_project() {
    let project = tempdir().expect("project");
    std::fs::create_dir_all(project.path().join("src")).expect("src");
    std::fs::write(project.path().join("src/main.rs"), "fn secret_body() {}").expect("file");
    std::fs::write(project.path().join(".secret"), "nope").expect("dotfile");
    let (text, attachments) =
        resolve_path_attachments(project.path(), "inspect @src/main.rs please")
            .expect("valid attachment");
    assert_eq!(text, "inspect @src/main.rs please");
    assert!(!text.contains("secret_body"));
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].path, "src/main.rs");
    assert!(
        resolve_path_attachments(project.path(), "see @../etc/passwd")
            .expect_err("escape")
            .contains("outside")
    );
    assert!(resolve_path_attachments(project.path(), "see @.secret")
        .expect_err("dotfile")
        .contains("outside"));
}

#[test]
fn terminal_scope_recovery_is_consistent_live_and_reloaded() {
    let cached = project_session_event(
        &SessionEvent {
            index: 0,
            kind: "reconciliation".to_string(),
            details: serde_json::json!({"outcome": "tool_scope_required"}),
            timestamp: None,
        },
        &[],
    )
    .expect("scope failure remains visible");
    assert_eq!(cached.kind, ActivityKind::Failure);
    assert!(cached.body.contains("affected_paths"));
    let mut live = Vec::new();
    apply_stream_event(
        &mut live,
        StreamEvent::Reconciled {
            outcome: "tool_scope_required".to_string(),
        },
        &mut None,
        &[],
    );
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].kind, ActivityKind::Failure);
    assert_eq!(live[0].title, cached.title);
    assert_eq!(live[0].body, cached.body);
    let status = display_reconciliation_status("tool_scope_required");
    assert!(
        matches!(status, StreamDisplay::Status(text) if text.contains("affected_paths") && !text.contains("context_length"))
    );
}
