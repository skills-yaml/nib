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

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Name of the directory nib keeps inside a project.
pub const PROJECT_DIR_NAME: &str = ".nib";

/// Git pathspec that leaves the project's nib directory out of a diff.
pub const PROJECT_DIR_EXCLUDE_PATHSPEC: &str = ":(exclude).nib/**";

/// The user home's directory name under the account's home directory.
#[cfg(not(test))]
const USER_HOME_DIR_NAME: &str = ".nib";

/// Resolves nib's on-disk locations. It carries no state: every location is
/// derived from the project root it is given, or from the environment.
pub struct NibPaths;

impl NibPaths {
    /// `$NIB_HOME`, else `~/.nib`. Unit tests use only `TestNibHome`, never
    /// the developer's home or the process environment.
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

#[cfg(test)]
pub(crate) use test_home::TestNibHome;

#[cfg(test)]
mod test_home {
    use std::path::PathBuf;

    thread_local! {
        static TEST_HOME: std::cell::RefCell<Option<PathBuf>> =
            const { std::cell::RefCell::new(None) };
    }

    pub(super) fn current() -> Option<PathBuf> {
        TEST_HOME.with(|home| home.borrow().clone())
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
