//! T082: the single authority for where nib keeps its files.
//!
//! Every on-disk location nib owns is resolved here, so the layout can move
//! (to `~/.nib`, see T082) without hand-built `.join(".nib")` paths spread
//! through the code. A test forbids new ones outside this module.
//!
//! - [`NibPaths::user_home`]: `$NIB_HOME`, else `~/.nib`. It holds the user
//!   configuration and the per-project settings a repository must not write.
//! - [`NibPaths::project_state`]: a project's runtime state (sessions,
//!   memory, daemons, subagents, receipts, managed worktrees).
//! - [`NibPaths::project_prefs`]: a project's preferences (`config.toml`,
//!   `skills/`), which may hold no secrets and no safety settings.

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
    /// root. Copying or moving the project therefore changes the key.
    pub fn project_key(project_root: &Path) -> Option<String> {
        let root = Self::project_identity_root(project_root)?;
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
        Some(if name.is_empty() {
            hash
        } else {
            format!("{name}-{hash}")
        })
    }

    /// A project's user-level directory, `<user home>/projects/<key>`. It
    /// holds what the project itself must not set: credentials, endpoints,
    /// approvals, the sandbox level and trust.
    pub fn user_project_dir(project_root: &Path) -> Option<PathBuf> {
        let key = Self::project_key(project_root)?;
        Some(Self::user_home()?.join(USER_PROJECTS_DIR).join(key))
    }

    /// The directory that holds a project's runtime state.
    pub fn project_state(project_root: &Path) -> PathBuf {
        project_root.join(PROJECT_DIR_NAME)
    }

    /// The directory that holds a project's preferences.
    pub fn project_prefs(project_root: &Path) -> PathBuf {
        project_root.join(PROJECT_DIR_NAME)
    }

    /// Project skills, `<prefs>/skills`.
    pub fn project_skills(project_root: &Path) -> PathBuf {
        Self::project_prefs(project_root).join("skills")
    }

    /// Plan artifacts written by `write_plan`.
    pub fn project_plans(project_root: &Path) -> PathBuf {
        Self::project_state(project_root).join("plans")
    }

    /// Parent of every managed worktree (`sessions/` and `subagents/`).
    pub fn managed_worktrees(project_root: &Path) -> PathBuf {
        Self::project_state(project_root).join("worktrees")
    }

    /// Opt-in session worktrees.
    pub fn session_worktrees(project_root: &Path) -> PathBuf {
        Self::managed_worktrees(project_root).join("sessions")
    }

    /// Subagent worktrees.
    pub fn subagent_worktrees(project_root: &Path) -> PathBuf {
        Self::managed_worktrees(project_root).join("subagents")
    }

    /// The default state directory of a profile, relative to its root.
    pub fn relative_profile_state(profile_id: &str) -> PathBuf {
        Path::new(PROJECT_DIR_NAME)
            .join("profiles")
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
