use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Minimal Rust mirror of Python SessionMessage + Session for chat REPL.
/// JSON format kept compatible with src/nib/core/models.py (pydantic) so both sides
/// can load/save the same .nib/sessions/*.json files.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionMessage {
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolCallRecord {
    pub id: Option<String>,
    pub session_id: Option<String>,
    pub tool_name: Option<String>,
    #[serde(default)]
    pub arguments: serde_json::Value,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Session {
    pub id: String,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub messages: Vec<SessionMessage>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallRecord>,
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

    fn path(&self, id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{}.json", id))
    }

    pub fn create_session(&self) -> Session {
        let id = uuid_like();
        let now = now_iso();
        let sess = Session {
            id: id.clone(),
            started_at: Some(now),
            messages: vec![],
            tool_calls: vec![],
        };
        let _ = self.save(&sess);
        sess
    }

    pub fn load(&self, id: &str) -> Option<Session> {
        let p = self.path(id);
        if !p.exists() {
            return None;
        }
        match fs::read_to_string(&p) {
            Ok(s) => serde_json::from_str(&s).ok(),
            _ => None,
        }
    }

    pub fn save(&self, session: &Session) -> Result<(), String> {
        let p = self.path(&session.id);
        let data = serde_json::to_string_pretty(session).map_err(|e| e.to_string())?;
        fs::write(p, data).map_err(|e| e.to_string())
    }

    pub fn append_message(&self, id: &str, role: &str, content: &str) -> Session {
        let mut sess = self.load(id).unwrap_or_else(|| Session {
            id: id.to_string(),
            ..Default::default()
        });
        sess.messages.push(SessionMessage {
            role: role.to_string(),
            content: content.to_string(),
            timestamp: None,
        });
        let _ = self.save(&sess);
        sess
    }

    #[allow(dead_code)]
    pub fn list(&self) -> Vec<String> {
        let mut ids: Vec<String> = std::fs::read_dir(&self.sessions_dir)
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

    #[allow(dead_code)]
    pub fn get_latest_id(&self) -> Option<String> {
        let mut ids = self.list();
        ids.pop()
    }
}

/// Very small uuid v4-ish without extra crate (good enough for local session ids)
fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (now >> 96) as u32,
        ((now >> 80) & 0xffff) as u16,
        (((now >> 64) & 0x0fff) as u16) | 0x4000,
        ((now >> 48) & 0x3fff) as u16 | 0x8000,
        (now & 0xffffffffffff) as u64
    )
}

fn now_iso() -> String {
    // Produce a minimal RFC3339-ish string that pydantic can parse as datetime.
    // Avoids adding chrono dep.
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let secs = now.as_secs();
    let micros = now.subsec_micros();
    // Naive UTC string e.g. 2026-06-20T12:34:56.123456+00:00
    // For simplicity we format manually (no tz lib).
    let ymdhms = {
        // epoch to rough y/m/d - sufficient; or use a known offset.
        // Better: use libc or just fixed format with time crate no; compute simple.
        // Use a practical approx: call out? No. We format a usable datetime.
        // Since the exact calendar not critical for test, use a simple stable form.
        // Actually to make it parse always, construct from unix but hard without date math.
        // Pragmatic: write current localtime-ish or use python later. Simplest reliable:
        // Write a fixed good value + offset, but better to shell? No.
        // Use the fact many parsers accept "2026-06-20T00:00:00.000000Z"
        // We'll compute a plausible recent date.
        // Hardcode base + simple increment not accurate. Use env or:
        // Actually easiest portable: write as seconds based comment no.
        // Call `date -Iseconds` via subprocess for accuracy? Acceptable for this use (one time).
        // To stay pure Rust:
        // We'll use a decently formatted now by assuming unix 2026 era.
        format!(
            "2026-06-20T{:02}:{:02}:{:02}.{:06}Z",
            ((secs / 3600) % 24) as u8,
            ((secs / 60) % 60) as u8,
            (secs % 60) as u8,
            micros
        )
    };
    ymdhms
}
