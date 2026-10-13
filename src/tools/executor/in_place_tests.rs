//! T080 phase 5b: in place, each audited edit records the project files it
//! changed; in a managed worktree nothing is recorded.

use super::stale_write_tests::{call, executor, git, patch, read, shared_folder};
use super::*;
use crate::session::ChangedPaths;
use crate::tools::permission_rules::TestNibHome;

fn changed_paths(store: &SessionStore, session_id: &str) -> Vec<Option<ChangedPaths>> {
    store
        .load_result(session_id)
        .unwrap()
        .unwrap()
        .tool_calls
        .into_iter()
        .map(|record| record.changed_paths)
        .collect()
}

const ONE_TO_TWO: &str = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+two\n";

#[tokio::test]
async fn in_place_patches_and_commands_record_what_they_changed() {
    let _home = TestNibHome::isolated();
    let (_repository, root) = shared_folder();
    let top = root.canonicalize().unwrap().to_string_lossy().to_string();
    let store = SessionStore::new(&root);
    let session = store.create_session_with_id("in-place-session");
    let mut executor = executor(&root, &store);

    read(&mut executor, &root, &session.id, "a.txt").await;
    let dry_run = patch(&mut executor, &root, &session.id, ONE_TO_TWO, true).await;
    assert!(dry_run.success, "{dry_run:?}");
    let applied = patch(&mut executor, &root, &session.id, ONE_TO_TWO, false).await;
    assert!(applied.success, "{applied:?}");
    let command = executor
        .execute(
            call(
                &root,
                &session.id,
                "run_terminal",
                json!({"command": "printf x > b.txt && rm a.txt"}),
            ),
            Some(&session.id),
        )
        .await;
    assert!(command.success, "{command:?}");

    let recorded = |paths: &[&str]| {
        Some(ChangedPaths {
            root: top.clone(),
            paths: paths.iter().map(|path| (*path).to_string()).collect(),
            complete: true,
        })
    };
    assert_eq!(
        changed_paths(&store, &session.id),
        vec![
            None, // read_file
            None, // dry run
            recorded(&["a.txt"]),
            recorded(&["a.txt", "b.txt"]),
        ]
    );
}

#[tokio::test]
async fn a_managed_worktree_records_no_changed_paths() {
    let _home = TestNibHome::isolated();
    let repository = tempfile::tempdir().unwrap();
    let root = repository.path().join("main");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.txt"), "one\n").unwrap();
    git(&root, &["init", "--quiet"]);
    git(
        &root,
        &["config", "user.email", "nib-tests@example.invalid"],
    );
    git(&root, &["config", "user.name", "nib tests"]);
    git(&root, &["add", "a.txt"]);
    git(&root, &["commit", "--quiet", "-m", "initial"]);
    let store = SessionStore::new(&root);
    let session = store.create_session_with_id("isolated-session");
    let mut executor = executor(&root, &store);

    read(&mut executor, &root, &session.id, "a.txt").await;
    let applied = patch(&mut executor, &root, &session.id, ONE_TO_TWO, false).await;
    assert!(applied.success, "{applied:?}");
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "one\n",
        "the edit landed in the session worktree"
    );
    assert!(changed_paths(&store, &session.id)
        .iter()
        .all(Option::is_none));
}
