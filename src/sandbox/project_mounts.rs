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
    /// writable again. `hidden_home` is the masked home directory, if any.
    pub(super) fn append_protections(
        &self,
        args: &mut Vec<String>,
        cwd: &Path,
        writable: bool,
        hidden_home: Option<&Path>,
        allowed: &[PathBuf],
    ) -> Result<(), String> {
        let workspace = self.workspace(cwd);
        let plan = MaskPlan::new(
            cwd,
            &workspace,
            mode(writable),
            std::slice::from_ref(&self.root),
            allowed,
            hidden_home,
        );
        // The managed worktree lives inside the project state mask; bind it
        // again between the outer and inner masks.
        plan.append(args, self.is_managed().then_some(mode(writable)))?;
        if self.is_managed() {
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

/// Protections for working directories without a trusted project: hide nib
/// state above and below the working directory, keep existing Git metadata in
/// it read-only, and re-bind the working directory in case an ancestor mask
/// covered it (for example a session worktree of a project whose own `.git`
/// is a file).
pub(super) fn append_fallback_protections(
    args: &mut Vec<String>,
    cwd: &Path,
    writable: bool,
    hidden_home: Option<&Path>,
    allowed: &[PathBuf],
) -> Result<(), String> {
    let plan = MaskPlan::new(cwd, cwd, mode(writable), &[], allowed, hidden_home);
    plan.append(args, Some(mode(writable)))?;
    protect_git_entry(args, &cwd.join(".git"))
}

/// Directory names never descended into while looking for nested state and
/// repositories: they are large dependency or build trees.
const SKIPPED_DIRECTORIES: [&str; 5] = [".git", ".nib", "node_modules", "target", ".venv"];
/// Depth below each scan root searched for nested `.nib` and `.git` entries.
const NESTED_SCAN_DEPTH: usize = 3;
/// Upper bound on directories visited per scan root, keeping command start-up
/// cheap in very wide trees.
const NESTED_SCAN_LIMIT: usize = 4096;

struct MaskPlan<'a> {
    cwd: &'a Path,
    /// Primary writable area; it is re-bound inside the masks that contain it.
    workspace: &'a Path,
    /// Bound areas that stay visible even when they are under the hidden home.
    visible_roots: Vec<PathBuf>,
    /// Trees searched for nested `.nib` directories and `.git` entries.
    scan_roots: Vec<PathBuf>,
    /// Writable areas and their bind mode: the workspace and `allow_write`.
    writable_areas: Vec<(PathBuf, &'static str)>,
    hidden_home: Option<&'a Path>,
}

impl<'a> MaskPlan<'a> {
    /// Builds a plan for `workspace` (bound in `kind`), the read-only
    /// `extra_roots` that are also scanned and visible, and the configured
    /// `allow_write` paths, which are treated as further writable areas.
    fn new(
        cwd: &'a Path,
        workspace: &'a Path,
        kind: &'static str,
        extra_roots: &[PathBuf],
        allowed: &[PathBuf],
        hidden_home: Option<&'a Path>,
    ) -> Self {
        let mut roots: Vec<PathBuf> = extra_roots.to_vec();
        roots.push(workspace.to_path_buf());
        roots.extend(allowed.iter().cloned());
        let mut writable_areas = vec![(workspace.to_path_buf(), kind)];
        writable_areas.extend(allowed.iter().map(|path| (path.clone(), "--bind")));
        Self {
            cwd,
            workspace,
            visible_roots: roots.clone(),
            scan_roots: roots,
            writable_areas,
            hidden_home,
        }
    }

    /// Hides every reachable `.nib` directory and keeps existing nested Git
    /// metadata in writable areas read-only. Order:
    /// 1. masks that contain the workspace;
    /// 2. the workspace re-bind (`rebind`);
    /// 3. every directory between a writable area and a nested repository
    ///    root, bound onto itself so it cannot be renamed and recreated;
    /// 4. all other masks;
    /// 5. nested `.git` entries bound read-only.
    fn append(&self, args: &mut Vec<String>, rebind: Option<&str>) -> Result<(), String> {
        let mut states: Vec<PathBuf> = self
            .cwd
            .ancestors()
            .map(|ancestor| ancestor.join(".nib"))
            .filter(|state| std::fs::symlink_metadata(state).is_ok())
            .collect();
        let mut nested_git = Vec::new();
        for root in &self.scan_roots {
            scan_nested(root, &mut states, &mut nested_git);
        }
        states.retain(|state| self.is_visible(state));
        states.sort();
        states.dedup();
        nested_git.retain(|git| self.writable_area(git).is_some());
        nested_git.sort();
        nested_git.dedup();

        let (containing, other): (Vec<_>, Vec<_>) = states
            .iter()
            .partition(|state| self.workspace.starts_with(state));
        for state in containing {
            append_state_mask(args, state)?;
        }
        if let Some(kind) = rebind {
            push_mount(args, kind, self.workspace)?;
        }
        for (directory, kind) in self.repository_parents(&nested_git) {
            push_mount(args, kind, &directory)?;
        }
        for state in other {
            append_state_mask(args, state)?;
        }
        for git in &nested_git {
            push_mount(args, "--ro-bind", git)?;
        }
        Ok(())
    }

    /// The innermost writable area containing `path`, with its bind mode.
    fn writable_area(&self, path: &Path) -> Option<&(PathBuf, &'static str)> {
        self.writable_areas
            .iter()
            .filter(|(area, _)| path.starts_with(area) && path != area)
            .max_by_key(|(area, _)| area.components().count())
    }

    /// Directories from just below each nested repository's writable area
    /// down to the repository root, parents first.
    fn repository_parents(&self, nested_git: &[PathBuf]) -> Vec<(PathBuf, &'static str)> {
        let mut directories = Vec::new();
        for git in nested_git {
            let Some((area, kind)) = self.writable_area(git) else {
                continue;
            };
            for directory in git.ancestors().skip(1) {
                if directory == area || !directory.starts_with(area) {
                    break;
                }
                directories.push((directory.to_path_buf(), *kind));
            }
        }
        directories.sort();
        directories.dedup();
        directories
    }

    /// Paths under the masked home are invisible unless they are inside a
    /// bound area, so they need no mask (and a symlink there is harmless).
    fn is_visible(&self, path: &Path) -> bool {
        self.hidden_home.is_none_or(|home| {
            !path.starts_with(home) || self.visible_roots.iter().any(|root| path.starts_with(root))
        })
    }
}

/// Collects real `.nib` directories and `.git` entries below `root` (not
/// `root` itself) down to [`NESTED_SCAN_DEPTH`]. Symbolic links are neither
/// followed nor collected.
fn scan_nested(root: &Path, states: &mut Vec<PathBuf>, git: &mut Vec<PathBuf>) {
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    let mut visited = 0usize;
    while let Some((directory, depth)) = pending.pop() {
        visited += 1;
        if visited > NESTED_SCAN_LIMIT {
            return;
        }
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        let nested = directory != root;
        for entry in entries.filter_map(Result::ok) {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            let name = entry.file_name();
            if name == ".nib" && file_type.is_dir() && nested {
                states.push(path);
            } else if name == ".git" && (file_type.is_dir() || file_type.is_file()) && nested {
                git.push(path);
            } else if file_type.is_dir()
                && depth + 1 < NESTED_SCAN_DEPTH
                && !SKIPPED_DIRECTORIES.iter().any(|skipped| name == *skipped)
            {
                pending.push((path, depth + 1));
            }
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
