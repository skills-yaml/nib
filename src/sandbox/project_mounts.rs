//! Project-aware bwrap filesystem plan (T080 phase 1).
//!
//! The plan exposes the trusted project read-only, makes the working tree
//! available in the requested mode, hides nib runtime state and keeps every
//! Git metadata path read-only. Git metadata is never writable from the
//! sandbox: configuration, hooks, `commondir`, `gitdir` pointers and rebase
//! state all execute or redirect execution on the host later, so a block-list
//! cannot make a writable `.git` safe. Git writes go through approved
//! host-side tools instead.
//!
//! Every mounted source is derived from the canonical working directory and
//! nib's fixed layout, never from the sandbox-writable `.git` pointer file of
//! a linked worktree. Protected paths are mount points, so the sandbox cannot
//! rename them (or their bound parents) away and recreate them.

use std::path::{Path, PathBuf};

/// Directories created by nib for managed worktrees, relative to the project.
const MANAGED_WORKTREE_PARENT: [&str; 2] = [".nib", "worktrees"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProjectMounts {
    /// Main checkout that owns the common Git directory.
    root: PathBuf,
    /// Top of the working tree that contains the working directory.
    worktree: PathBuf,
}

impl ProjectMounts {
    /// Resolves the plan for a canonical working directory. Returns `None`
    /// when no trusted project can be identified; callers then bind only the
    /// working directory, which is the historical behavior.
    pub(super) fn resolve(cwd: &Path, home: Option<&Path>) -> Option<Self> {
        let worktree = cwd
            .ancestors()
            .find(|candidate| is_git_entry(&candidate.join(".git")))?
            .to_path_buf();
        let root = if is_real_directory(&worktree.join(".git")) {
            worktree.clone()
        } else {
            managed_worktree_project(&worktree)?
        };
        if !is_safe_root(&root, home) || !is_safe_root(&worktree, home) {
            return None;
        }
        Some(Self { root, worktree })
    }

    fn state(&self) -> PathBuf {
        self.root.join(".nib")
    }

    fn is_managed(&self) -> bool {
        self.worktree != self.root
    }

    /// Rejects working directories inside nib state, then binds the project
    /// read-only and the workspace in the requested mode. Masks and Git
    /// protections follow in [`Self::append_protections`].
    pub(super) fn append_binds(
        &self,
        args: &mut Vec<String>,
        cwd: &Path,
        writable: bool,
    ) -> Result<(), String> {
        let unmanaged_state = !self.is_managed() && cwd.starts_with(self.state());
        if unmanaged_state || cwd.starts_with(self.worktree.join(".nib")) {
            return Err(format!(
                "sandbox working directory is inside nib runtime state: {}",
                cwd.display()
            ));
        }
        push_mount(args, "--ro-bind", &self.root)?;
        push_mount(args, mode(writable), &self.workspace(cwd))
    }

    /// Appends the state masks and read-only Git mounts, then `--chdir`.
    /// Runs after every other bind, including configured `allow_write`
    /// paths, so no configuration can expose nib state or make Git metadata
    /// writable again.
    pub(super) fn append_protections(
        &self,
        args: &mut Vec<String>,
        cwd: &Path,
        writable: bool,
    ) -> Result<(), String> {
        append_state_mask(args, &self.state())?;
        if self.is_managed() {
            // The managed worktree lives inside the state mask; bind it again.
            push_mount(args, mode(writable), &self.workspace(cwd))?;
            append_state_mask(args, &self.worktree.join(".nib"))?;
            protect_git_entry(args, &self.worktree.join(".git"))?;
        }
        protect_git_entry(args, &self.root.join(".git"))?;
        args.extend(["--chdir".to_string(), utf8(cwd)?.to_string()]);
        Ok(())
    }

    /// The writable area: the whole managed worktree, or the working
    /// directory inside the main checkout.
    fn workspace(&self, cwd: &Path) -> PathBuf {
        if self.is_managed() {
            self.worktree.clone()
        } else {
            cwd.to_path_buf()
        }
    }
}

fn mode(writable: bool) -> &'static str {
    if writable {
        "--bind"
    } else {
        "--ro-bind"
    }
}

fn managed_worktree_project(worktree: &Path) -> Option<PathBuf> {
    let kind = worktree.parent()?;
    let worktrees = kind.parent()?;
    let state = worktrees.parent()?;
    let root = state.parent()?;
    let layout_matches = worktrees.file_name()? == MANAGED_WORKTREE_PARENT[1]
        && state.file_name()? == MANAGED_WORKTREE_PARENT[0];
    (layout_matches && is_real_directory(&root.join(".git"))).then(|| root.to_path_buf())
}

fn is_safe_root(path: &Path, home: Option<&Path>) -> bool {
    path.parent().is_some() && home.is_none_or(|home| !home.starts_with(path))
}

fn is_git_entry(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir() || metadata.is_file())
}

fn is_real_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

fn append_state_mask(args: &mut Vec<String>, state: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(state) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "nib state directory is a symbolic link and cannot be hidden from the sandbox: {}",
            state.display()
        )),
        Ok(metadata) if metadata.is_dir() => {
            args.extend(["--tmpfs".to_string(), utf8(state)?.to_string()]);
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Binds a `.git` directory or pointer file onto itself read-only. The mount
/// point cannot be renamed, replaced or written from the sandbox.
fn protect_git_entry(args: &mut Vec<String>, path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "Git metadata {} is a symbolic link; the sandbox cannot protect it",
            path.display()
        )),
        Ok(_) => push_mount(args, "--ro-bind", path),
        Err(_) => Ok(()),
    }
}

fn push_mount(args: &mut Vec<String>, kind: &str, path: &Path) -> Result<(), String> {
    let path = utf8(path)?;
    args.extend([kind.to_string(), path.to_string(), path.to_string()]);
    Ok(())
}

fn utf8(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("sandbox path is not valid UTF-8: {}", path.display()))
}
