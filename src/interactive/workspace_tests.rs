use super::*;
use crate::interactive::tests::git_repository;

#[test]
fn worktree_command_moves_the_session_into_a_worktree_from_its_next_request() {
    let repository = git_repository();
    let store = SessionStore::for_project(repository.path()).unwrap();
    let session = store.try_create_session().unwrap();
    assert_eq!(
        workspace_label(repository.path(), Some(&session), &session.id).unwrap(),
        format!("in place ({})", workspace::DEFAULT_REASON)
    );

    let message = request_worktree(
        repository.path(),
        &store,
        &session.id,
        workspace::COMMAND_REASON,
    )
    .unwrap();
    assert!(message.contains("From your next request"), "{message}");
    assert!(message.contains("stay in the folder"), "{message}");
    let stored = store.load(&session.id).unwrap();
    assert_eq!(
        stored.workspace,
        Some(SessionWorkspace::worktree(workspace::COMMAND_REASON))
    );
    assert_eq!(
        workspace_label(repository.path(), Some(&stored), &session.id).unwrap(),
        "worktree not created yet (you ran /worktree)"
    );

    let again = request_worktree(
        repository.path(),
        &store,
        &session.id,
        workspace::COMMAND_REASON,
    )
    .unwrap();
    assert!(
        again.contains("already works in its own worktree"),
        "{again}"
    );
}

#[test]
fn worktree_command_is_refused_in_a_linked_worktree_project() {
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join(".git"), "gitdir: elsewhere\n").unwrap();
    let store = SessionStore::new(folder.path());
    let session = store.create_session_with_id("linked");
    let message = request_worktree(
        folder.path(),
        &store,
        &session.id,
        workspace::COMMAND_REASON,
    )
    .unwrap();
    assert!(message.contains("itself a Git worktree"), "{message}");
    assert_eq!(store.load(&session.id).unwrap().workspace, None);
}

#[test]
fn diff_follows_the_recorded_workspace() {
    let repository = git_repository();
    let store = SessionStore::for_project(repository.path()).unwrap();
    let session = store.try_create_session().unwrap();
    let worktree =
        crate::integrations::worktree::WorktreeManager::new(repository.path().to_path_buf())
            .create_for_session(&session.id)
            .unwrap();
    std::fs::write(repository.path().join("tracked.txt"), "folder change\n").unwrap();
    std::fs::write(worktree.join("tracked.txt"), "worktree change\n").unwrap();
    store
        .update_session(&session.id, |session| {
            session.workspace = Some(SessionWorkspace::in_place(workspace::DEFAULT_REASON));
            Ok(())
        })
        .unwrap();

    let diff =
        super::super::bounded_session_workspace_diff(repository.path(), &store, &session.id, &[])
            .unwrap();
    assert!(diff.contains("folder change"), "{diff}");
    assert!(!diff.contains("worktree change"), "{diff}");
}
