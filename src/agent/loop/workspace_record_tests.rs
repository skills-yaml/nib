//! T080 phase 5b: in place, verification covers the session's own changed
//! files, and each run records its workspace.

use super::*;
use crate::session::{ChangedPaths, IdentityScope, ToolCallRecord, VerificationAuthority};

const GOAL: &str = "verify the shared folder";

fn changed(root: &str, paths: &[&str], complete: bool) -> ToolCallRecord {
    ToolCallRecord {
        tool_name: Some("run_terminal".to_string()),
        timestamp: Some(Utc::now()),
        changed_paths: Some(ChangedPaths {
            root: root.to_string(),
            paths: paths.iter().map(|path| (*path).to_string()).collect(),
            complete,
        }),
        ..Default::default()
    }
}

struct Verified {
    _directory: tempfile::TempDir,
    root: PathBuf,
    store: SessionStore,
    session_id: String,
    plan_id: String,
    obligation_id: String,
}

impl Verified {
    fn obligation(&self) -> VerificationObligation {
        let session = self.store.load_result(&self.session_id).unwrap().unwrap();
        let plan = session.plan.unwrap();
        plan.steps[plan.current_step_index].verification_obligations[0].clone()
    }

    fn revalidate(&self) -> Vec<String> {
        revalidate_plan_verification_content(&self.store, &self.session_id, Some(&self.plan_id))
            .unwrap()
    }
}

/// A required whole-folder command (`task verify`) passes in a shared
/// folder after the session recorded `records`.
fn verified_in_place(records: impl FnOnce(&str) -> Vec<ToolCallRecord>) -> Verified {
    let directory = tempfile::tempdir().unwrap();
    let root = shared_folder(directory.path());
    std::fs::write(root.join("a.txt"), "mine\n").unwrap();
    std::fs::write(root.join("b.txt"), "theirs\n").unwrap();
    let root_text = root.to_string_lossy().to_string();
    let store = SessionStore::new(&directory.path().join("state"));
    let mut session = store.create_session_with_id("in-place");
    let obligation =
        command_verification_obligation("task verify", VerificationAuthority::ApprovedPlan, None)
            .unwrap();
    let obligation_id = obligation.id.clone();
    let mut plan = crate::session::Plan::new(
        GOAL,
        vec![crate::session::PlanStep {
            description: "verify".to_string(),
            status: "Pending".to_string(),
            outcome: None,
            attempts: 0,
            updated_at: None,
            verification_obligations: vec![obligation],
            content_generation: 0,
        }],
    );
    plan.approve();
    let plan_id = plan.id.clone();
    let invocation_id = crate::tools::ToolInvocationId::new();
    plan.begin_verification(
        &obligation_id,
        invocation_id,
        "run_terminal",
        &json!({"command": "task verify", "affected_paths": ["."]}),
        Some(root_text.clone()),
    )
    .unwrap();
    session.plan = Some(plan);
    session.tool_calls = records(&root_text);
    let mut verification = changed(&root_text, &[], true);
    verification.invocation_id = Some(invocation_id);
    session.tool_calls.push(verification);
    store.save(&mut session).unwrap();

    let result = crate::tools::ToolResult {
        invocation_id,
        tool_name: "run_terminal".to_string(),
        success: true,
        output: None,
        error: None,
        duration_seconds: 0.0,
        approval_granted: true,
        approval_source: None,
    };
    finish_plan_verification(
        &store,
        &session.id,
        Some(&plan_id),
        &crate::session::normalize_plan_goal(GOAL),
        &obligation_id,
        Some(&root_text),
        &result,
    )
    .unwrap();
    Verified {
        _directory: directory,
        root,
        store,
        session_id: session.id,
        plan_id,
        obligation_id,
    }
}

fn shared_folder(directory: &Path) -> PathBuf {
    let root = directory.join("shared");
    std::fs::create_dir_all(&root).unwrap();
    root.canonicalize().unwrap()
}

#[test]
fn another_sessions_edit_elsewhere_keeps_the_check_and_an_edit_to_its_files_stales_it() {
    let verified = verified_in_place(|root| vec![changed(root, &["a.txt"], true)]);
    assert_eq!(
        verified.obligation().identity_scope,
        Some(IdentityScope {
            root: verified.root.to_string_lossy().to_string(),
            paths: vec!["a.txt".to_string()],
        })
    );

    // Another session (or the user) edits a file this session never touched.
    std::fs::write(verified.root.join("b.txt"), "theirs, edited\n").unwrap();
    std::fs::write(verified.root.join("c.txt"), "new\n").unwrap();
    assert!(verified.revalidate().is_empty());

    // Anyone changing a file this session changed makes the check stale.
    std::fs::write(verified.root.join("a.txt"), "overwritten\n").unwrap();
    assert_eq!(verified.revalidate(), vec![verified.obligation_id.clone()]);
}

#[test]
fn a_session_that_changed_nothing_is_not_staled_by_other_edits() {
    let verified = verified_in_place(|_| Vec::new());
    assert_eq!(
        verified
            .obligation()
            .identity_scope
            .map(|scope| scope.paths),
        Some(Vec::new())
    );
    std::fs::write(verified.root.join("a.txt"), "edited elsewhere\n").unwrap();
    assert!(verified.revalidate().is_empty());
}

#[test]
fn unknown_changes_keep_the_whole_folder() {
    let verified = verified_in_place(|root| {
        vec![
            changed(root, &["a.txt"], true),
            // A background command or an MCP tool: nib cannot see its edits.
            changed(root, &[], false),
        ]
    });
    assert_eq!(verified.obligation().identity_scope, None);
    std::fs::write(verified.root.join("b.txt"), "theirs, edited\n").unwrap();
    assert_eq!(verified.revalidate(), vec![verified.obligation_id.clone()]);
}

#[test]
fn changes_under_another_top_keep_the_whole_folder() {
    let verified = verified_in_place(|_| vec![changed("/elsewhere", &["a.txt"], true)]);
    assert_eq!(verified.obligation().identity_scope, None);
}

#[test]
fn session_changes_are_capped() {
    let mut session = crate::session::Session::new("capped".to_string());
    let paths: Vec<String> = (0..=crate::session::MAX_IN_PLACE_CHANGES)
        .map(|index| format!("f{index}"))
        .collect();
    let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
    session.tool_calls.push(changed("/top", &paths, true));
    assert_eq!(session.in_place_changes("/top", None), None);
}

#[test]
fn runs_record_their_workspace_other_sessions_and_changed_paths() {
    let directory = tempfile::tempdir().unwrap();
    let root = shared_folder(directory.path());
    // A `.git` file: a linked worktree, which nib edits in place.
    std::fs::write(root.join(".git"), "gitdir: elsewhere\n").unwrap();
    std::fs::write(root.join("a.txt"), "one\n").unwrap();
    let store = SessionStore::new(&directory.path().join("state"));
    let session = store.create_session_with_id("recorded");
    let mut other = crate::session::presence::Presence::new(store.sessions_dir(), &root);
    other.tick("other-session", std::time::Instant::now());

    let started = run_started_workspace(&root, store.sessions_dir(), &session.id);
    assert_eq!(started["workspace"], "in_place");
    assert_eq!(started["workspace_root"], json!(root.to_string_lossy()));
    assert_eq!(started["checkpoint_before"], Value::Null);
    assert_eq!(started["other_sessions"], json!(["other-session"]));

    let root_text = root.to_string_lossy().to_string();
    store
        .update_session(&session.id, |session| {
            // Changes before this run are not this run's.
            let mut earlier = changed(&root_text, &["old.txt"], true);
            earlier.timestamp = Some(Utc::now() - chrono::Duration::seconds(5));
            session.tool_calls.push(earlier);
            let mut details = started.clone();
            details["run_id"] = json!("run-1");
            append_session_event(session, "run_started", details);
            session
                .tool_calls
                .push(changed(&root_text, &["a.txt"], true));
            Ok(())
        })
        .unwrap();
    runtime_terminal_event(&store, &session.id, "run-1", "completed").unwrap();
    let session = store.load_result(&session.id).unwrap().unwrap();
    let terminal = session
        .events
        .iter()
        .find(|event| event.kind == "run_terminal")
        .unwrap();
    assert_eq!(terminal.details["changed_paths"], json!(["a.txt"]));
    assert!(terminal.details["content_identity"]
        .as_str()
        .is_some_and(|identity| identity.starts_with("sha256:")));
    assert_eq!(terminal.details["outcome"], "completed");
}

#[test]
fn worktree_runs_record_no_in_place_details() {
    let directory = tempfile::tempdir().unwrap();
    let store = SessionStore::new(&directory.path().join("state"));
    let session = store.create_session_with_id("isolated");
    let started = run_started_workspace(directory.path(), store.sessions_dir(), &session.id);
    assert_eq!(started["workspace"], "worktree");
    assert_eq!(started["workspace_root"], Value::Null);
}

#[test]
fn an_own_change_outside_the_scope_stales_the_check_even_when_its_call_failed() {
    let verified = verified_in_place(|root| vec![changed(root, &["a.txt"], true)]);
    let root = verified.root.to_string_lossy().to_string();
    verified
        .store
        .update_session(&verified.session_id, |session| {
            // `sed -i ... b.txt && cargo test` failed after editing b.txt, so
            // nothing invalidated the check when it ran.
            session.tool_calls.push(changed(&root, &["b.txt"], true));
            Ok(())
        })
        .unwrap();
    assert_eq!(verified.revalidate(), vec![verified.obligation_id.clone()]);
}

#[test]
fn an_attempt_without_a_record_stales_a_narrowed_check() {
    let verified = verified_in_place(|root| vec![changed(root, &["a.txt"], true)]);
    verified
        .store
        .update_session(&verified.session_id, |session| {
            // Cancelled mid-command: the attempt was logged, the record never.
            append_session_event(
                session,
                "tool_attempted",
                json!({
                    "invocation_id": crate::tools::ToolInvocationId::new(),
                    "tool_name": "run_terminal",
                }),
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(verified.revalidate(), vec![verified.obligation_id.clone()]);
}

#[test]
fn records_without_change_details_keep_the_whole_folder() {
    let verified = verified_in_place(|root| {
        vec![
            changed(root, &["a.txt"], true),
            // Written before phase 5b: an edit with no change record.
            ToolCallRecord {
                tool_name: Some("apply_patch".to_string()),
                timestamp: Some(Utc::now()),
                ..Default::default()
            },
            // Tools that never edit files need no change record.
            ToolCallRecord {
                tool_name: Some("read_file".to_string()),
                timestamp: Some(Utc::now()),
                ..Default::default()
            },
        ]
    });
    assert_eq!(verified.obligation().identity_scope, None);
}
