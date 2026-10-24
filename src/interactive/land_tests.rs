use super::*;
use crate::integrations::worktree::WorktreeManager;
use serial_test::serial;
use std::path::PathBuf;

/// Keeps the developer's global Git configuration (signing, hooks) out of
/// `/land commit` and `/land merge`, which run the user's own Git.
struct IsolatedGit(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl IsolatedGit {
    fn new() -> Self {
        let keys = ["GIT_CONFIG_GLOBAL", "GIT_CONFIG_NOSYSTEM"];
        let previous = keys
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        Self(previous)
    }
}

impl Drop for IsolatedGit {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("git fixture");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A folder on `feature` with a session that works in its own worktree.
struct Fixture {
    _git: IsolatedGit,
    folder: tempfile::TempDir,
    store: SessionStore,
    session_id: String,
}

impl Fixture {
    fn new() -> Self {
        let isolated = IsolatedGit::new();
        let folder = tempfile::tempdir().unwrap();
        git(
            folder.path(),
            &["init", "--quiet", "--initial-branch", "main"],
        );
        git(folder.path(), &["config", "user.name", "Fixture"]);
        git(
            folder.path(),
            &["config", "user.email", "fixture@example.invalid"],
        );
        std::fs::write(folder.path().join("README.md"), "start\n").unwrap();
        git(folder.path(), &["add", "."]);
        git(folder.path(), &["commit", "--quiet", "-m", "start"]);
        git(folder.path(), &["switch", "--quiet", "-c", "feature"]);
        let store = SessionStore::for_project(folder.path()).unwrap();
        let session_id = store.try_create_session().unwrap().id;
        store
            .update_session(&session_id, |session| {
                session.workspace = Some(SessionWorkspace::worktree(workspace::COMMAND_REASON));
                Ok(())
            })
            .unwrap();
        Self {
            _git: isolated,
            folder,
            store,
            session_id,
        }
    }

    fn root(&self) -> &Path {
        self.folder.path()
    }

    fn create_worktree(&self) -> PathBuf {
        WorktreeManager::new(self.root().to_path_buf())
            .create_for_session(&self.session_id)
            .unwrap()
    }

    fn land(&self, action: LandAction) -> Result<String, String> {
        land(self.root(), &self.store, &self.session_id, &action)
    }

    fn code(&self) -> String {
        let preview = self.land(LandAction::Preview).unwrap();
        preview
            .lines()
            .find_map(|line| line.strip_prefix("/land merge "))
            .and_then(|rest| rest.split_whitespace().next())
            .expect("preview shows a code")
            .to_string()
    }

    fn workspace(&self) -> Option<SessionWorkspace> {
        self.store.load(&self.session_id).unwrap().workspace
    }
}

#[test]
fn land_actions_parse_and_reject_bad_usage() {
    let parse = |words: &[&str]| {
        parse_land_action(
            &words
                .iter()
                .map(|word| (*word).to_string())
                .collect::<Vec<_>>(),
        )
    };
    assert_eq!(parse(&[]), Ok(LandAction::Preview));
    assert_eq!(
        parse(&["commit", "add", "the", "parser"]),
        Ok(LandAction::Commit {
            message: "add the parser".to_string()
        })
    );
    assert_eq!(
        parse(&["merge", "abc1234"]),
        Ok(LandAction::Merge {
            code: "abc1234".to_string()
        })
    );
    assert_eq!(parse(&["discard"]), Ok(LandAction::Discard { code: None }));
    for bad in [&["commit"][..], &["merge"], &["merge", "a", "b"], &["push"]] {
        assert!(parse(bad).unwrap_err().contains("usage: /land"), "{bad:?}");
    }
    assert_eq!(
        crate::interactive::parse_interactive_command("/land discard abc1234").unwrap(),
        Some(crate::interactive::InteractiveCommand::Land {
            action: LandAction::Discard {
                code: Some("abc1234".to_string())
            }
        })
    );
}

#[test]
#[serial]
fn a_worktree_session_is_previewed_committed_and_merged_into_the_folder() {
    let fixture = Fixture::new();
    let worktree = fixture.create_worktree();
    std::fs::write(worktree.join("a.txt"), "from the session\n").unwrap();

    let preview = fixture.land(LandAction::Preview).unwrap();
    assert!(
        preview.contains("Uncommitted in the worktree (1)"),
        "{preview}"
    );
    assert!(preview.contains("?? a.txt"), "{preview}");
    assert!(preview.contains("/land commit <message>"), "{preview}");
    assert!(preview.contains("into feature"), "{preview}");

    // A merge needs the changes committed first.
    let error = fixture
        .land(LandAction::Merge {
            code: fixture.code(),
        })
        .unwrap_err();
    assert!(error.contains("/land commit"), "{error}");

    let committed = fixture
        .land(LandAction::Commit {
            message: "Add a".to_string(),
        })
        .unwrap();
    assert!(committed.starts_with("Committed "), "{committed}");
    assert!(
        committed.contains("1 commit your folder (feature) does not have"),
        "{committed}"
    );
    assert!(committed.contains("No uncommitted changes"), "{committed}");

    // A stale code is refused with the current preview.
    let stale = fixture
        .land(LandAction::Merge {
            code: "0000000".to_string(),
        })
        .unwrap_err();
    assert!(stale.contains("changed since the preview"), "{stale}");
    assert!(!fixture.root().join("a.txt").exists());

    let merged = fixture
        .land(LandAction::Merge {
            code: fixture.code(),
        })
        .unwrap();
    assert!(merged.contains("into feature"), "{merged}");
    assert!(merged.contains("works in your folder"), "{merged}");
    assert_eq!(
        std::fs::read_to_string(fixture.root().join("a.txt")).unwrap(),
        "from the session\n"
    );
    assert!(!worktree.exists());
    assert_eq!(
        git(fixture.root(), &["branch", "--list", "nib/session/*"]),
        ""
    );
    assert_eq!(
        fixture.workspace(),
        Some(SessionWorkspace::in_place(workspace::LANDED_REASON))
    );
    let after = fixture.land(LandAction::Preview).unwrap();
    assert!(after.contains("nothing to land"), "{after}");
}

#[test]
#[serial]
fn discard_needs_the_code_and_a_required_worktree_is_reported() {
    let fixture = Fixture::new();
    let worktree = fixture.create_worktree();
    std::fs::write(worktree.join("a.txt"), "dropped\n").unwrap();
    std::fs::write(
        fixture.root().join("AGENTS.md"),
        format!("{}\n", workspace::WORKTREE_DIRECTIVE),
    )
    .unwrap();

    let error = fixture
        .land(LandAction::Discard { code: None })
        .unwrap_err();
    assert!(error.contains("Confirm with the code"), "{error}");
    assert!(worktree.exists());

    let discarded = fixture
        .land(LandAction::Discard {
            code: Some(fixture.code()),
        })
        .unwrap();
    assert!(
        discarded.contains("dropping 1 uncommitted change"),
        "{discarded}"
    );
    assert!(
        discarded.contains("next request works in a new worktree, because AGENTS.md requires"),
        "{discarded}"
    );
    assert!(!worktree.exists());
    assert!(!fixture.root().join("a.txt").exists());
    assert_eq!(
        fixture.workspace(),
        Some(SessionWorkspace::in_place(workspace::DISCARDED_REASON))
    );
}

#[test]
#[serial]
fn without_a_worktree_there_is_nothing_to_land() {
    let fixture = Fixture::new();
    let waiting = fixture.land(LandAction::Preview).unwrap();
    assert!(waiting.contains("has not created it yet"), "{waiting}");
    assert!(waiting.contains("/land discard"), "{waiting}");

    let back = fixture.land(LandAction::Discard { code: None }).unwrap();
    assert!(back.contains("had not created its worktree"), "{back}");
    assert!(back.contains("works in your folder"), "{back}");
    let in_place = fixture.land(LandAction::Preview).unwrap();
    assert!(in_place.contains("works in your folder"), "{in_place}");
    assert!(in_place.contains("nothing to land"), "{in_place}");
}

#[test]
#[serial]
fn commit_without_changes_and_a_stale_discard_code_change_nothing() {
    let fixture = Fixture::new();
    let worktree = fixture.create_worktree();
    let error = fixture
        .land(LandAction::Commit {
            message: "nothing".to_string(),
        })
        .unwrap_err();
    assert!(error.contains("nothing to commit"), "{error}");

    let code = fixture.code();
    std::fs::write(worktree.join("late.txt"), "written after the preview\n").unwrap();
    let error = fixture
        .land(LandAction::Discard { code: Some(code) })
        .unwrap_err();
    assert!(error.contains("changed since the preview"), "{error}");
    assert!(worktree.join("late.txt").exists());
}

/// Removing a worktree can stop partway (files nib may not delete). nib
/// then cannot load what remains and never deletes it blindly: `/land`
/// explains, and `/land discard` only stops the session from using it.
#[cfg(unix)]
#[test]
#[serial]
fn an_interrupted_removal_is_left_for_the_user_and_the_session_moves_on() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let worktree = fixture.create_worktree();
    let locked = worktree.join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("file"), "kept\n").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::remove_file(locked.join("file")).is_ok() {
        // Running as root: permissions cannot stop the removal.
        return;
    }

    let error = fixture
        .land(LandAction::Discard {
            code: Some(fixture.code()),
        })
        .unwrap_err();
    assert!(
        error.starts_with("Removing the session worktree did not finish"),
        "{error}"
    );
    assert_eq!(
        fixture.workspace(),
        Some(SessionWorkspace::worktree(workspace::COMMAND_REASON))
    );
    let error = fixture.land(LandAction::Preview).unwrap_err();
    assert!(error.contains("run /land discard to stop using"), "{error}");

    let moved_on = fixture.land(LandAction::Discard { code: None }).unwrap();
    assert!(moved_on.contains("Nothing was deleted"), "{moved_on}");
    assert!(moved_on.contains("works in your folder"), "{moved_on}");
    assert_eq!(
        fixture.workspace(),
        Some(SessionWorkspace::in_place(workspace::DISCARDED_REASON))
    );
    // What could not be removed is still there for the user to inspect.
    assert!(crate::paths::NibPaths::project_state(fixture.root())
        .join("worktrees/sessions")
        .read_dir()
        .unwrap()
        .next()
        .is_some());
    let in_place = fixture.land(LandAction::Preview).unwrap();
    assert!(in_place.contains("works in your folder"), "{in_place}");
    assert!(in_place.contains("could not be loaded"), "{in_place}");
    // `/rewind` no longer trips over the worktree it cannot load.
    let rewind =
        crate::interactive::rewind_files(fixture.root(), &fixture.store, &fixture.session_id, 1);
    if let Err(error) = rewind {
        assert!(!error.contains("worktree"), "{error}");
    }
    // Leave the temporary directory removable.
    for entry in walk(fixture.root()) {
        let _ = std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o755));
    }
}

#[cfg(unix)]
fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        found.push(directory);
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                pending.push(entry.path());
            }
        }
    }
    found
}
