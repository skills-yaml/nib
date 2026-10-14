//! T080 phase 4: applied patches are checkpointed and `/rewind` restores them.

use super::*;
use crate::interactive::{execute_interactive_command, InteractiveCommand, InteractiveEffect};
use crate::session::MessageOrigin;
use crate::tools::checkpoints::CheckpointStore;
use crate::tools::permission_rules::TestNibHome;

fn ask(store: &SessionStore, session_id: &str, root: &Path, text: &str) {
    crate::agent::r#loop::prepare_user_turn(store, session_id, text, root).expect("human turn");
}

fn patch(root: &Path, session_id: &str, patch: &str, dry_run: bool) -> ToolCall {
    ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "apply_patch".to_string(),
        arguments: json!({"patch": patch, "dry_run": dry_run}),
        session_id: Some(session_id.to_string()),
        project_root: Some(root.to_path_buf()),
    }
}

async fn read_file(executor: &mut ToolExecutor, root: &Path, session_id: &str, path: &str) {
    let call = ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: "read_file".to_string(),
        arguments: json!({"path": path}),
        session_id: Some(session_id.to_string()),
        project_root: Some(root.to_path_buf()),
    };
    let read = executor.execute(call, Some(session_id)).await;
    assert!(read.success, "{read:?}");
}

fn rewind(root: &Path, store: &SessionStore, session_id: &str, checkpoint: Option<u64>) -> String {
    match execute_interactive_command(
        InteractiveCommand::Rewind { checkpoint },
        root,
        store,
        session_id,
    )
    .expect("rewind command")
    {
        InteractiveEffect::Output(output) => output,
        other => panic!("unexpected effect {other:?}"),
    }
}

fn commit_fixture(root: &Path) {
    for arguments in [
        vec!["init", "--quiet"],
        vec!["config", "user.email", "nib-tests@example.invalid"],
        vec!["config", "user.name", "nib tests"],
        vec!["add", "a.txt"],
        vec!["commit", "--quiet", "-m", "initial"],
    ] {
        let status = std::process::Command::new("git")
            .args(arguments)
            .current_dir(root)
            .status()
            .expect("git fixture command");
        assert!(status.success());
    }
}

/// Edits land in the session worktree when there is one.
fn session_workspace(root: &Path, session_id: &str) -> PathBuf {
    crate::integrations::worktree::with_validated_session_worktree(root, session_id, |path| {
        Ok(path.to_path_buf())
    })
    .expect("worktree lookup")
    .unwrap_or_else(|| root.to_path_buf())
}

#[tokio::test]
async fn applied_patches_are_checkpointed_per_turn_and_rewound() {
    let _home = TestNibHome::isolated();
    let root = tempfile::tempdir().expect("root");
    std::fs::write(root.path().join("a.txt"), "one\n").expect("seed");
    commit_fixture(root.path());
    let store = SessionStore::new(root.path());
    let session = store.create_session_with_id("checkpoint-session");
    let config = ExecutionConfig {
        plan_mode: false,
        ..ExecutionConfig::default()
    };
    let mut executor = ToolExecutor::new(root.path().to_path_buf(), config)
        .with_session_store(store.clone())
        .with_auto_approve(true);

    ask(&store, &session.id, root.path(), "change a and add b");
    // Edits need a prior read in the session (phase 5a).
    read_file(&mut executor, root.path(), &session.id, "a.txt").await;
    let edit = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+two\n";
    let create = "--- /dev/null\n+++ b/b.txt\n@@ -0,0 +1 @@\n+new\n";
    // A dry run checks the patch and saves nothing.
    let checked = executor
        .execute(
            patch(root.path(), &session.id, edit, true),
            Some(&session.id),
        )
        .await;
    assert!(checked.success, "{checked:?}");
    let checkpoints =
        CheckpointStore::for_session(store.sessions_dir(), &session.id).expect("store");
    assert!(checkpoints.list().expect("list").is_empty());

    for diff in [edit, create] {
        let applied = executor
            .execute(
                patch(root.path(), &session.id, diff, false),
                Some(&session.id),
            )
            .await;
        assert!(applied.success, "{applied:?}");
    }
    ask(&store, &session.id, root.path(), "change a again");
    let again = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-two\n+three\n";
    let applied = executor
        .execute(
            patch(root.path(), &session.id, again, false),
            Some(&session.id),
        )
        .await;
    assert!(applied.success, "{applied:?}");

    let workspace = session_workspace(root.path(), &session.id);
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).expect("a"),
        "three\n"
    );
    let listed = checkpoints.list().expect("list");
    assert_eq!(listed[0].root, workspace.canonicalize().expect("root"));
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].label, "change a and add b");
    assert_eq!(listed[0].files.len(), 2);
    let listing = rewind(root.path(), &store, &session.id, None);
    assert!(listing.contains("#2"), "{listing}");
    assert!(
        listing.contains("\"change a and add b\", 2 files"),
        "{listing}"
    );

    let output = rewind(root.path(), &store, &session.id, Some(1));
    assert!(output.contains("Restored: a.txt"), "{output}");
    assert!(output.contains("Removed: b.txt"), "{output}");
    assert!(output.contains("Undo with /rewind 3."), "{output}");
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).expect("a"),
        "one\n"
    );
    assert!(!workspace.join("b.txt").exists());

    let session = store
        .load_result(&session.id)
        .expect("load")
        .expect("session");
    let note = session.messages.last().expect("rewind note");
    assert!(
        note.content.contains("before checkpoint #1"),
        "{}",
        note.content
    );
    assert_eq!(
        session.message_origin(note.index),
        MessageOrigin::RuntimeContinuation
    );
    assert!(session
        .events
        .iter()
        .any(|event| event.kind == "files_rewound"));

    let undone = rewind(root.path(), &store, &session.id, Some(3));
    assert!(undone.contains("Restored: a.txt, b.txt"), "{undone}");
    assert_eq!(
        std::fs::read_to_string(workspace.join("a.txt")).expect("a"),
        "three\n"
    );
}

#[test]
fn rewind_arguments_parse_numbers() {
    use crate::interactive::parse_interactive_command;
    assert_eq!(
        parse_interactive_command("/rewind").expect("parse"),
        Some(InteractiveCommand::Rewind { checkpoint: None })
    );
    assert_eq!(
        parse_interactive_command("/rewind #4").expect("parse"),
        Some(InteractiveCommand::Rewind {
            checkpoint: Some(4)
        })
    );
    assert!(parse_interactive_command("/rewind latest").is_err());
}
