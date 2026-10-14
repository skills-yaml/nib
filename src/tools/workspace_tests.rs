use super::*;
use crate::session::WorkspaceMode;

fn project() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".git")).unwrap();
    root
}

fn mode(workspace: &SessionWorkspace) -> (WorkspaceMode, &str) {
    (workspace.mode, workspace.reason.as_str())
}

#[test]
fn sessions_work_in_place_by_default() {
    let root = project();
    let decided = decide(root.path(), "in_place", None, "s1");
    assert_eq!(mode(&decided), (WorkspaceMode::InPlace, DEFAULT_REASON));
}

#[test]
fn configuration_and_instructions_require_a_worktree() {
    let root = project();
    let recorded = SessionWorkspace::in_place(DEFAULT_REASON);
    assert_eq!(
        mode(&decide(root.path(), "worktree", Some(&recorded), "s1")),
        (WorkspaceMode::Worktree, CONFIGURED_REASON)
    );

    std::fs::write(
        root.path().join("AGENTS.md"),
        "# Rules\n\n- nib-workspace: worktree\n",
    )
    .unwrap();
    assert_eq!(
        mode(&decide(root.path(), "in_place", Some(&recorded), "s1")),
        (
            WorkspaceMode::Worktree,
            "AGENTS.md requires a worktree (nib-workspace: worktree)"
        )
    );
}

#[test]
fn prose_about_isolation_is_not_a_requirement() {
    let root = project();
    std::fs::write(
        root.path().join("AGENTS.md"),
        "Keep the primary checkout coordination-only during concurrent edits.\n\
         nib-workspace: in_place\n",
    )
    .unwrap();
    assert!(!decide(root.path(), "in_place", None, "s1").is_worktree());
}

#[test]
fn a_recorded_worktree_is_kept() {
    let root = project();
    let recorded = SessionWorkspace::worktree(COMMAND_REASON);
    assert_eq!(
        decide(root.path(), "in_place", Some(&recorded), "s1"),
        recorded
    );
}

#[test]
fn a_linked_worktree_project_always_works_in_place() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".git"), "gitdir: elsewhere\n").unwrap();
    let recorded = SessionWorkspace::worktree(COMMAND_REASON);
    assert_eq!(
        mode(&decide(root.path(), "worktree", Some(&recorded), "s1")),
        (WorkspaceMode::InPlace, LINKED_PROJECT_REASON)
    );
}

#[test]
fn a_session_from_before_phase_5c_keeps_its_worktree() {
    let repository = crate::interactive::tests::git_repository();
    let store = crate::session::SessionStore::for_project(repository.path()).unwrap();
    let session = store.try_create_session().unwrap();
    crate::integrations::worktree::WorktreeManager::new(repository.path().to_path_buf())
        .create_for_session(&session.id)
        .unwrap();

    assert_eq!(
        mode(&decide(repository.path(), "in_place", None, &session.id)),
        (WorkspaceMode::Worktree, LEGACY_REASON)
    );
    // A session that recorded in place keeps working in place, even when a
    // scheduled run created a worktree for it.
    let recorded = SessionWorkspace::in_place(DEFAULT_REASON);
    assert!(!decide(repository.path(), "in_place", Some(&recorded), &session.id).is_worktree());
    assert!(!decide(repository.path(), "in_place", None, "another-session").is_worktree());
}
