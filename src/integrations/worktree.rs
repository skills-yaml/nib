//! Git worktree isolation for mutating tool execution.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use uuid::Uuid;

pub struct WorktreeManager {
    repo_root: PathBuf,
    worktrees: HashMap<String, PathBuf>,
}

impl WorktreeManager {
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root: repo_root.canonicalize().unwrap_or(repo_root),
            worktrees: HashMap::new(),
        }
    }

    pub fn create_for_session(&mut self, session_id: &str) -> Result<PathBuf, String> {
        if let Some(p) = self.worktrees.get(session_id) {
            return Ok(p.clone());
        }

        if !self.repo_root.join(".git").exists() {
            return Err("Not a git repository".to_string());
        }

        let wt_name = format!(
            "nib-session-{}-{}",
            session_id,
            &Uuid::new_v4().simple().to_string()[..8]
        );
        let wt_path = self.repo_root.join(".worktrees").join(&wt_name);
        std::fs::create_dir_all(wt_path.parent().unwrap()).map_err(|e| e.to_string())?;

        let branch = format!("nib/{session_id}/{wt_name}");
        let status = Command::new("git")
            .current_dir(&self.repo_root)
            .args([
                "worktree",
                "add",
                "-b",
                &branch,
                wt_path.to_str().unwrap_or(""),
                "HEAD",
            ])
            .output()
            .map_err(|e| e.to_string())?;

        if !status.status.success() {
            return Err(format!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&status.stderr)
            ));
        }

        self.worktrees
            .insert(session_id.to_string(), wt_path.clone());
        Ok(wt_path)
    }

    pub fn get_path(&self, session_id: &str) -> Option<&PathBuf> {
        self.worktrees.get(session_id)
    }
}
