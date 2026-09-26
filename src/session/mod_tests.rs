use super::*;
use tempfile::tempdir;

fn session_namespace_snapshot(path: &Path) -> Vec<(std::ffi::OsString, Vec<u8>)> {
    let mut snapshot = fs::read_dir(path)
        .expect("read session namespace")
        .map(|entry| {
            let entry = entry.expect("session namespace entry");
            (
                entry.file_name(),
                crate::fs_security::read_namespace_snapshot_file(&entry.path())
                    .expect("session namespace bytes"),
            )
        })
        .collect::<Vec<_>>();
    snapshot.sort_by(|left, right| left.0.cmp(&right.0));
    snapshot
}

fn session_namespace_shape(path: &Path) -> Vec<(std::ffi::OsString, u64)> {
    let mut snapshot = fs::read_dir(path)
        .expect("read session namespace shape")
        .map(|entry| {
            let entry = entry.expect("session namespace shape entry");
            (
                entry.file_name(),
                entry
                    .metadata()
                    .expect("session namespace shape metadata")
                    .len(),
            )
        })
        .collect::<Vec<_>>();
    snapshot.sort_by(|left, right| left.0.cmp(&right.0));
    snapshot
}

#[test]
fn failed_preparation_preserves_a_concurrently_adopted_session_namespace() {
    let root = tempfile::tempdir().expect("project root");
    let mut config = crate::config::NibConfig::default();
    crate::config::save_nib_config_full(root.path(), &mut config).expect("config");
    let deadline = Instant::now() + Duration::from_secs(5);
    let preflight_a = SessionStore::preflight_project_sessions_dir_until(root.path(), deadline)
        .expect("A preflight");
    let mut preparation_a = preflight_a.open_until(deadline).expect("A preparation");
    preparation_a
        .create_unpublished_session("session-a")
        .expect("A session");

    let preflight_b = SessionStore::preflight_project_sessions_dir_until(root.path(), deadline)
        .expect("B preflight after A publication");
    let mut preparation_b = preflight_b
        .open_until(deadline)
        .expect("B adopts namespace");
    preparation_b
        .create_unpublished_session("session-b")
        .expect("B session");
    let sessions_dir = preparation_b.store().sessions_dir().to_path_buf();
    let store_b = preparation_b.disarm();
    let committed_b = session_namespace_snapshot(&sessions_dir)
        .into_iter()
        .filter(|(name, _)| name != "session-a.json")
        .collect::<Vec<_>>();

    preparation_a
        .cleanup(deadline)
        .expect("A leaf-only compensation");

    assert_eq!(session_namespace_snapshot(&sessions_dir), committed_b);
    assert!(store_b.load_result("session-b").expect("load B").is_some());
    assert!(!sessions_dir.join("session-a.json").exists());
    assert!(sessions_dir.join(SESSION_DIRECTORY_IDENTITY_FILE).is_file());
}

#[test]
fn durable_planned_session_receipt_cleans_publication_before_identity_update() {
    let root = tempfile::tempdir().expect("project root");
    let mut config = crate::config::NibConfig::default();
    crate::config::save_nib_config_full(root.path(), &mut config).expect("config");
    let deadline = Instant::now() + Duration::from_secs(5);
    let preflight = SessionStore::preflight_project_sessions_dir_until(root.path(), deadline)
        .expect("session preflight");
    let mut preparation = preflight.open_until(deadline).expect("session preparation");
    preparation
        .plan_unpublished_session("planned-before-publication")
        .expect("durable session plan");
    let receipt = preparation
        .durable_receipt("planned-before-publication")
        .expect("receipt before publication");
    assert!(receipt.session_identity.is_none());

    preparation
        .create_unpublished_session("planned-before-publication")
        .expect("publish planned session");
    let sessions_dir = preparation.store().sessions_dir().to_path_buf();
    assert!(sessions_dir
        .join("planned-before-publication.json")
        .is_file());
    drop(preparation.disarm());

    SessionStorePreparation::cleanup_durable(&receipt, deadline)
        .expect("restart cleans the exact planned publication");
    assert!(!sessions_dir
        .join("planned-before-publication.json")
        .exists());
    assert!(!sessions_dir.exists());
}

#[test]
fn planned_namespace_cleanup_preserves_unrelated_or_ambiguous_state_byte_exactly() {
    let root = tempfile::tempdir().expect("project root");
    let mut config = crate::config::NibConfig::default();
    crate::config::save_nib_config_full(root.path(), &mut config).expect("config");
    let deadline = Instant::now() + Duration::from_secs(5);
    let preflight = SessionStore::preflight_project_sessions_dir_until(root.path(), deadline)
        .expect("session preflight");
    let plan = preflight
        .durable_preparation_plan_after_owned_worktree("hostile-preservation", deadline, None)
        .expect("durable namespace plan");
    let preparation = preflight
        .open_until_with_owned_worktree(deadline, None, Some(&plan), &mut || Ok(()))
        .expect("publish planned namespace");
    let sessions_dir = preparation.store().sessions_dir().to_path_buf();
    drop(preparation.disarm());
    std::fs::write(sessions_dir.join("hostile-sentinel"), b"preserve-me")
        .expect("hostile sentinel");
    let before = session_namespace_snapshot(&sessions_dir);

    let error = SessionStorePreparation::cleanup_planned_namespace(&plan, deadline)
        .expect_err("unrelated state must fail closed");
    assert!(
        error.contains("unrelated entry"),
        "unexpected error: {error}"
    );
    assert_eq!(session_namespace_snapshot(&sessions_dir), before);

    std::fs::remove_file(sessions_dir.join("hostile-sentinel")).expect("remove sentinel");
    let marker = sessions_dir.join(SESSION_DIRECTORY_IDENTITY_FILE);
    std::fs::write(&marker, b"ambiguous-marker").expect("replace marker content");
    let anchor = session_directory_identity_anchor(&marker).expect("identity anchor");
    let before_marker = session_namespace_snapshot(&sessions_dir);
    let before_anchor = std::fs::read(&anchor).expect("anchor bytes");
    let error = SessionStorePreparation::cleanup_planned_namespace(&plan, deadline)
        .expect_err("ambiguous marker must fail closed");
    assert!(
        error.contains("marker changed") || error.contains("marker is ambiguous"),
        "unexpected marker error: {error}"
    );
    assert_eq!(session_namespace_snapshot(&sessions_dir), before_marker);
    assert_eq!(
        std::fs::read(anchor).expect("preserved anchor"),
        before_anchor
    );
}

#[cfg(unix)]
const SESSION_COMMIT_CHILD_ROOT: &str = "NIB_SESSION_COMMIT_CHILD_ROOT";
#[cfg(unix)]
const SESSION_COMMIT_CHILD_ID: &str = "NIB_SESSION_COMMIT_CHILD_ID";
#[cfg(unix)]
const SESSION_COMMIT_CHILD_MODE: &str = "NIB_SESSION_COMMIT_CHILD_MODE";
#[cfg(unix)]
const SESSION_COMMIT_CHILD_READY: &str = "NIB_SESSION_COMMIT_CHILD_READY";
#[cfg(unix)]
const SESSION_COMMIT_CHILD_RELEASE: &str = "NIB_SESSION_COMMIT_CHILD_RELEASE";

fn plan_step(description: &str) -> PlanStep {
    PlanStep {
        description: description.to_string(),
        status: "Pending".to_string(),
        outcome: None,
        attempts: 0,
        updated_at: None,
        verification_obligations: Vec::new(),
        content_generation: 0,
    }
}

fn required_terminal_check(
    id: &str,
    description: &str,
    paths: Vec<String>,
) -> VerificationObligation {
    VerificationObligation::pending_tool(
        id,
        description,
        paths.clone(),
        "run_terminal",
        serde_json::json!({"command": "true", "affected_paths": paths}),
        VerificationExpectedOutcome::Success,
    )
    .expect("valid verification contract")
}

#[test]
fn adjacent_user_turns_are_valid_after_a_run_without_an_assistant_message() {
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path());
    let session = store.create_session_with_id("adjacent-user-turns");

    store
        .try_append_message(&session.id, "user", "first request")
        .expect("first user turn");
    store
        .try_append_message(&session.id, "user", "retry request")
        .expect("a reconciled run may accept the next user turn directly");

    let persisted = store.load(&session.id).expect("persisted session");
    assert_eq!(
        persisted
            .messages
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        ["user", "user"]
    );
    persisted.validate().expect("valid persisted sequence");
}

#[test]
fn plan_identity_is_unique_and_goal_matching_is_exact_after_normalization() {
    let first = Plan::new(
        "  Implement\tplan identity\nand resume  ",
        vec![plan_step("implement")],
    );
    let second = Plan::new(
        "Implement plan identity and resume",
        vec![plan_step("implement")],
    );

    assert_ne!(first.id, second.id);
    assert_eq!(first.goal, "Implement plan identity and resume");
    assert!(first.matches_goal("Implement   plan identity and resume"));
    assert!(!first.matches_goal("implement plan identity and resume"));
    assert!(first.is_resumable_for("Implement plan identity and resume"));

    let mut malformed = Plan::new(
        "Implement plan identity and resume",
        vec![plan_step("implement")],
    );
    malformed.steps[0].description.clear();
    malformed.approve();
    assert!(!malformed.is_structured());
    assert!(!malformed.is_resumable_for("Implement plan identity and resume"));

    let mut stale_cursor = Plan::new(
        "Implement plan identity and resume",
        vec![plan_step("implement"), plan_step("verify")],
    );
    stale_cursor.approve();
    stale_cursor.steps[0].status = "Completed".to_string();
    assert!(!stale_cursor.is_structured());
    assert!(!stale_cursor.is_resumable_for("Implement plan identity and resume"));

    let mut advanced_future = Plan::new(
        "Implement plan identity and resume",
        vec![plan_step("implement"), plan_step("verify")],
    );
    advanced_future.approve();
    advanced_future.steps[1].status = "InProgress".to_string();
    assert!(!advanced_future.is_structured());
}

#[test]
fn legacy_plan_metadata_defaults_empty_for_runtime_invalidation() {
    let legacy: Plan = serde_json::from_value(serde_json::json!({
        "steps": [{
            "description": "legacy step",
            "status": "Pending"
        }],
        "current_step_index": 0
    }))
    .expect("legacy plan");

    assert!(!legacy.has_identity());
    assert!(!legacy.is_structured());
    assert!(!legacy.is_resumable_for("legacy goal"));
    assert!(legacy.steps[0].verification_obligations.is_empty());
    assert_eq!(legacy.steps[0].content_generation, 0);
}

#[test]
fn failed_required_verification_survives_unrelated_success_and_requires_exact_rerun() {
    let mut step = plan_step("repair and verify");
    step.verification_obligations.push(required_terminal_check(
        "required-check",
        "run the required check",
        vec!["src".to_string()],
    ));
    let mut plan = Plan::new("repair", vec![step]);
    plan.approve();
    let failed_invocation = crate::tools::ToolInvocationId::new();
    plan.begin_verification(
        "required-check",
        failed_invocation,
        "run_terminal",
        &serde_json::json!({"command": "true", "affected_paths": ["src"]}),
        Some("worktree-a".to_string()),
    )
    .expect("bind required check");
    plan.finish_verification(
        "required-check",
        failed_invocation,
        Some("worktree-a"),
        None,
        false,
        Some("check failed".to_string()),
    )
    .expect("record failed check");

    plan.record_tool_outcome(true, "unrelated read succeeded");
    assert_eq!(plan.steps[0].status, "Blocked");
    assert_eq!(
        plan.steps[0].verification_obligations[0].status,
        VerificationStatus::Failed
    );
    plan.complete_current_step("model claimed completion");
    assert!(!plan.is_complete());
    assert_eq!(plan.outcome.as_deref(), Some("verification_unresolved"));

    let corrective_invocation = crate::tools::ToolInvocationId::new();
    plan.begin_verification(
        "required-check",
        corrective_invocation,
        "run_terminal",
        &serde_json::json!({"command": "true", "affected_paths": ["src"]}),
        Some("worktree-a".to_string()),
    )
    .expect("bind corrective rerun");
    assert!(plan
        .finish_verification(
            "required-check",
            corrective_invocation,
            Some("worktree-b"),
            Some("sha256:other".to_string()),
            true,
            None,
        )
        .is_err());
    plan.finish_verification(
        "required-check",
        corrective_invocation,
        Some("worktree-a"),
        Some("sha256:content".to_string()),
        true,
        None,
    )
    .expect("record corrective pass");
    plan.record_tool_outcome(true, "required check passed");
    plan.complete_current_step("verified");
    assert!(plan.is_complete());
}

#[test]
fn mutation_stales_passed_verification_and_unknown_status_fails_closed() {
    let mut step = plan_step("verify then edit");
    step.verification_obligations.push(required_terminal_check(
        "required-check",
        "run the required check",
        vec!["src".to_string()],
    ));
    let mut plan = Plan::new("repair", vec![step]);
    plan.approve();
    let check_invocation = crate::tools::ToolInvocationId::new();
    plan.begin_verification(
        "required-check",
        check_invocation,
        "run_terminal",
        &serde_json::json!({"command": "true", "affected_paths": ["src"]}),
        Some("worktree-a".to_string()),
    )
    .expect("bind check");
    plan.finish_verification(
        "required-check",
        check_invocation,
        Some("worktree-a"),
        Some("sha256:content".to_string()),
        true,
        None,
    )
    .expect("record pass");
    assert_eq!(
        plan.invalidate_verification_after_mutation(crate::tools::ToolInvocationId::new()),
        ["required-check"]
    );
    assert_eq!(
        plan.steps[0].verification_obligations[0].status,
        VerificationStatus::Stale
    );
    assert_eq!(plan.steps[0].content_generation, 1);
    assert_eq!(plan.steps[0].status, "Blocked");

    let mut encoded = serde_json::to_value(&plan).expect("serialize plan");
    encoded["steps"][0]["verification_obligations"][0]["status"] =
        serde_json::json!("future_unknown_status");
    assert!(serde_json::from_value::<Plan>(encoded).is_err());
}

#[test]
fn cancellation_and_waiver_remain_distinct_and_project_gate_fails_closed() {
    let mut step = plan_step("cancel or narrow scope");
    let mut human = required_terminal_check(
        "human-check",
        "user-requested check",
        vec!["src".to_string()],
    );
    human.authority = VerificationAuthority::Human;
    let mut project = required_terminal_check(
        "project-check",
        "mandatory project gate",
        vec![".".to_string()],
    );
    project.authority = VerificationAuthority::Project;
    step.verification_obligations = vec![human, project];
    let mut plan = Plan::new("verify", vec![step]);
    plan.approve();
    plan.begin_verification(
        "human-check",
        crate::tools::ToolInvocationId::new(),
        "run_terminal",
        &serde_json::json!({"command": "true", "affected_paths": ["src"]}),
        None,
    )
    .expect("start human check");

    assert_eq!(
        plan.cancel_running_verifications("cancelled by user"),
        ["human-check"]
    );
    assert_eq!(
        plan.steps[0].verification_obligations[0].status,
        VerificationStatus::Cancelled
    );
    let plan_id = plan.id.clone();
    assert!(plan
        .waive_verification("wrong-plan", "human-check", 7, "request scope changed")
        .is_err());
    plan.waive_verification(&plan_id, "human-check", 7, "request scope changed")
        .expect("human requirement may become inapplicable");
    assert_eq!(
        plan.steps[0].verification_obligations[0].status,
        VerificationStatus::Waived
    );
    assert_eq!(
        plan.steps[0].verification_obligations[0].waiver_source_message_index,
        Some(7)
    );
    assert!(plan
        .waive_verification(&plan_id, "project-check", 7, "skip mandatory gate")
        .is_err());
    assert_eq!(
        plan.steps[0].verification_obligations[1].status,
        VerificationStatus::Pending
    );
}

#[cfg(windows)]
fn create_directory_junction(junction: &Path, target: &Path) {
    let output = std::process::Command::new("cmd")
        .args(["/D", "/C", "mklink", "/J"])
        .arg(junction)
        .arg(target)
        .output()
        .expect("create directory junction");
    assert!(
        output.status.success(),
        "mklink failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn session_roundtrip() {
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path());
    let session = store.create_session();
    store.append_message(&session.id, "user", "hello");

    let loaded = store.load(&session.id).expect("load session");
    assert_eq!(loaded.id, session.id);
    assert_eq!(loaded.messages.len(), 1);
    assert_eq!(loaded.messages[0].role, "user");
    assert_eq!(loaded.messages[0].content, "hello");
    assert!(loaded.messages[0].timestamp.is_some());

    let raw = fs::read_to_string(store.path(&session.id)).expect("read file");
    let reparsed: Session = serde_json::from_str(&raw).expect("parse");
    assert_eq!(reparsed, loaded);
}

#[test]
fn session_audit_floats_roundtrip_without_losing_a_ulp() {
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path());
    let session = store.create_session();
    let duration = 1.551_978_736_000_000_1_f64;
    let invocation_id = crate::tools::ToolInvocationId::new();

    store
        .record_tool_call(ToolCallRecord {
            invocation_id: Some(invocation_id),
            session_id: Some(session.id.clone()),
            tool_name: Some("roundtrip_probe".to_string()),
            arguments: serde_json::json!({"nested_duration": duration}),
            duration_seconds: Some(duration),
            ..ToolCallRecord::default()
        })
        .expect("persist exact audit float");

    let loaded = store.load(&session.id).expect("load session");
    let call = loaded.tool_calls.last().expect("audit call");
    assert_eq!(call.invocation_id, Some(invocation_id));
    assert_eq!(
        call.duration_seconds.expect("duration").to_bits(),
        duration.to_bits()
    );
    assert_eq!(
        call.arguments["nested_duration"]
            .as_f64()
            .expect("nested duration")
            .to_bits(),
        duration.to_bits()
    );
}

#[test]
fn loads_legacy_session_without_timestamps() {
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path());
    let legacy = r#"{
  "id": "legacy-session",
  "messages": [
    {"role": "user", "content": "hi"}
  ],
  "tool_calls": []
}"#;
    fs::write(store.path("legacy-session"), legacy).expect("write legacy");

    let loaded = store.load("legacy-session").expect("load legacy");
    assert_eq!(loaded.messages.len(), 1);
    assert!(loaded.messages[0].timestamp.is_none());
}

#[test]
fn loads_legacy_tool_call_record_without_invocation_id() {
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path());
    let legacy = r#"{
  "id": "legacy-tool-record",
  "messages": [],
  "tool_calls": [
    {
      "id": "tool-legacy",
      "tool_name": "read_file",
      "arguments": {"path": "README.md"}
    }
  ]
}"#;
    fs::write(store.path("legacy-tool-record"), legacy).expect("write legacy");

    let loaded = store.load("legacy-tool-record").expect("load legacy");
    let call = loaded.tool_calls.first().expect("legacy tool record");
    assert_eq!(call.id.as_deref(), Some("tool-legacy"));
    assert_eq!(call.invocation_id, None);
}

#[test]
fn oversized_sparse_session_is_rejected_before_reading() {
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path());
    let path = store.path("oversized");
    fs::File::create(&path)
        .and_then(|file| file.set_len(MAX_SESSION_JSON_BYTES + 1))
        .expect("create sparse session");

    let error = store
        .load_result("oversized")
        .expect_err("oversized session must fail closed");

    assert!(matches!(error, SessionError::FileTooLarge { .. }));
    assert_eq!(
        fs::metadata(path).unwrap().len(),
        MAX_SESSION_JSON_BYTES + 1
    );
}

#[test]
fn list_result_rejects_valid_named_corrupt_or_mismatched_sessions() {
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path());
    let path = store.path("corrupt-session");
    fs::write(&path, b"{not valid json").expect("write corrupt session");

    let corrupt_error = store
        .list_result()
        .expect_err("corrupt session must fail strict enumeration");
    assert!(
        corrupt_error.to_string().contains("parse session JSON"),
        "{corrupt_error}"
    );

    fs::write(&path, r#"{"id":"different-session"}"#).expect("write mismatched session");
    let mismatch_error = store
        .list_result()
        .expect_err("mismatched session must fail strict enumeration");
    assert!(
        mismatch_error
            .to_string()
            .contains("contains session different-session"),
        "{mismatch_error}"
    );
}

#[test]
fn record_skill_usage_rejects_noncanonicalizable_name_without_persisting() {
    let dir = tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path());
    let session = store.create_session();

    let error = store
        .record_skill_usage(&session.id, "!!!", Some("invalid".to_string()))
        .expect_err("invalid skill name must be rejected");

    assert!(matches!(error, SessionError::InvalidMutation(_)));
    let persisted = store.load(&session.id).expect("load session");
    assert!(persisted.active_skills.is_empty());
    assert!(persisted.skill_usage.is_empty());
}

#[cfg(any(unix, windows))]
#[test]
fn session_file_replacement_during_read_is_rejected() {
    let root = tempdir().expect("project");
    let store = SessionStore::new(root.path());
    let session = store.create_session_with_id("replace-session-during-read");
    let path = store.path(&session.id);
    let displaced = store.sessions_dir().join("displaced-read-session.json");
    let mut replacement = Session::new(session.id.clone());
    replacement.summary = Some("replacement".to_string());
    let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("serialize");

    let error = store
        .with_session_lock(&session.id, |directory| {
            store.load_opened_unlocked_with_hook(directory, &session.id, || {
                fs::rename(&path, &displaced)?;
                fs::write(&path, &replacement_bytes)?;
                Ok(())
            })
        })
        .expect_err("replacement during read must fail");

    assert!(error.to_string().contains("identity changed"), "{error}");
    assert_eq!(
        fs::read(&path).expect("replacement bytes"),
        replacement_bytes
    );
}

#[cfg(any(unix, windows))]
#[test]
fn session_file_replacement_during_update_is_not_overwritten() {
    let root = tempdir().expect("project");
    let store = SessionStore::new(root.path());
    let session = store.create_session_with_id("replace-session-file");
    let path = store.path(&session.id);
    let displaced = store.sessions_dir().join("displaced-session.json");
    let mut replacement = Session::new(session.id.clone());
    replacement.summary = Some("replacement".to_string());
    let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("serialize");

    let error = store
        .update_session(&session.id, |current| {
            fs::rename(&path, &displaced)?;
            fs::write(&path, &replacement_bytes)?;
            current.summary = Some("must-not-publish".to_string());
            Ok(())
        })
        .expect_err("replaced session identity must fail");

    assert!(error.to_string().contains("identity changed"), "{error}");
    assert_eq!(
        fs::read(&path).expect("replacement bytes"),
        replacement_bytes
    );
}

#[cfg(unix)]
#[test]
fn real_child_session_commit_barrier_and_fsync_crash_recovery() {
    if let Some(root) = std::env::var_os(SESSION_COMMIT_CHILD_ROOT) {
        run_session_commit_child(Path::new(&root));
        return;
    }

    let root = tempdir().expect("project");
    let store = SessionStore::new(root.path());
    let replacement_session = store.create_session_with_id("child-session-commit-substitution");
    let replacement_path = store.path(&replacement_session.id);
    let displaced_path = store.sessions_dir().join("child-session-displaced");
    let ready = root.path().join("session-replacement.ready");
    let release = root.path().join("session-replacement.release");
    let mut child = spawn_session_commit_child(
        root.path(),
        &replacement_session.id,
        "replace",
        &ready,
        Some(&release),
    );
    wait_for_session_commit_child(&mut child, &ready);

    let mut replacement = replacement_session.clone();
    replacement.revision = 1;
    replacement.summary = Some("authoritative replacement".to_string());
    let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("serialize");
    fs::rename(&replacement_path, &displaced_path).expect("displace expected session");
    fs::write(&replacement_path, &replacement_bytes).expect("install replacement session");
    fs::write(&release, b"release").expect("release replacement child");
    let status = child.wait().expect("wait for replacement child");
    assert!(status.success(), "replacement child failed: {status}");
    assert_eq!(
        fs::read(&replacement_path).expect("replacement session bytes"),
        replacement_bytes
    );
    assert_eq!(
        fs::read(&displaced_path).expect("displaced session bytes"),
        serde_json::to_vec_pretty(&replacement_session).expect("serialize original")
    );

    let crash_session = store.create_session_with_id("child-session-fsync-crash");
    let crash_path = store.path(&crash_session.id);
    let crash_before = fs::read(&crash_path).expect("session before crash");
    let crash_ready = root.path().join("session-crash.ready");
    let mut crash_child =
        spawn_session_commit_child(root.path(), &crash_session.id, "kill", &crash_ready, None);
    wait_for_session_commit_child(&mut crash_child, &crash_ready);
    let temporary = session_temporary_paths(store.sessions_dir());
    assert_eq!(temporary.len(), 1, "expected one fsynced session temp");
    crash_child.kill().expect("kill session writer");
    crash_child.wait().expect("reap session writer");
    assert!(
        temporary[0].exists(),
        "killed writer temp disappeared early"
    );

    drop(store);
    let recovered = SessionStore::new(root.path());
    assert_eq!(
        fs::read(&crash_path).expect("session after recovery"),
        crash_before
    );
    assert!(
        session_temporary_paths(recovered.sessions_dir()).is_empty(),
        "fresh session store left the killed writer temp"
    );
    assert_eq!(
        recovered
            .load_result(&crash_session.id)
            .expect("load recovered session")
            .expect("recovered session"),
        crash_session
    );
}

#[test]
fn stale_session_snapshot_cannot_overwrite_a_newer_revision() {
    let root = tempdir().expect("project");
    let store = SessionStore::new(root.path());
    let session = store.create_session_with_id("session-revision-cas");
    let mut first = store.load(&session.id).expect("first snapshot");
    let mut stale = first.clone();
    first.summary = Some("first writer".to_string());
    stale.summary = Some("stale writer".to_string());

    store.save(&mut first).expect("first revision commit");
    let error = store
        .save(&mut stale)
        .expect_err("stale snapshot revision must be rejected");

    assert!(
        error.to_string().contains("stale session revision"),
        "{error}"
    );
    assert_eq!(
        store
            .load(&session.id)
            .expect("authoritative session")
            .summary
            .as_deref(),
        Some("first writer")
    );
}

#[test]
fn consecutive_session_saves_refresh_the_snapshot_revision() {
    let root = tempdir().expect("project");
    let store = SessionStore::new(root.path());
    let mut session = store.create_session_with_id("session-consecutive-save");
    assert_eq!(session.revision, 0);

    session.summary = Some("first".to_string());
    store.save(&mut session).expect("first save");
    assert_eq!(session.revision, 1);
    session.summary = Some("second".to_string());
    store.save(&mut session).expect("second save");
    assert_eq!(session.revision, 2);

    let persisted = store.load(&session.id).expect("persisted session");
    assert_eq!(persisted.revision, 2);
    assert_eq!(persisted.summary.as_deref(), Some("second"));
}

#[test]
fn legacy_session_without_revision_defaults_to_zero() {
    let root = tempdir().expect("project");
    let store = SessionStore::new(root.path());
    let session = store.create_session_with_id("legacy-session-revision");
    let path = store.path(&session.id);
    let bytes = fs::read(&path).expect("legacy-compatible session bytes");
    assert!(!String::from_utf8_lossy(&bytes).contains("\"revision\""));
    assert_eq!(store.load(&session.id).expect("legacy session").revision, 0);
}

#[test]
fn session_revision_overflow_preserves_disk_and_snapshot() {
    let root = tempdir().expect("project");
    let store = SessionStore::new(root.path());
    let mut session = store.create_session_with_id("session-revision-overflow");
    session.revision = u64::MAX;
    let path = store.path(&session.id);
    fs::write(
        &path,
        serde_json::to_vec_pretty(&session).expect("overflow session JSON"),
    )
    .expect("write overflow session");
    let before = fs::read(&path).expect("session before failed save");

    let error = store
        .save(&mut session)
        .expect_err("revision overflow must fail closed");

    assert!(error.to_string().contains("revision overflowed"), "{error}");
    assert_eq!(session.revision, u64::MAX);
    assert_eq!(fs::read(path).expect("session after failed save"), before);
}

#[cfg(any(unix, windows))]
#[test]
fn session_delete_rejects_replacement_at_quarantine_boundary() {
    let root = tempdir().expect("project");
    let store = SessionStore::new(root.path());
    let session = store.create_session_with_id("replace-session-delete");
    let path = store.path(&session.id);
    let displaced = store.sessions_dir().join("displaced-delete-session.json");
    let mut replacement = Session::new(session.id.clone());
    replacement.summary = Some("newer replacement".to_string());
    let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("serialize");

    let error = store
        .delete_if_with_commit_check(
            &session.id,
            |_, _| true,
            || {
                fs::rename(&path, &displaced)?;
                fs::write(&path, &replacement_bytes)?;
                Ok(())
            },
        )
        .expect_err("replaced session must not be deleted");

    assert!(error.to_string().contains("identity changed"), "{error}");
    assert_eq!(
        fs::read(path).expect("replacement session"),
        replacement_bytes
    );
    assert!(displaced.exists());
}

#[cfg(any(unix, windows))]
#[test]
fn session_lock_replacement_while_held_is_rejected() {
    let root = tempdir().expect("project");
    let store = SessionStore::new(root.path());
    let session = store.create_session_with_id("replace-session-lock");
    let lock_path = store.lock_path(&session.id).expect("lock path");
    let displaced = store.sessions_dir().join("displaced-session.lock");

    let error = store
        .update_session(&session.id, |current| {
            fs::rename(&lock_path, &displaced)?;
            fs::write(&lock_path, b"replacement-lock")?;
            current.summary = Some("operation-must-report-failure".to_string());
            Ok(())
        })
        .expect_err("replaced lock identity must fail");

    assert!(
        error.to_string().contains("different identities"),
        "{error}"
    );
    assert_eq!(
        fs::read(&lock_path).expect("replacement lock"),
        b"replacement-lock"
    );
    assert!(SessionStore::new(root.path())
        .load_result(&session.id)
        .is_err());
}

#[test]
fn expired_deadline_rejects_free_session_locks_without_mutating() {
    let root = tempdir().expect("project");
    let store = SessionStore::new(root.path());
    let session = store.create_session_with_id("expired-free-session-lock");
    let mut update_ran = false;

    let error = store
        .update_session_with_deadline(
            &session.id,
            Instant::now() - Duration::from_millis(1),
            |current| {
                update_ran = true;
                current.summary = Some("must not persist".to_string());
                Ok(())
            },
        )
        .expect_err("an expired deadline must reject uncontended session locks");

    assert!(
        error
            .to_string()
            .contains("timed out acquiring session lock"),
        "{error}"
    );
    assert!(!update_ran, "expired session update entered its mutation");
    assert_eq!(
        store.load(&session.id).expect("session remains").summary,
        None
    );
}

#[test]
fn session_read_crossing_its_deadline_is_rejected_without_late_anchor_cleanup() {
    let root = tempdir().expect("project");
    let store = SessionStore::new(root.path());
    let session = store.create_session_with_id("expired-post-read-session");
    let deadline = Instant::now() + Duration::from_millis(40);
    let mut read_namespace = None;

    let error = store
        .with_session_lock_until(&session.id, deadline, |directory| {
            store.load_opened_unlocked_with_hook(directory, &session.id, || {
                read_namespace = Some(session_namespace_snapshot(store.sessions_dir()));
                while Instant::now() < deadline {
                    std::thread::yield_now();
                }
                Ok(())
            })?;
            Ok(())
        })
        .expect_err("a session read crossing its deadline must be rejected");

    assert!(
        error
            .to_string()
            .contains("timed out acquiring daemon state lock"),
        "{error}"
    );
    let read_namespace = read_namespace.expect("captured post-read lock namespace");
    assert_eq!(
        session_namespace_snapshot(store.sessions_dir()),
        read_namespace,
        "post-expiry lock cleanup mutated its persistent anchor"
    );
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        session_namespace_snapshot(store.sessions_dir()),
        read_namespace,
        "post-expiry lock cleanup mutated its persistent anchor later"
    );
    assert_eq!(
        store
            .load_result(&session.id)
            .expect("later unbounded read")
            .expect("session")
            .id,
        session.id
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
fn session_publication_rechecks_deadline_at_the_commit_boundary() {
    let root = tempdir().expect("project");
    let store = SessionStore::new(root.path());
    let session = store.create_session_with_id("expired-session-commit");
    let path = store.path(&session.id);
    let original = fs::read(&path).expect("original session bytes");
    let before_namespace = session_namespace_snapshot(store.sessions_dir());
    let mut paused_namespace = None;
    let mut expected_temporary = None;

    let error = store
        .with_skill_usage_lock(|| {
            store.with_session_lock(&session.id, |directory| {
                let mut opened = store
                    .load_opened_unlocked(directory, &session.id)?
                    .ok_or_else(|| SessionError::NotFound(session.id.clone()))?;
                opened.session.summary = Some("must not publish".to_string());
                opened.session.revision += 1;
                expected_temporary = Some(
                    serde_json::to_vec_pretty(&opened.session)
                        .expect("expected temporary session bytes"),
                );
                let commit_timeout = if cfg!(windows) {
                    Duration::from_secs(2)
                } else {
                    Duration::from_millis(40)
                };
                let deadline = Instant::now() + commit_timeout;
                store.save_unlocked_with_deadline_and_commit_check(
                    directory,
                    &opened.session,
                    Some(&opened.file),
                    Some(deadline),
                    || {
                        paused_namespace = Some(session_namespace_shape(store.sessions_dir()));
                        while Instant::now() < deadline {
                            std::thread::yield_now();
                        }
                        Ok(())
                    },
                )
            })
        })
        .expect_err("expired session precommit must fail");

    assert!(
        error
            .to_string()
            .contains("timed out acquiring session lock"),
        "{error}"
    );
    assert_eq!(fs::read(&path).expect("unchanged session bytes"), original);
    let paused_namespace = paused_namespace.expect("captured session precommit namespace");
    let post_failure_namespace = session_namespace_snapshot(store.sessions_dir());
    assert_eq!(
        session_namespace_shape(store.sessions_dir()),
        paused_namespace,
        "expired session cleanup mutated transaction artifacts"
    );
    let expected_temporary = expected_temporary.expect("serialized temporary session");
    let mut saw_temporary = false;
    for (name, bytes) in &post_failure_namespace {
        let rendered = name.to_string_lossy();
        if rendered.starts_with(".nib-session-") && rendered.ends_with(".tmp") {
            assert!(!saw_temporary, "multiple retained session temporaries");
            assert_eq!(bytes, &expected_temporary, "retained temporary bytes");
            saw_temporary = true;
        } else if let Some((_, expected)) = before_namespace
            .iter()
            .find(|(before_name, _)| before_name == name)
        {
            assert_eq!(bytes, expected, "pre-existing session namespace bytes");
        } else {
            assert!(
                bytes.is_empty(),
                "unexpected nonempty session transaction artifact: {rendered}"
            );
        }
    }
    assert!(
        saw_temporary,
        "expired publication did not retain its temporary"
    );
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        session_namespace_snapshot(store.sessions_dir()),
        post_failure_namespace,
        "expired session cleanup mutated transaction artifacts later"
    );

    let mut update_ran = false;
    let missing = "preexpired-session-create";
    store
        .update_or_create_session_with_deadline(
            missing,
            Instant::now() - Duration::from_millis(1),
            |_session| {
                update_ran = true;
                Ok(())
            },
        )
        .expect_err("preexpired update-or-create must fail on free locks");
    assert!(!update_ran, "preexpired session mutation ran");
    assert!(
        !store.path(missing).exists(),
        "preexpired session was created"
    );
}

#[test]
fn append_once_audit_expiry_preserves_the_complete_session_namespace() {
    let root = tempdir().expect("project");
    let store = SessionStore::new(root.path());
    let session = store.create_session_with_id("expired-append-once-audit");
    let path = store.path(&session.id);
    let original = fs::read(&path).expect("original audit session bytes");
    let before_namespace = session_namespace_snapshot(store.sessions_dir());
    let deadline = Instant::now() + Duration::from_millis(40);

    PAUSE_RECORD_EVENT_ONCE_COMMIT.set(true);
    let result = store.record_event_once_with_deadline(
        &session.id,
        "subagent_execution_reconciled",
        "reconciliation-v1",
        serde_json::json!({
            "reconciliation_id": "reconciliation-v1",
            "outcome": "cancelled_after_verified_cleanup",
        }),
        serde_json::json!({
            "outcome": "cancelled_after_verified_cleanup",
        }),
        deadline,
    );
    PAUSE_RECORD_EVENT_ONCE_COMMIT.set(false);
    let error = result.expect_err("expired append-once publication must fail closed");

    assert!(
        error
            .to_string()
            .contains("timed out acquiring daemon state lock"),
        "{error}"
    );
    assert_eq!(fs::read(&path).expect("unchanged audit session"), original);
    let expired_namespace = session_namespace_snapshot(store.sessions_dir());
    assert_ne!(
        expired_namespace, before_namespace,
        "the prepublication pause must retain its recoverable transaction"
    );
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        session_namespace_snapshot(store.sessions_dir()),
        expired_namespace,
        "expired append-once cleanup mutated the session namespace later"
    );
    assert!(store
        .load_result(&session.id)
        .expect("load unchanged audit session")
        .expect("audit session")
        .events
        .is_empty());
}

#[test]
fn session_lock_registry_contention_obeys_the_absolute_deadline() {
    let root = tempdir().expect("project");
    let store = SessionStore::new(root.path());
    let registry: SessionLockRegistry = Mutex::new(HashMap::new());
    let held_registry = registry.lock().expect("hold session lock registry");
    let path = store.sessions_dir().join("registry-contention.lock");
    let started = Instant::now();

    let error = store
        .process_lock_in(
            &registry,
            &path,
            Some(Instant::now() + Duration::from_millis(75)),
        )
        .expect_err("the session lock registry wait must be bounded");

    assert!(
        error
            .to_string()
            .contains("timed out acquiring session lock"),
        "{error}"
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(
        held_registry.is_empty(),
        "contended registry was not mutated"
    );
}

#[test]
fn deadline_update_times_out_behind_process_lock_without_mutating() {
    let root = tempdir().expect("project");
    let store = SessionStore::new(root.path());
    let session = store.create_session_with_id("deadline-session-lock");
    let holder_store = store.clone();
    let holder_id = session.id.clone();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        holder_store
            .update_session(&holder_id, |_current| {
                held_tx.send(()).expect("signal held session lock");
                release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("release held session lock");
                Ok(())
            })
            .expect("holder session update");
    });
    held_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("session lock is held");

    let error = store
        .update_session_with_deadline(
            &session.id,
            Instant::now() + Duration::from_millis(100),
            |current| {
                current.summary = Some("must not persist".to_string());
                Ok(())
            },
        )
        .expect_err("deadline must bound the process lock wait");
    assert!(
        error
            .to_string()
            .contains("timed out acquiring session lock"),
        "{error}"
    );

    release_tx.send(()).expect("release session lock");
    holder.join().expect("session lock holder");
    assert_eq!(
        store.load(&session.id).expect("session remains").summary,
        None
    );
}

#[test]
fn configured_lock_timeout_bounds_default_session_updates() {
    let root = tempdir().expect("project");
    let unbounded_store = SessionStore::new(root.path());
    let session = unbounded_store.create_session_with_id("configured-deadline-session");
    let holder_store = unbounded_store.clone();
    let holder_id = session.id.clone();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        holder_store
            .update_session(&holder_id, |_current| {
                held_tx.send(()).expect("signal held session lock");
                release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("release held session lock");
                Ok(())
            })
            .expect("holder session update");
    });
    held_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("session lock is held");

    let bounded_store = unbounded_store
        .clone()
        .with_lock_timeout(Duration::from_millis(100));
    let started = Instant::now();
    let error = bounded_store
        .update_session(&session.id, |current| {
            current.summary = Some("must not persist".to_string());
            Ok(())
        })
        .expect_err("configured deadline must bound the default update API");
    assert!(
        error
            .to_string()
            .contains("timed out acquiring session lock"),
        "{error}"
    );
    assert!(started.elapsed() < Duration::from_secs(1));

    release_tx.send(()).expect("release session lock");
    holder.join().expect("session lock holder");
    assert_eq!(
        unbounded_store
            .load(&session.id)
            .expect("session remains")
            .summary,
        None
    );
}

#[test]
fn configured_lock_timeout_is_shared_across_nested_locks() {
    let root = tempdir().expect("project");
    let store = SessionStore::new(root.path());
    let session = store.create_session_with_id("shared-deadline-session");

    let session_holder_store = store.clone();
    let session_holder_id = session.id.clone();
    let (session_held_tx, session_held_rx) = std::sync::mpsc::channel();
    let (release_session_tx, release_session_rx) = std::sync::mpsc::channel();
    let session_holder = std::thread::spawn(move || {
        session_holder_store
            .with_session_lock(&session_holder_id, |_| {
                session_held_tx.send(()).expect("signal session lock held");
                release_session_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("release session lock");
                Ok(())
            })
            .expect("hold session lock");
    });
    session_held_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("session lock is held");

    let skill_holder_store = store.clone();
    let (skill_held_tx, skill_held_rx) = std::sync::mpsc::channel();
    let (release_skill_tx, release_skill_rx) = std::sync::mpsc::channel();
    let skill_holder = std::thread::spawn(move || {
        skill_holder_store
            .with_skill_usage_lock(|| {
                skill_held_tx.send(()).expect("signal skill lock held");
                release_skill_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("release skill lock");
                Ok(())
            })
            .expect("hold skill usage lock");
    });
    skill_held_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("skill usage lock is held");

    let delayed_release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(1));
        release_skill_tx.send(()).expect("release skill usage lock");
    });
    let bounded_store = store.clone().with_lock_timeout(Duration::from_secs(3));
    let started = Instant::now();
    let error = bounded_store
        .update_session(&session.id, |_current| Ok(()))
        .expect_err("one deadline must cover both nested lock waits");
    let elapsed = started.elapsed();
    assert!(
        error
            .to_string()
            .contains("timed out acquiring daemon state lock"),
        "{error}"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "nested locks received separate timeout budgets: {elapsed:?}"
    );

    delayed_release.join().expect("delayed skill lock release");
    skill_holder.join().expect("skill lock holder");
    release_session_tx.send(()).expect("release session lock");
    session_holder.join().expect("session lock holder");
}

#[test]
fn strict_enumeration_uses_the_session_mutation_lock() {
    let root = tempdir().expect("project");
    let store = SessionStore::new(root.path());
    let session = store.create_session_with_id("serialized-list-session");
    let holder_store = store.clone();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        holder_store
            .with_skill_usage_lock(|| {
                held_tx.send(()).expect("signal mutation lock held");
                release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("release mutation lock");
                Ok(())
            })
            .expect("hold mutation lock");
    });
    held_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("mutation lock is held");

    let bounded_store = store.clone().with_lock_timeout(Duration::from_millis(100));
    let started = Instant::now();
    let error = bounded_store
        .list_result()
        .expect_err("strict enumeration must join the mutation lock domain");
    assert!(
        error
            .to_string()
            .contains("timed out acquiring session lock"),
        "{error}"
    );
    assert!(started.elapsed() < Duration::from_secs(1));

    release_tx.send(()).expect("release mutation lock");
    holder.join().expect("mutation lock holder");
    assert_eq!(
        store.list_result().expect("strict session enumeration"),
        vec![session.id]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn scoped_lock_policy_propagates_to_spawned_store_without_starving_runtime() {
    let root = tempdir().expect("project");
    let holder_store = SessionStore::new(root.path());
    let session = holder_store.create_session_with_id("scoped-deadline-session");
    let holder_id = session.id.clone();
    let operation_store = SessionStore::new(root.path());
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        holder_store
            .update_session(&holder_id, |_current| {
                held_tx.send(()).expect("signal held session lock");
                release_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("release held session lock");
                Ok(())
            })
            .expect("holder session update");
    });
    held_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("session lock is held");

    let operation_id = session.id.clone();
    let operation = tokio::spawn(SessionStore::with_lock_policy(
        Duration::from_millis(200),
        async move {
            let policy = SessionStore::current_lock_policy();
            tokio::spawn(SessionStore::with_optional_lock_policy(
                policy,
                async move { operation_store.update_session(&operation_id, |_current| Ok(())) },
            ))
            .await
            .expect("spawned session operation")
        },
    ));
    tokio::time::timeout(Duration::from_millis(100), async {
        tokio::time::sleep(Duration::from_millis(20)).await;
    })
    .await
    .expect("the sole async worker remains responsive during a session lock wait");

    let error = operation
        .await
        .expect("scoped operation task")
        .expect_err("scoped lock policy must time out");
    assert!(
        error
            .to_string()
            .contains("timed out acquiring session lock"),
        "{error}"
    );

    release_tx.send(()).expect("release session lock");
    holder.join().expect("session lock holder");
}

#[cfg(any(unix, windows))]
#[test]
fn skill_usage_lock_replacement_while_held_is_rejected() {
    let root = tempdir().expect("project");
    let store = SessionStore::new(root.path());
    let lock_path = store.sessions_dir().join(SKILL_USAGE_LOCK_FILE);
    let displaced = store.sessions_dir().join("displaced-skill-usage.lock");

    let error = store
        .with_skill_usage_lock(|| {
            fs::rename(&lock_path, &displaced)?;
            fs::write(&lock_path, b"replacement-lock")?;
            Ok(())
        })
        .expect_err("replaced skill usage lock identity must fail");

    assert!(
        error.to_string().contains("different identities"),
        "{error}"
    );
    assert_eq!(
        fs::read(&lock_path).expect("replacement lock"),
        b"replacement-lock"
    );
}

#[cfg(any(unix, windows))]
#[test]
fn whole_session_directory_replacement_is_rejected() {
    let root = tempdir().expect("project");
    let store = SessionStore::new(root.path());
    let session = store.create_session_with_id("replace-session-directory");
    let sessions_dir = store.sessions_dir().to_path_buf();
    let displaced = root.path().join("displaced-sessions");
    #[cfg(unix)]
    let (replacement_path, replacement_bytes) = {
        let replacement_path = sessions_dir.join(format!("{}.json", session.id));
        let mut replacement = Session::new(session.id.clone());
        replacement.summary = Some("replacement-directory".to_string());
        let replacement_bytes = serde_json::to_vec_pretty(&replacement).expect("serialize");
        (replacement_path, replacement_bytes)
    };

    let error = store
        .update_session(&session.id, |current| {
            #[cfg(unix)]
            {
                fs::rename(&sessions_dir, &displaced)?;
                fs::create_dir(&sessions_dir)?;
                fs::write(&replacement_path, &replacement_bytes)?;
                current.summary = Some("must-not-publish".to_string());
                Ok(())
            }
            #[cfg(windows)]
            {
                let _ = current;
                let error = fs::rename(&sessions_dir, &displaced)
                    .expect_err("live Windows session lock pins the sessions directory");
                Err::<(), SessionError>(SessionError::Io(error))
            }
        })
        .expect_err("detached directory must fail");

    #[cfg(unix)]
    {
        assert!(error.to_string().contains("directory"), "{error}");
        assert_eq!(
            fs::read(&replacement_path).expect("replacement session"),
            replacement_bytes
        );
        assert!(store.list_result().is_err());
        assert!(SessionStore::new(root.path())
            .load_result(&session.id)
            .is_err());
    }
    #[cfg(windows)]
    {
        assert!(!error.to_string().is_empty());
        assert!(sessions_dir.is_dir());
        assert!(!displaced.exists());
        assert_eq!(
            store
                .load_result(&session.id)
                .expect("original session remains readable")
                .expect("original session remains present")
                .summary,
            session.summary
        );
    }
}

#[cfg(any(unix, windows))]
#[test]
fn session_lock_artifacts_are_bounded_by_fixed_stripes() {
    let root = tempdir().expect("project");
    let store = SessionStore::new(root.path());
    let ids = (0..256)
        .map(|index| format!("bounded-lock-{index}"))
        .collect::<Vec<_>>();
    for id in &ids {
        store
            .try_create_session_with_id(id)
            .expect("create striped session");
    }

    let lock_names = fs::read_dir(store.sessions_dir())
        .expect("list session locks")
        .map(|entry| entry.expect("directory entry").file_name())
        .filter(|name| {
            name.to_string_lossy().starts_with(".session-lock-")
                && name.to_string_lossy().ends_with(".lock")
        })
        .collect::<Vec<_>>();
    assert!(!lock_names.is_empty());
    assert!(lock_names.len() <= SESSION_LOCK_STRIPES);
    for id in ids {
        assert!(!store.sessions_dir().join(format!(".{id}.lock")).exists());
    }
}

#[cfg(unix)]
#[test]
fn direct_store_rejects_symlinked_nib_without_outside_write() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("project");
    let outside = tempdir().expect("outside");
    symlink(outside.path(), root.path().join(".nib")).expect("symlink .nib");
    let store = SessionStore::new(root.path());

    assert!(store.try_create_session_with_id("blocked").is_err());
    assert!(!outside.path().join("sessions").exists());
}

#[cfg(windows)]
#[test]
fn direct_store_rejects_junctioned_nib_without_outside_write() {
    let root = tempdir().expect("project");
    let outside = tempdir().expect("outside");
    create_directory_junction(&root.path().join(".nib"), outside.path());
    let store = SessionStore::new(root.path());

    assert!(store.try_create_session_with_id("blocked").is_err());
    assert!(!outside.path().join("sessions").exists());
}

#[cfg(unix)]
#[test]
fn mutation_rechecks_directory_after_constructor_race() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("project");
    let outside = tempdir().expect("outside");
    let store = SessionStore::new(root.path());
    fs::rename(root.path().join(".nib"), root.path().join(".nib-displaced"))
        .expect("displace .nib");
    symlink(outside.path(), root.path().join(".nib")).expect("swap state ancestor");

    assert!(store.try_create_session_with_id("raced").is_err());
    assert!(!outside.path().join("sessions").exists());
}

#[cfg(unix)]
fn run_session_commit_child(root: &Path) {
    let id =
        std::env::var(SESSION_COMMIT_CHILD_ID).expect("session commit child id must be configured");
    let mode = std::env::var(SESSION_COMMIT_CHILD_MODE)
        .expect("session commit child mode must be configured");
    let ready = PathBuf::from(
        std::env::var_os(SESSION_COMMIT_CHILD_READY)
            .expect("session commit child ready path must be configured"),
    );
    let release = std::env::var_os(SESSION_COMMIT_CHILD_RELEASE).map(PathBuf::from);
    let store = SessionStore::new(root);
    let result = store.with_skill_usage_lock(|| {
        store.with_session_lock(&id, |directory| {
            let mut opened = store
                .load_opened_unlocked(directory, &id)?
                .ok_or_else(|| SessionError::NotFound(id.clone()))?;
            opened.session.summary = Some("child must not publish".to_string());
            opened.session.revision = opened.session.revision.checked_add(1).ok_or_else(|| {
                SessionError::InvalidMutation("session revision overflowed".to_string())
            })?;
            store.save_unlocked_with_commit_check(
                directory,
                &opened.session,
                Some(&opened.file),
                || {
                    fs::write(&ready, b"ready")?;
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                    loop {
                        if release.as_ref().is_some_and(|path| path.exists()) {
                            return Ok(());
                        }
                        if std::time::Instant::now() >= deadline {
                            return Err(SessionError::InvalidMutation(
                                "session commit child timed out".to_string(),
                            ));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                },
            )
        })
    });

    match mode.as_str() {
        "replace" => {
            let error = result.expect_err("commit-barrier replacement must fail closed");
            assert!(error.to_string().contains("identity changed"), "{error}");
        }
        "kill" => panic!("session crash child unexpectedly left its commit barrier"),
        value => panic!("unsupported session commit child mode: {value}"),
    }
}

#[cfg(unix)]
fn spawn_session_commit_child(
    root: &Path,
    id: &str,
    mode: &str,
    ready: &Path,
    release: Option<&Path>,
) -> std::process::Child {
    let _ = fs::remove_file(ready);
    if let Some(release) = release {
        let _ = fs::remove_file(release);
    }
    let mut command =
        std::process::Command::new(std::env::current_exe().expect("current session test binary"));
    command
        .args([
            "--exact",
            "session::tests::real_child_session_commit_barrier_and_fsync_crash_recovery",
            "--nocapture",
        ])
        .env(SESSION_COMMIT_CHILD_ROOT, root)
        .env(SESSION_COMMIT_CHILD_ID, id)
        .env(SESSION_COMMIT_CHILD_MODE, mode)
        .env(SESSION_COMMIT_CHILD_READY, ready);
    if let Some(release) = release {
        command.env(SESSION_COMMIT_CHILD_RELEASE, release);
    }
    command.spawn().expect("spawn session commit child")
}

#[cfg(unix)]
fn wait_for_session_commit_child(child: &mut std::process::Child, ready: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !ready.exists() {
        if let Some(status) = child.try_wait().expect("inspect session commit child") {
            panic!("session commit child exited before readiness: {status}");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session commit child did not become ready"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn session_temporary_paths(directory: &Path) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .expect("list session directory")
        .map(|entry| entry.expect("session directory entry").path())
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with(".nib-session-") && name.ends_with(".tmp")
            })
        })
        .collect()
}
