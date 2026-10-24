//! T082: the single authority for where nib keeps its files.
//!
//! Every on-disk location nib owns is resolved here, so the layout can move
//! (to `~/.nib`, see T082) without hand-built `.join(".nib")` paths spread
//! through the code. A test forbids new ones outside this module.
//!
//! - [`NibPaths::user_home`]: `$NIB_HOME`, else `~/.nib`. It holds the user
//!   configuration and the per-project settings a repository must not write.
//! - [`NibPaths::project_state`]: a project's runtime state (memory,
//!   daemons, plans, subagents, process scopes, receipts and managed
//!   worktrees), `<user home>/projects/<key>` since T082 phase 3. A project
//!   whose state still lives in its own `.nib` keeps using it until the state
//!   migration (`crate::state_migration`) has moved it.
//! - [`NibPaths::project_prefs`]: a project's preferences (`config.toml`,
//!   `skills/`), which may hold no secrets and no safety settings. The same
//!   in-project directory, [`NibPaths::project_dir`], also holds the legacy
//!   state of older versions, legacy managed worktrees and, until T082 phase
//!   4, the session files.

use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Name of the directory nib keeps inside a project.
pub const PROJECT_DIR_NAME: &str = ".nib";

/// Git pathspec that leaves the project's nib directory out of a diff.
pub const PROJECT_DIR_EXCLUDE_PATHSPEC: &str = ":(exclude).nib/**";

/// The user home's directory name under the account's home directory.
#[cfg(not(test))]
const USER_HOME_DIR_NAME: &str = ".nib";

/// The user configuration file inside the user home.
const USER_CONFIG_FILE: &str = "config.toml";

/// Directory, inside the user home, of per-project user settings.
const USER_PROJECTS_DIR: &str = "projects";

/// Bytes of the canonical-root digest kept in a project key.
const PROJECT_KEY_HASH_BYTES: usize = 8;

/// Characters of the folder name kept in a project key.
const MAX_PROJECT_KEY_NAME_CHARS: usize = 40;

/// Largest Git pointer file read while resolving a linked worktree.
const MAX_GIT_POINTER_BYTES: u64 = 64 * 1024;

/// Records, in a project directory of the user home, which folder it
/// belongs to (`version` and `root`).
pub(crate) const PROJECT_IDENTITY_FILE: &str = "project.toml";

/// Records, in a project's user-home state directory, that the project's
/// runtime state lives there. The state migration writes it, also for a
/// project that had no legacy state to move.
pub(crate) const STATE_RECORD_FILE: &str = "state.toml";

/// Largest identity file read while resolving a managed worktree's project.
const MAX_IDENTITY_FILE_BYTES: u64 = 64 * 1024;

/// Directory, inside a state directory, of the managed worktrees.
const MANAGED_WORKTREES_DIR: &str = "worktrees";

/// Kinds of managed worktrees, one directory each.
const MANAGED_WORKTREE_KINDS: [&str; 2] = ["sessions", "subagents"];

/// Directory, inside a state directory, of the per-profile state.
const PROFILES_DIR: &str = "profiles";

/// Runtime entries older versions kept directly in a project's `.nib`. The
/// state migration moves them to the user home.
pub(crate) const LEGACY_STATE_ENTRIES: [&str; 8] = [
    "command-prefixes.json",
    "daemons",
    "memory.json",
    "plans",
    "process-scopes",
    "subagent-owner-leases",
    "subagents",
    "worktree-ownership",
];

/// Non-session entries of a profile's legacy state directory
/// (`.nib/profiles/<id>`). Sessions and what belongs to them (checkpoints,
/// read ledgers, presence) stay until the SQLite sessions of T082 phase 4.
pub(crate) const LEGACY_PROFILE_STATE_ENTRIES: [&str; 5] = [
    ".legacy-state-migration-v1.json",
    "context.json",
    "daemons",
    "managed-skills",
    "memory.json",
];

/// Most profile directories inspected for legacy state.
const MAX_LEGACY_PROFILE_DIRS: usize = 256;

/// Resolves nib's on-disk locations. It carries no state: every location is
/// derived from the project root it is given, or from the environment.
pub struct NibPaths;

impl NibPaths {
    /// `$NIB_HOME`, else `~/.nib`. Unit tests use only a `TestNibHome` or a
    /// temporary home of the test process, never the developer's home or the
    /// process environment.
    pub fn user_home() -> Option<PathBuf> {
        #[cfg(test)]
        {
            test_home::current()
        }
        #[cfg(not(test))]
        {
            if let Some(path) = std::env::var_os("NIB_HOME").filter(|value| !value.is_empty()) {
                return Some(PathBuf::from(path));
            }
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(USER_HOME_DIR_NAME))
        }
    }

    /// The user configuration file, `<user home>/config.toml`.
    pub fn user_config_file() -> Option<PathBuf> {
        Self::user_home().map(|home| home.join(USER_CONFIG_FILE))
    }

    /// The canonical root a project's user-level settings are keyed by. A
    /// linked Git worktree (a session or subagent worktree) resolves to its
    /// main checkout, so it shares that project's settings.
    pub fn project_identity_root(project_root: &Path) -> Option<PathBuf> {
        let root = project_root.canonicalize().ok()?;
        Some(main_worktree(&root).unwrap_or(root))
    }

    /// The main checkout when `root` is a linked Git worktree.
    pub fn linked_worktree_main(root: &Path) -> Option<PathBuf> {
        main_worktree(&root.canonicalize().ok()?)
    }

    /// A stable name for a project inside the user home: its folder name,
    /// reduced to portable characters, and a SHA-256 prefix of its canonical
    /// root. Copying or moving the project therefore changes the key. A
    /// linked worktree uses its main checkout's key.
    pub fn project_key(project_root: &Path) -> Option<String> {
        Some(key_for(&Self::project_identity_root(project_root)?))
    }

    /// The key of a folder's own runtime state: like [`Self::project_key`],
    /// but a linked worktree the user created keeps state of its own, since
    /// it is a separate working folder with its own subagents and processes.
    pub fn state_key(project_root: &Path) -> Option<String> {
        Some(key_for(&project_root.canonicalize().ok()?))
    }

    /// A project's user-level directory, `<user home>/projects/<key>`. It
    /// holds what the project itself must not set: credentials, endpoints,
    /// approvals, the sandbox level and trust.
    pub fn user_project_dir(project_root: &Path) -> Option<PathBuf> {
        let key = Self::project_key(project_root)?;
        Some(Self::user_home()?.join(USER_PROJECTS_DIR).join(key))
    }

    /// The user-home directory of a folder's runtime state,
    /// `<user home>/projects/<state key>`. For a main checkout it is the
    /// same directory as [`Self::user_project_dir`].
    pub fn user_state_dir(project_root: &Path) -> Option<PathBuf> {
        let key = Self::state_key(project_root)?;
        Some(Self::user_home()?.join(USER_PROJECTS_DIR).join(key))
    }

    /// The directory that holds a project's runtime state.
    ///
    /// It is the project's user-home state directory, except:
    /// - a worktree nib manages keeps its state inside itself, as before
    ///   T082, so it goes away with the worktree;
    /// - a project whose `.nib` still holds legacy runtime state, and whose
    ///   state was never moved, keeps using it until the state migration
    ///   moves it, so no state is split between two places;
    /// - without a user home (no `NIB_HOME` and no home directory) or a
    ///   resolvable project folder, the in-project directory is used.
    pub fn project_state(project_root: &Path) -> PathBuf {
        let legacy = Self::project_dir(project_root);
        if Self::is_managed_worktree(project_root) {
            return legacy;
        }
        let Some(state) = Self::user_state_dir(project_root) else {
            return legacy;
        };
        if is_regular_file(&state.join(STATE_RECORD_FILE)) || !Self::has_legacy_state(project_root)
        {
            state
        } else {
            legacy
        }
    }

    /// The project's own nib directory, `<project>/.nib`: preferences, and
    /// the state, managed worktrees and sessions older versions kept there.
    pub fn project_dir(project_root: &Path) -> PathBuf {
        project_root.join(PROJECT_DIR_NAME)
    }

    /// Whether the project's `.nib` holds runtime state of an older version
    /// that the state migration would move.
    pub fn has_legacy_state(project_root: &Path) -> bool {
        let legacy = Self::project_dir(project_root);
        if LEGACY_STATE_ENTRIES
            .iter()
            .any(|entry| std::fs::symlink_metadata(legacy.join(entry)).is_ok())
        {
            return true;
        }
        let Ok(profiles) = std::fs::read_dir(legacy.join(PROFILES_DIR)) else {
            return false;
        };
        profiles
            .filter_map(Result::ok)
            .take(MAX_LEGACY_PROFILE_DIRS)
            .any(|profile| {
                LEGACY_PROFILE_STATE_ENTRIES
                    .iter()
                    .any(|entry| std::fs::symlink_metadata(profile.path().join(entry)).is_ok())
            })
    }

    /// The default state directory of a profile rooted at `profile_root`:
    /// `<state>/profiles/<id>`.
    pub fn profile_state(profile_root: &Path, profile_id: &str) -> PathBuf {
        Self::project_state(profile_root)
            .join(PROFILES_DIR)
            .join(profile_id)
    }

    /// The default session directory of a profile rooted at `profile_root`.
    /// Sessions stay in the project, `.nib/profiles/<id>/sessions`, until
    /// they move to SQLite (T082 phase 4).
    pub fn profile_sessions(profile_root: &Path, profile_id: &str) -> PathBuf {
        Self::legacy_profile_state(profile_root, profile_id).join("sessions")
    }

    /// A profile's state directory before T082, `.nib/profiles/<id>`.
    pub fn legacy_profile_state(profile_root: &Path, profile_id: &str) -> PathBuf {
        Self::project_dir(profile_root)
            .join(PROFILES_DIR)
            .join(profile_id)
    }

    /// The directory that holds a project's preferences.
    pub fn project_prefs(project_root: &Path) -> PathBuf {
        Self::project_dir(project_root)
    }

    /// Project skills, `<prefs>/skills`.
    pub fn project_skills(project_root: &Path) -> PathBuf {
        Self::project_prefs(project_root).join("skills")
    }

    /// Plan artifacts written by `write_plan`.
    pub fn project_plans(project_root: &Path) -> PathBuf {
        Self::project_state(project_root).join("plans")
    }

    /// Parent of every managed worktree nib creates (`sessions/` and
    /// `subagents/`).
    pub fn managed_worktrees(project_root: &Path) -> PathBuf {
        Self::project_state(project_root).join(MANAGED_WORKTREES_DIR)
    }

    /// Opt-in session worktrees.
    pub fn session_worktrees(project_root: &Path) -> PathBuf {
        Self::managed_worktrees(project_root).join(MANAGED_WORKTREE_KINDS[0])
    }

    /// Subagent worktrees.
    pub fn subagent_worktrees(project_root: &Path) -> PathBuf {
        Self::managed_worktrees(project_root).join(MANAGED_WORKTREE_KINDS[1])
    }

    /// Where managed worktrees of a project may be: the current location
    /// and, for worktrees created before T082, `<project>/.nib/worktrees`.
    /// Legacy worktrees stay where they are and remain manageable.
    pub fn managed_worktree_roots(project_root: &Path) -> Vec<PathBuf> {
        let mut roots = vec![Self::managed_worktrees(project_root)];
        let legacy = Self::project_dir(project_root).join(MANAGED_WORKTREES_DIR);
        if !roots.contains(&legacy) {
            roots.push(legacy);
        }
        roots
    }

    /// [`Self::managed_worktree_roots`] for subagent worktrees.
    pub fn subagent_worktree_roots(project_root: &Path) -> Vec<PathBuf> {
        Self::managed_worktree_roots(project_root)
            .into_iter()
            .map(|root| root.join(MANAGED_WORKTREE_KINDS[1]))
            .collect()
    }

    /// [`Self::managed_worktree_roots`] for session worktrees.
    pub fn session_worktree_roots(project_root: &Path) -> Vec<PathBuf> {
        Self::managed_worktree_roots(project_root)
            .into_iter()
            .map(|root| root.join(MANAGED_WORKTREE_KINDS[0]))
            .collect()
    }

    /// The state directory a managed worktree was created in, when `worktree`
    /// is one: `<state>/worktrees/<kind>/<name>`, where `<state>` is a
    /// project's `.nib` or a project directory of the user home.
    pub fn managed_worktree_state_dir(worktree: &Path) -> Option<PathBuf> {
        let kind = worktree.parent()?;
        let worktrees = kind.parent()?;
        let state = worktrees.parent()?;
        if worktree.file_name().is_none()
            || !MANAGED_WORKTREE_KINDS
                .iter()
                .any(|name| kind.file_name() == Some(OsStr::new(name)))
            || worktrees.file_name()? != MANAGED_WORKTREES_DIR
        {
            return None;
        }
        let in_project = state.file_name()? == PROJECT_DIR_NAME;
        let in_user_home = Self::user_home()
            .is_some_and(|home| state.parent() == Some(home.join(USER_PROJECTS_DIR).as_path()));
        (in_project || in_user_home).then(|| state.to_path_buf())
    }

    /// Whether `root` is the top of a worktree nib manages.
    pub fn is_managed_worktree(root: &Path) -> bool {
        root.canonicalize()
            .ok()
            .and_then(|root| Self::managed_worktree_state_dir(&root))
            .is_some()
    }

    /// The project a managed worktree belongs to. For a worktree in the user
    /// home it is the root its state directory records, accepted only when
    /// that root's state key names the directory, so the worktree's own
    /// (writable) files never decide it.
    pub fn managed_worktree_project(worktree: &Path) -> Option<PathBuf> {
        let state = Self::managed_worktree_state_dir(worktree)?;
        if state.file_name()? == PROJECT_DIR_NAME {
            return state.parent().map(Path::to_path_buf);
        }
        let root = recorded_root(&state.join(PROJECT_IDENTITY_FILE))?;
        (Self::state_key(&root).as_deref() == state.file_name()?.to_str()).then_some(root)
    }

    /// A profile state directory inside a worktree, relative to it. A
    /// subagent keeps its profile state in its own worktree.
    pub fn relative_profile_state(profile_id: &str) -> PathBuf {
        Path::new(PROJECT_DIR_NAME)
            .join(PROFILES_DIR)
            .join(profile_id)
    }

    /// The default project skill directory, relative to the project root.
    pub fn relative_project_skills() -> PathBuf {
        Path::new(PROJECT_DIR_NAME).join("skills")
    }

    /// Whether a path component names a project's nib directory.
    pub fn is_project_dir_name(name: &OsStr) -> bool {
        name == PROJECT_DIR_NAME
    }

    /// Like [`Self::is_project_dir_name`], ignoring ASCII case, for checks
    /// that must also hold on case-insensitive file systems.
    pub fn is_project_dir_name_ignore_case(name: &str) -> bool {
        name.eq_ignore_ascii_case(PROJECT_DIR_NAME)
    }
}

/// A key: the folder name of `root`, reduced to portable characters, and a
/// SHA-256 prefix of the path.
fn key_for(root: &Path) -> String {
    let name: String = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|char| {
            if char.is_ascii_alphanumeric() || matches!(char, '-' | '_' | '.') {
                char
            } else {
                '-'
            }
        })
        .take(MAX_PROJECT_KEY_NAME_CHARS)
        .collect();
    let name = name.trim_start_matches('.');
    let digest = Sha256::digest(root.as_os_str().as_encoded_bytes());
    let hash: String = digest[..PROJECT_KEY_HASH_BYTES]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if name.is_empty() {
        hash
    } else {
        format!("{name}-{hash}")
    }
}

fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

/// The `root` an identity file records, if it is a canonical directory.
fn recorded_root(path: &Path) -> Option<PathBuf> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_IDENTITY_FILE_BYTES {
        return None;
    }
    let table: toml::Table = std::fs::read_to_string(path).ok()?.parse().ok()?;
    let root = PathBuf::from(table.get("root")?.as_str()?);
    (root.canonicalize().ok()? == root).then_some(root)
}

/// The main checkout of a linked worktree, accepted only when the repository
/// links back to this worktree, so a crafted `.git` file cannot claim
/// another project's settings.
fn main_worktree(root: &Path) -> Option<PathBuf> {
    let pointer = root.join(".git");
    if !std::fs::symlink_metadata(&pointer)
        .ok()?
        .file_type()
        .is_file()
    {
        return None;
    }
    let text = read_git_pointer(&pointer)?;
    let gitdir = root
        .join(text.trim().strip_prefix("gitdir:")?.trim())
        .canonicalize()
        .ok()?;
    let back_link = read_git_pointer(&gitdir.join("gitdir"))?;
    if gitdir.join(back_link.trim()).canonicalize().ok()? != pointer.canonicalize().ok()? {
        return None;
    }
    let common = read_git_pointer(&gitdir.join("commondir"))?;
    let common = gitdir.join(common.trim()).canonicalize().ok()?;
    if common.file_name()? != ".git" || gitdir.parent()? != common.join("worktrees") {
        return None;
    }
    common.parent().map(Path::to_path_buf)
}

fn read_git_pointer(path: &Path) -> Option<String> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_GIT_POINTER_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

#[cfg(test)]
pub(crate) use test_home::TestNibHome;

#[cfg(test)]
mod test_home {
    use std::path::PathBuf;

    thread_local! {
        static TEST_HOME: std::cell::RefCell<Option<PathBuf>> =
            const { std::cell::RefCell::new(None) };
    }

    /// The current thread's `TestNibHome`, else one temporary home shared by
    /// the test process. Per-project settings are keyed by project, so tests
    /// with their own temporary projects do not meet in it; a test that
    /// writes the user configuration itself takes a `TestNibHome`.
    pub(super) fn current() -> Option<PathBuf> {
        TEST_HOME
            .with(|home| home.borrow().clone())
            .or_else(|| Some(process_home().to_path_buf()))
    }

    pub(crate) fn process_home() -> &'static std::path::Path {
        static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        HOME.get_or_init(|| {
            tempfile::Builder::new()
                .prefix("nib-unit-test-home-")
                .tempdir()
                .expect("unit test nib home")
        })
        .path()
    }

    /// A temporary user home for the current test thread.
    pub(crate) struct TestNibHome {
        pub(crate) directory: tempfile::TempDir,
        previous: Option<PathBuf>,
    }

    impl TestNibHome {
        pub(crate) fn isolated() -> Self {
            let directory = tempfile::tempdir().expect("nib home");
            let previous =
                TEST_HOME.with(|home| home.replace(Some(directory.path().to_path_buf())));
            Self {
                directory,
                previous,
            }
        }
    }

    impl Drop for TestNibHome {
        fn drop(&mut self) {
            let previous = self.previous.take();
            TEST_HOME.with(|home| *home.borrow_mut() = previous);
        }
    }
}

#[cfg(test)]
#[path = "paths_tests.rs"]
mod tests;
