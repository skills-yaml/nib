//! File-based session persistence under `<project>/.nib/sessions/`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use uuid::Uuid;

pub mod memory;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMessage {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ToolCallRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub arguments: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bwrap_args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundaries: Option<crate::config::BoundaryConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanStep {
    pub description: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Plan {
    pub steps: Vec<PlanStep>,
    pub current_step_index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub messages: Vec<SessionMessage>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Plan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default)]
    pub summary_index: usize,
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse session JSON: {0}")]
    Json(#[from] serde_json::Error),
}

pub struct SessionStore {
    sessions_dir: PathBuf,
}

impl SessionStore {
    pub fn new(project_root: &Path) -> Self {
        let nib = project_root.join(".nib");
        let sessions_dir = nib.join("sessions");
        let _ = fs::create_dir_all(&sessions_dir);
        Self { sessions_dir }
    }

    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    fn path(&self, id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{id}.json"))
    }

    pub fn create_session(&self) -> Session {
        let session = Session {
            id: Uuid::new_v4().to_string(),
            started_at: Some(Utc::now()),
            messages: vec![],
            tool_calls: vec![],
            plan: None,
            summary: None,
            summary_index: 0,
        };
        let _ = self.save(&session);
        session
    }

    pub fn load(&self, id: &str) -> Option<Session> {
        let path = self.path(id);
        if !path.exists() {
            return None;
        }
        let content = fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }

    pub fn save(&self, session: &Session) -> Result<(), SessionError> {
        let path = self.path(&session.id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(session)?;
        fs::write(path, data)?;
        Ok(())
    }

    pub fn append_message(&self, id: &str, role: &str, content: &str) -> Session {
        let mut session = self.load(id).unwrap_or_else(|| Session {
            id: id.to_string(),
            started_at: Some(Utc::now()),
            messages: vec![],
            tool_calls: vec![],
            plan: None,
            summary: None,
            summary_index: 0,
        });
        session.messages.push(SessionMessage {
            role: role.to_string(),
            content: content.to_string(),
            timestamp: Some(Utc::now()),
        });
        let _ = self.save(&session);
        session
    }

    pub fn list(&self) -> Vec<String> {
        let mut ids: Vec<String> = fs::read_dir(&self.sessions_dir)
            .ok()
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter_map(|e| {
                        e.path()
                            .file_stem()
                            .and_then(|s| s.to_str().map(|s| s.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids
    }

    pub fn record_tool_call(&self, record: ToolCallRecord) -> Result<(), SessionError> {
        let sid = record
            .session_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let mut session = self.load(&sid).unwrap_or_else(|| Session {
            id: sid.clone(),
            started_at: Some(Utc::now()),
            messages: vec![],
            tool_calls: vec![],
            plan: None,
            summary: None,
            summary_index: 0,
        });
        session.tool_calls.push(record);
        self.save(&session)
    }

    pub fn get_latest_id(&self) -> Option<String> {
        self.list().pop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn session_roundtrip() {
        let dir = tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());
        let session = store.create_session();
        store.append_message(&session.id, "user", "hello");

        let loaded = store.load(&session.id).expect("load session");
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].role, "user");
        assert_eq!(loaded.messages[0].content, "hello");
        assert!(loaded.messages[0].timestamp.is_some());

        let raw = fs::read_to_string(store.path(&session.id)).expect("read file");
        let reparsed: Session = serde_json::from_str(&raw).expect("parse");
        assert_eq!(reparsed, loaded);
    }

    #[test]
    fn loads_legacy_session_without_timestamps() {
        let dir = tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path());
        let legacy = r#"{
  "id": "legacy-session",
  "messages": [
    {"role": "user", "content": "hi"}
  ],
  "tool_calls": []
}"#;
        fs::write(store.path("legacy-session"), legacy).expect("write legacy");

        let loaded = store.load("legacy-session").expect("load legacy");
        assert_eq!(loaded.messages.len(), 1);
        assert!(loaded.messages[0].timestamp.is_none());
    }
}
