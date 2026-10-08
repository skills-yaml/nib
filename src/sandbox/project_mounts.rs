//! Project-aware bwrap filesystem plan (T080 phase 1).
//!
//! The plan exposes the trusted project read-only, makes the working tree and
//! the repository's common Git directory available in the requested mode,
//! hides nib runtime state, and re-mounts the Git paths that later execute
//! outside the sandbox read-only. Every mounted source is derived from the
//! canonical working directory and fixed nib layout, never from the
//! sandbox-writable `.git` pointer file of a linked worktree.

use std::path::{Path, PathBuf};

/// Directories created by nib for managed worktrees, relative to the project.
const MANAGED_WORKTREE_PARENT: [&str; 2] = [".nib", "worktrees"];

/// Git paths whose contents run on the host during the user's own Git
/// commands (hooks, configuration that selects helpers, submodule hooks).
const PROTECTED_GIT_DIRECTORIES: [&str; 3] = ["hooks", "info", "modules"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProjectMounts {
    /// Main checkout that owns the common Git directory.
    root: PathBuf,
    /// Top of the working tree that contains the working directory.
    worktree: PathBuf,
    /// Common Git directory, when the project root has one.
    git_dir: Option<PathBuf>,
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
        let git_dir = Some(root.join(".git")).filter(|path| is_real_directory(path));
        Some(Self {
            root,
            worktree,
            git_dir,
        })
    }

    /// Appends the bind and mask arguments, ending with `--chdir`.
    pub(super) fn append_binds(
        &self,
        args: &mut Vec<String>,
        cwd: &Path,
        writable: bool,
    ) -> Result<(), String> {
        let bind = if writable { "--bind" } else { "--ro-bind" };
        let state = self.root.join(".nib");
        let cwd_in_state = cwd.starts_with(&state);
        let worktree_state = self.worktree.join(".nib");
        if (cwd_in_state && self.worktree == self.root) || cwd.starts_with(&worktree_state) {
            return Err(format!(
                "sandbox working directory is inside nib runtime state: {}",
                cwd.display()
            ));
        }
        push_mount(args, "--ro-bind", &self.root)?;
        if !cwd_in_state {
            push_mount(args, bind, cwd)?;
        }
        if let Some(git_dir) = &self.git_dir {
            if !git_dir.starts_with(cwd) {
                push_mount(args, bind, git_dir)?;
            }
        }
        append_state_mask(args, &state)?;
        if cwd_in_state {
            push_mount(args, bind, cwd)?;
        }
        if self.worktree != self.root {
            append_state_mask(args, &worktree_state)?;
        }
        args.extend(["--chdir".to_string(), utf8(cwd)?.to_string()]);
        Ok(())
    }

    /// Appends read-only re-mounts for executable Git surfaces. This runs
    /// after all writable binds, including configured `allow_write` paths, so
    /// no configuration can make them writable again.
    pub(super) fn append_git_protections(
        &self,
        args: &mut Vec<String>,
        cwd: &Path,
    ) -> Result<(), String> {
        let Some(git_dir) = &self.git_dir else {
            return Ok(());
        };
        for name in PROTECTED_GIT_DIRECTORIES {
            protect_directory(args, &git_dir.join(name))?;
        }
        protect_existing_file(args, &git_dir.join("config"))?;
        let worktree_config = git_config_bool(git_dir, "extensions.worktreeConfig");
        for config in worktree_config_files(git_dir) {
            if config.exists() || worktree_config {
                protect_file(args, &config)?;
            }
        }
        let writable_roots = [cwd, git_dir.as_path()];
        for hooks_path in self.hooks_paths(git_dir) {
            if writable_roots
                .iter()
                .any(|root| hooks_path.starts_with(root))
            {
                protect_directory(args, &hooks_path)?;
            }
        }
        // Included configuration can set hooks, fsmonitor or aliases just like
        // the main file, so writable included files are protected as well.
        let roots: Vec<PathBuf> = std::iter::once(git_dir.join("config"))
            .chain(worktree_config_files(git_dir))
            .collect();
        for included in included_config_files(&roots) {
            if writable_roots.iter().any(|root| included.starts_with(root)) {
                protect_file(args, &included)?;
            }
        }
        Ok(())
    }

    fn hooks_paths(&self, git_dir: &Path) -> Vec<PathBuf> {
        let mut configured = Vec::new();
        for file in std::iter::once(git_dir.join("config")).chain(worktree_config_files(git_dir)) {
            if let Some(value) = git_config_value(&file, "core.hooksPath") {
                configured.push(PathBuf::from(value));
            }
        }
        let mut paths = Vec::new();
        for value in configured {
            if value.is_absolute() {
                paths.push(value);
            } else {
                paths.push(self.root.join(&value));
                paths.push(self.worktree.join(&value));
            }
        }
        paths.sort();
        paths.dedup();
        paths
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

fn worktree_config_files(git_dir: &Path) -> Vec<PathBuf> {
    let mut files = vec![git_dir.join("config.worktree")];
    if let Ok(entries) = std::fs::read_dir(git_dir.join("worktrees")) {
        let mut registrations: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| is_real_directory(path))
            .map(|path| path.join("config.worktree"))
            .collect();
        registrations.sort();
        files.extend(registrations);
    }
    files
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

fn protect_directory(args: &mut Vec<String>, path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(symlink_error(path)),
        Ok(metadata) if metadata.is_dir() => push_mount(args, "--ro-bind", path),
        Ok(_) => protect_file(args, path),
        Err(_) => {
            // An empty read-only tmpfs keeps the sandbox from creating the
            // path. bwrap leaves an empty directory as the host mount point.
            let path = utf8(path)?;
            args.extend([
                "--tmpfs".to_string(),
                path.to_string(),
                "--remount-ro".to_string(),
                path.to_string(),
            ]);
            Ok(())
        }
    }
}

fn protect_existing_file(args: &mut Vec<String>, path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(symlink_error(path)),
        Ok(metadata) if metadata.is_file() => push_mount(args, "--ro-bind", path),
        _ => Ok(()),
    }
}

fn protect_file(args: &mut Vec<String>, path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(symlink_error(path)),
        Ok(_) => push_mount(args, "--ro-bind", path),
        Err(_) => {
            args.extend([
                "--ro-bind".to_string(),
                "/dev/null".to_string(),
                utf8(path)?.to_string(),
            ]);
            Ok(())
        }
    }
}

fn symlink_error(path: &Path) -> String {
    format!(
        "Git path {} is a symbolic link; the sandbox cannot protect it from replacement",
        path.display()
    )
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

fn git_config_bool(git_dir: &Path, key: &str) -> bool {
    git_config_value(&git_dir.join("config"), key).is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "true" | "yes" | "on" | "1"
        )
    })
}

/// Maximum nesting followed when collecting included configuration files.
const MAX_INCLUDE_DEPTH: usize = 10;

/// Collects `include.path` and `includeIf.<condition>.path` targets reachable
/// from the given configuration files, resolving relative paths against the
/// including file as Git does. Conditions are ignored, so every possible
/// include is protected.
fn included_config_files(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut pending: Vec<(PathBuf, usize)> = roots.iter().map(|root| (root.clone(), 0)).collect();
    let mut seen = std::collections::BTreeSet::new();
    let mut included = Vec::new();
    while let Some((file, depth)) = pending.pop() {
        if depth >= MAX_INCLUDE_DEPTH || !seen.insert(file.clone()) {
            continue;
        }
        let base = file.parent().map(Path::to_path_buf).unwrap_or_default();
        for value in git_config_include_paths(&file) {
            let target = if let Some(relative) = value.strip_prefix("~/") {
                match std::env::var_os("HOME") {
                    Some(home) => PathBuf::from(home).join(relative),
                    None => continue,
                }
            } else if Path::new(&value).is_absolute() {
                PathBuf::from(&value)
            } else {
                base.join(&value)
            };
            included.push(target.clone());
            pending.push((target, depth + 1));
        }
    }
    included.sort();
    included.dedup();
    included
}

fn git_config_include_paths(file: &Path) -> Vec<String> {
    if !file.is_file() {
        return Vec::new();
    }
    let Ok(output) = std::process::Command::new("git")
        .args(["config", "--file"])
        .arg(file)
        .args(["--no-includes", "--get-regexp", r"^include(if\..*)?\.path$"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            line.split_once(' ')
                .map(|(_, value)| value.trim().to_string())
        })
        .filter(|value| !value.is_empty())
        .collect()
}

/// Reads one key from a single configuration file. Git only parses the file
/// here; it executes no helpers and ignores ambient system/global files.
fn git_config_value(file: &Path, key: &str) -> Option<String> {
    if !file.is_file() {
        return None;
    }
    let output = std::process::Command::new("git")
        .args(["config", "--file"])
        .arg(file)
        .args(["--includes", "--get", key])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}
