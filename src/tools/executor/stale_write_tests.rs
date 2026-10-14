//! T080 phase 5a: `apply_patch` changes only files the session read and
//! that are unchanged since, so sessions sharing a folder cannot silently
//! overwrite each other.

use super::*;
use crate::tools::permission_rules::TestNibHome;

pub(super) fn git(root: &Path, arguments: &[&str]) {
    let status = std::process::Command::new("git")
        .args(arguments)
        .current_dir(root)
        .status()
        .expect("git fixture command");
    assert!(status.success(), "git {arguments:?}");
}

/// A linked worktree: nib edits it in place, so two sessions share it.
pub(super) fn shared_folder() -> (tempfile::TempDir, PathBuf) {
    let repository = tempfile::tempdir().expect("repository");
    let main = repository.path().join("main");
    std::fs::create_dir(&main).expect("main");
    std::fs::write(main.join("a.txt"), "one\n").expect("seed");
    git(&main, &["init", "--quiet"]);
    git(
        &main,
        &["config", "user.email", "nib-tests@example.invalid"],
    );
    git(&main, &["config", "user.name", "nib tests"]);
    git(&main, &["add", "a.txt"]);
    git(&main, &["commit", "--quiet", "-m", "initial"]);
    let shared = repository.path().join("shared");
    git(
        &main,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            shared.to_str().expect("utf-8"),
        ],
    );
    (repository, shared)
}

pub(super) fn executor(root: &Path, store: &SessionStore) -> ToolExecutor {
    let config = ExecutionConfig {
        plan_mode: false,
        ..ExecutionConfig::default()
    };
    ToolExecutor::new(root.to_path_buf(), config)
        .with_session_store(store.clone())
        .with_auto_approve(true)
}

pub(super) fn call(root: &Path, session_id: &str, tool: &str, arguments: Value) -> ToolCall {
    ToolCall {
        invocation_id: crate::tools::ToolInvocationId::new(),
        tool_name: tool.to_string(),
        arguments,
        session_id: Some(session_id.to_string()),
        project_root: Some(root.to_path_buf()),
    }
}

pub(super) async fn read(executor: &mut ToolExecutor, root: &Path, session_id: &str, path: &str) {
    let result = executor
        .execute(
            call(root, session_id, "read_file", json!({"path": path})),
            Some(session_id),
        )
        .await;
    assert!(result.success, "{result:?}");
}

pub(super) async fn patch(
    executor: &mut ToolExecutor,
    root: &Path,
    session_id: &str,
    diff: &str,
    dry_run: bool,
) -> ToolResult {
    executor
        .execute(
            call(
                root,
                session_id,
                "apply_patch",
                json!({"patch": diff, "dry_run": dry_run}),
            ),
            Some(session_id),
        )
        .await
}

const ONE_TO_TWO: &str = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+two\n";
const TWO_TO_THREE: &str = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-two\n+three\n";
const ONE_TO_OTHER: &str = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+other\n";

#[tokio::test]
async fn edits_need_a_read_and_consecutive_edits_do_not_need_another() {
    let _home = TestNibHome::isolated();
    let (_repository, root) = shared_folder();
    let store = SessionStore::new(&root);
    let session = store.create_session_with_id("stale-session");
    let mut executor = executor(&root, &store);

    for dry_run in [true, false] {
        let refused = patch(&mut executor, &root, &session.id, ONE_TO_TWO, dry_run).await;
        assert!(!refused.success);
        assert!(
            refused
                .error
                .as_deref()
                .is_some_and(|error| error.contains("read a.txt with read_file before editing it")),
            "{refused:?}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "one\n"
    );
    // A refused edit is not checkpointed.
    let checkpoints =
        crate::tools::checkpoints::CheckpointStore::for_session(store.sessions_dir(), &session.id)
            .unwrap();
    assert!(checkpoints.list().unwrap().is_empty());

    read(&mut executor, &root, &session.id, "a.txt").await;
    let applied = patch(&mut executor, &root, &session.id, ONE_TO_TWO, false).await;
    assert!(applied.success, "{applied:?}");
    let applied = patch(&mut executor, &root, &session.id, TWO_TO_THREE, false).await;
    assert!(applied.success, "{applied:?}");
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "three\n"
    );

    // New files need no read; the edit that created one is remembered.
    let create = "--- /dev/null\n+++ b/b.txt\n@@ -0,0 +1 @@\n+new\n";
    let created = patch(&mut executor, &root, &session.id, create, false).await;
    assert!(created.success, "{created:?}");
    let edit_new = "--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-new\n+newer\n";
    let edited = patch(&mut executor, &root, &session.id, edit_new, false).await;
    assert!(edited.success, "{edited:?}");

    // Once the file is gone (a rewind, a command, another session), the
    // session can create it again without a read that could never succeed.
    std::fs::remove_file(root.join("b.txt")).unwrap();
    let recreated = patch(&mut executor, &root, &session.id, create, false).await;
    assert!(recreated.success, "{recreated:?}");
}

#[tokio::test]
async fn a_change_after_the_read_refuses_the_patch_until_a_re_read() {
    let _home = TestNibHome::isolated();
    let (_repository, root) = shared_folder();
    let store = SessionStore::new(&root);
    let session = store.create_session_with_id("stale-session");
    let mut executor = executor(&root, &store);

    read(&mut executor, &root, &session.id, "a.txt").await;
    // The user (or a formatter) changes the file; the patch still applies
    // textually, so only the stale-write check stops the overwrite.
    std::fs::write(root.join("a.txt"), "one\n\nuser note\n").unwrap();
    let with_note = "--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,3 @@\n-one\n+two\n \n user note\n";
    let refused = patch(&mut executor, &root, &session.id, with_note, false).await;
    assert_eq!(
        refused.error.as_deref(),
        Some("file changed since you read it; re-read a.txt and retry")
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "one\n\nuser note\n"
    );

    read(&mut executor, &root, &session.id, "a.txt").await;
    let applied = patch(&mut executor, &root, &session.id, with_note, false).await;
    assert!(applied.success, "{applied:?}");
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "two\n\nuser note\n"
    );
}

#[tokio::test]
async fn two_sessions_in_one_folder_cannot_overwrite_each_other() {
    let _home = TestNibHome::isolated();
    let (_repository, root) = shared_folder();
    let store = SessionStore::new(&root);
    let first = store.create_session_with_id("first-session");
    let second = store.create_session_with_id("second-session");
    let mut first_executor = executor(&root, &store);
    let mut second_executor = executor(&root, &store);

    read(&mut first_executor, &root, &first.id, "a.txt").await;
    read(&mut second_executor, &root, &second.id, "a.txt").await;
    let applied = patch(&mut first_executor, &root, &first.id, ONE_TO_TWO, false).await;
    assert!(applied.success, "{applied:?}");

    let refused = patch(&mut second_executor, &root, &second.id, ONE_TO_OTHER, false).await;
    assert_eq!(
        refused.error.as_deref(),
        Some("file changed since you read it; re-read a.txt and retry")
    );
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "two\n"
    );
}

#[tokio::test]
async fn reads_and_edits_agree_across_scopes() {
    let _home = TestNibHome::isolated();
    let (_repository, root) = shared_folder();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/c.txt"), "one\n").unwrap();
    let store = SessionStore::new(&root);
    let session = store.create_session_with_id("scoped-session");
    let mut executor = executor(&root, &store);

    // Read from the subdirectory scope, edit from the folder top.
    let mut scoped = call(
        &root.join("sub"),
        &session.id,
        "read_file",
        json!({"path": "c.txt"}),
    );
    scoped.project_root = Some(root.join("sub"));
    let result = executor.execute(scoped, Some(&session.id)).await;
    assert!(result.success, "{result:?}");
    let diff = "--- a/sub/c.txt\n+++ b/sub/c.txt\n@@ -1 +1 @@\n-one\n+two\n";
    let applied = patch(&mut executor, &root, &session.id, diff, false).await;
    assert!(applied.success, "{applied:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_read_through_a_link_covers_the_link_and_its_target() {
    let _home = TestNibHome::isolated();
    let (_repository, root) = shared_folder();
    std::os::unix::fs::symlink("a.txt", root.join("alias")).unwrap();
    let store = SessionStore::new(&root);
    let session = store.create_session_with_id("link-session");
    let mut executor = executor(&root, &store);

    read(&mut executor, &root, &session.id, "alias").await;
    let applied = patch(&mut executor, &root, &session.id, ONE_TO_TWO, false).await;
    assert!(applied.success, "{applied:?}");
    // The link itself is recorded too, so a patch to the link does not ask
    // for a read that would only ever record its target.
    let ledger =
        crate::tools::file_reads::ReadLedger::for_session(store.sessions_dir(), &session.id)
            .unwrap();
    let link =
        crate::tools::file_reads::tracked_patch_paths(&root, Path::new(""), &["alias".to_string()])
            .unwrap();
    ledger.check_before_edit(&link).unwrap();
}
