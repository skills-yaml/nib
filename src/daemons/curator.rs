use std::path::{Path, PathBuf};
use chrono::{Utc, Duration};
use crate::session::SessionStore;

pub struct Curator {
    project_root: PathBuf,
    retention_days: i64,
}

impl Curator {
    pub fn new(project_root: &Path, retention_days: i64) -> Self {
        Self {
            project_root: project_root.to_path_buf(),
            retention_days,
        }
    }

    pub fn cleanup_old_sessions(&self) -> Result<usize, String> {
        let store = SessionStore::new(&self.project_root);
        let sessions = store.list();
        let cutoff = Utc::now() - Duration::days(self.retention_days);
        
        let mut deleted_count = 0;
        for session_id in sessions {
            if let Some(session) = store.load(&session_id) {
                // If the session has no messages or the last message is older than cutoff
                let is_old = session.messages.last()
                    .and_then(|m| m.timestamp)
                    .map(|ts| ts < cutoff)
                    .unwrap_or(true); // if no timestamp, consider it old if it has no messages
                
                if is_old && session.messages.is_empty() {
                    // Safe to delete empty sessions
                    let _ = std::fs::remove_file(store.sessions_dir().join(format!("{}.json", session_id)));
                    deleted_count += 1;
                } else if is_old {
                    let _ = std::fs::remove_file(store.sessions_dir().join(format!("{}.json", session_id)));
                    deleted_count += 1;
                }
            }
        }
        
        Ok(deleted_count)
    }
}
