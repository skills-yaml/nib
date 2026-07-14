//! Git worktree abstractions for subagent isolation.

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Worktree {
    pub path: PathBuf,
}

impl Worktree {
    /// Creates a new git worktree in the `.nib/worktrees/` directory based on the current HEAD.
    pub fn create(project_root: &Path, id: &str) -> Result<Self, String> {
        let worktree_dir = project_root.join(".nib").join("worktrees").join(id);
        
        if worktree_dir.exists() {
            return Err(format!("Worktree {} already exists", id));
        }

        // Branch name
        let branch_name = format!("nib-subagent-{}", id);

        // Command: git worktree add -b <branch_name> <path> HEAD
        let output = Command::new("git")
            .current_dir(project_root)
            .args(["worktree", "add", "-b", &branch_name, worktree_dir.to_str().unwrap(), "HEAD"])
            .output()
            .map_err(|e| format!("Failed to run git worktree: {}", e))?;

        if !output.status.success() {
            return Err(format!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Ok(Self { path: worktree_dir })
    }

    /// Removes the worktree.
    pub fn remove(project_root: &Path, id: &str) -> Result<(), String> {
        let worktree_dir = project_root.join(".nib").join("worktrees").join(id);
        
        if !worktree_dir.exists() {
            return Ok(());
        }

        // git worktree remove -f <path>
        let output = Command::new("git")
            .current_dir(project_root)
            .args(["worktree", "remove", "-f", worktree_dir.to_str().unwrap()])
            .output()
            .map_err(|e| format!("Failed to remove git worktree: {}", e))?;

        if !output.status.success() {
            return Err(format!(
                "git worktree remove failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        // Also delete the branch: git branch -D nib-subagent-id
        let branch_name = format!("nib-subagent-{}", id);
        let _ = Command::new("git")
            .current_dir(project_root)
            .args(["branch", "-D", &branch_name])
            .output();

        Ok(())
    }
}
