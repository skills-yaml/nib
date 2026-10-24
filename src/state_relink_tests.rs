use super::*;
use crate::paths::TestNibHome;

/// A project folder `<parent>/<name>` in a temporary directory, with a Git
/// repository, whose profiles were loaded once (as every nib start does).
struct Folder {
    _parent: tempfile::TempDir,
    root: PathBuf,
}

impl Folder {
    fn new(name: &str) -> Self {
        let parent = tempfile::tempdir().expect("parent");
        let root = parent.path().canonicalize().expect("parent").join(name);
        fs::create_dir(&root).expect("project");
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "nib-tests@example.invalid"],
            vec!["config", "user.name", "nib tests"],
            vec!["commit", "--quiet", "--allow-empty", "-m", "root"],
        ] {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .expect("git")
                .success());
        }
        let folder = Self {
            _parent: parent,
            root,
        };
        folder.load();
        folder
    }

    fn load(&self) -> crate::profile::ProfileRegistry {
        crate::profile::ProfileRegistry::load(&self.root, &crate::config::ProfilesConfig::default())
            .expect("profiles")
    }

    fn store(&self) -> crate::session::SessionStore {
        crate::session::SessionStore::for_project(&self.root).expect("session store")
    }

    /// Moves the folder to another parent, keeping its name.
    fn move_away(self) -> Self {
        let parent = tempfile::tempdir().expect("new parent");
        let root = parent
            .path()
            .canonicalize()
            .expect("new parent")
            .join(self.root.file_name().expect("name"));
        fs::rename(&self.root, &root).expect("move project");
        Self {
            _parent: parent,
            root,
        }
    }
}

fn unique_name() -> String {
    format!("proj-{}", uuid::Uuid::new_v4().simple())
}

#[test]
fn a_moved_project_is_found_and_relinked_with_its_sessions_and_memory() {
    let _home = TestNibHome::isolated();
    let original = Folder::new(&unique_name());
    let session = original.store().create_session_with_id("before-the-move");
    original
        .store()
        .append_message(&session.id, "user", "remember this");
    original
        .load()
        .default_profile()
        .memory_store()
        .set_user("style", "terse")
        .expect("memory");
    let old_root = original.root.clone();
    let old_state = NibPaths::user_state_dir(&old_root).expect("old state");

    let moved = original.move_away();
    // nib started in the new folder before the user re-linked it, and ran a
    // session there.
    moved.load();
    moved.store().create_session_with_id("after-the-move");
    let candidates = find_candidates(&moved.root).expect("candidates");
    let candidate = candidates
        .iter()
        .find(|candidate| candidate.recorded_root == old_root)
        .expect("the old state is a candidate");
    assert_eq!(candidate.kind, StateLinkKind::Moved);
    assert_eq!(candidate.sessions, 1);

    let outcome = relink(&moved.root, &old_root.display().to_string()).expect("relink");

    assert_eq!(outcome.sessions_moved, 1);
    assert_eq!(
        outcome.state_dir,
        NibPaths::user_state_dir(&moved.root).unwrap()
    );
    assert!(!old_state.exists());
    let store = moved.store();
    assert_eq!(store.list(), vec!["before-the-move".to_string()]);
    assert_eq!(
        store.load("before-the-move").expect("session").messages[0].content,
        "remember this"
    );
    assert_eq!(
        moved
            .load()
            .default_profile()
            .memory_store()
            .user("style")
            .as_deref(),
        Some("terse")
    );
    // The new folder's own state was kept aside with its session, not lost.
    assert_eq!(outcome.replaced.len(), 1);
    assert!(outcome.replaced[0].join(PROJECT_IDENTITY_FILE).is_file());
    let aside_key = outcome.replaced[0]
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap()
        .to_string();
    assert!(session_counts()
        .iter()
        .any(|(key, _, count)| key == &aside_key && *count == 1));
    // `nib doctor` names it, so it can be brought back.
    assert_eq!(
        layout_report(&moved.root).expect("layout").asides,
        outcome.replaced
    );
    assert_eq!(
        recorded_root_of(&outcome.state_dir).as_deref(),
        Some(moved.root.as_path())
    );
    assert!(!outcome.state_dir.join(RELINK_RECORD_FILE).exists());
    assert!(find_candidates(&moved.root)
        .unwrap()
        .iter()
        .all(|candidate| candidate.recorded_root != old_root));
    let error = relink(&moved.root, &old_root.display().to_string()).expect_err("done");
    assert!(error.contains("has no state"), "{error}");
}

#[test]
fn a_copy_keeps_its_own_state_and_is_never_relinked() {
    let _home = TestNibHome::isolated();
    let original = Folder::new(&unique_name());
    original.store().create_session_with_id("original-session");
    let copy_parent = tempfile::tempdir().expect("copy parent");
    let copy_root = copy_parent
        .path()
        .canonicalize()
        .unwrap()
        .join(original.root.file_name().unwrap());
    assert!(std::process::Command::new("cp")
        .arg("-a")
        .arg(&original.root)
        .arg(&copy_root)
        .status()
        .expect("cp")
        .success());
    let copy = Folder {
        _parent: copy_parent,
        root: copy_root,
    };

    copy.load();
    assert!(copy.store().list().is_empty(), "the copy starts fresh");
    let candidates = find_candidates(&copy.root).expect("candidates");
    assert!(candidates.iter().any(|candidate| {
        candidate.recorded_root == original.root && candidate.kind == StateLinkKind::Copied
    }));

    let error = relink(&copy.root, &original.root.display().to_string()).expect_err("copy");
    assert!(error.contains("copy"), "{error}");
    assert_eq!(
        original.store().list(),
        vec!["original-session".to_string()]
    );
}

#[test]
fn relink_waits_while_the_old_state_is_in_use() {
    let _home = TestNibHome::isolated();
    let original = Folder::new(&unique_name());
    let old_root = original.root.clone();
    let old_state = NibPaths::user_state_dir(&old_root).unwrap();
    let presence = old_state.join("profiles/default/presence/live.json");
    fs::create_dir_all(presence.parent().unwrap()).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    fs::write(
        &presence,
        format!("{{\"session_id\":\"live\",\"folder\":\"/w\",\"updated_at\":{now}}}"),
    )
    .unwrap();
    let moved = original.move_away();

    let error = relink(&moved.root, &old_root.display().to_string()).expect_err("in use");

    assert!(error.contains("nothing was changed"), "{error}");
    assert!(old_state.join(PROJECT_IDENTITY_FILE).is_file());
    assert!(!old_state.join(RELINK_RECORD_FILE).exists());
}

#[test]
fn an_interrupted_relink_finishes_when_the_profiles_load() {
    let _home = TestNibHome::isolated();
    let original = Folder::new(&unique_name());
    original.store().create_session_with_id("kept-across");
    let old_root = original.root.clone();
    let old_state = NibPaths::user_state_dir(&old_root).unwrap();
    let old_key = old_state.file_name().unwrap().to_str().unwrap().to_string();
    let parent = tempfile::tempdir().unwrap();
    let new_root = parent
        .path()
        .canonicalize()
        .unwrap()
        .join(old_root.file_name().unwrap());
    fs::rename(&old_root, &new_root).unwrap();
    let new_state = NibPaths::user_state_dir(&new_root).unwrap();
    let new_key = new_state.file_name().unwrap().to_str().unwrap().to_string();
    // A re-link stopped after the state directory moved, before its sessions.
    let journal = RelinkRecord {
        version: 1,
        from: old_key,
        to: new_key,
        root: new_root.clone(),
        started_at: chrono::Utc::now().to_rfc3339(),
        asides: Vec::new(),
        receipts: None,
        sessions_moved: false,
    };
    write_journal(&old_state, &journal).unwrap();
    rewrite_root(&old_state.join(PROJECT_IDENTITY_FILE), &new_root, true).unwrap();
    fs::rename(&old_state, &new_state).unwrap();

    crate::profile::ProfileRegistry::load(&new_root, &crate::config::ProfilesConfig::default())
        .expect("profiles finish the re-link");

    assert!(!new_state.join(RELINK_RECORD_FILE).exists());
    let store = crate::session::SessionStore::for_project(&new_root).unwrap();
    assert_eq!(store.list(), vec!["kept-across".to_string()]);
}
