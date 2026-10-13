//! Session presence (T080 phase 5a, section 6).
//!
//! Several sessions may work in one folder. Each live session keeps a small
//! heartbeat file in profile state so the others can show "N other sessions
//! in this folder". Presence is informational and never blocks anything. A
//! session that stops beating, for example after a crash, is ignored once
//! its heartbeat is [`PRESENCE_TIMEOUT`] old.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A heartbeat older than this no longer counts.
pub const PRESENCE_TIMEOUT: Duration = Duration::from_secs(120);
/// How often a live session refreshes its heartbeat.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// How often the status line re-counts the other sessions.
const SCAN_INTERVAL: Duration = Duration::from_secs(5);
/// Heartbeat files this old are removed by any session that scans.
const STALE_FILE_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// At most this many heartbeat files are read per scan.
const MAX_SCANNED: usize = 512;
const PRESENCE_DIR: &str = "presence";

#[derive(Debug, Serialize, Deserialize)]
struct Heartbeat {
    session_id: String,
    folder: PathBuf,
    updated_at: u64,
}

/// One session's presence in a folder, refreshed from the caller's loop.
#[derive(Debug)]
pub struct Presence {
    dir: PathBuf,
    folder: PathBuf,
    session_id: Option<String>,
    last_beat: Option<Instant>,
    last_scan: Option<Instant>,
    others: usize,
}

impl Presence {
    /// Presence for sessions stored in `sessions_dir`, working in `folder`.
    pub fn new(sessions_dir: &Path, folder: &Path) -> Self {
        let dir = sessions_dir
            .parent()
            .unwrap_or(sessions_dir)
            .join(PRESENCE_DIR);
        Self {
            dir,
            folder: folder
                .canonicalize()
                .unwrap_or_else(|_| folder.to_path_buf()),
            session_id: None,
            last_beat: None,
            last_scan: None,
            others: 0,
        }
    }

    /// Beats for `session_id` when due (or when the active session changed)
    /// and returns how many other live sessions share the folder.
    pub fn tick(&mut self, session_id: &str, now: Instant) -> usize {
        if !valid_session_id(session_id) {
            return self.others;
        }
        if self.session_id.as_deref() != Some(session_id) {
            self.leave();
            self.session_id = Some(session_id.to_string());
            self.last_scan = None;
        }
        if self
            .last_beat
            .is_none_or(|last| now.duration_since(last) >= HEARTBEAT_INTERVAL)
        {
            if let Err(error) = self.beat(session_id) {
                tracing::debug!("presence heartbeat not written: {error}");
            }
            self.last_beat = Some(now);
        }
        if self
            .last_scan
            .is_none_or(|last| now.duration_since(last) >= SCAN_INTERVAL)
        {
            self.others = self.others(session_id, SystemTime::now()).len();
            self.last_scan = Some(now);
        }
        self.others
    }

    /// Removes this session's heartbeat.
    pub fn leave(&mut self) {
        if let Some(session_id) = self.session_id.take() {
            let _ = std::fs::remove_file(self.file(&session_id));
        }
        self.last_beat = None;
    }

    fn file(&self, session_id: &str) -> PathBuf {
        self.dir.join(format!("{session_id}.json"))
    }

    fn beat(&self, session_id: &str) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let heartbeat = Heartbeat {
            session_id: session_id.to_string(),
            folder: self.folder.clone(),
            updated_at: unix_seconds(SystemTime::now()),
        };
        let bytes = serde_json::to_vec(&heartbeat).map_err(io::Error::other)?;
        let mut file = tempfile::NamedTempFile::new_in(&self.dir)?;
        io::Write::write_all(&mut file, &bytes)?;
        file.persist(self.file(session_id))
            .map_err(|error| error.error)?;
        Ok(())
    }

    /// The ids of the other live sessions in this folder, sorted.
    fn others(&self, session_id: &str, now: SystemTime) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let now = unix_seconds(now);
        let mut others = Vec::new();
        for entry in entries.flatten().take(MAX_SCANNED) {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Some(heartbeat) = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Heartbeat>(&bytes).ok())
            else {
                continue;
            };
            let age = now.saturating_sub(heartbeat.updated_at);
            if age >= STALE_FILE_AGE.as_secs() {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            if heartbeat.session_id != session_id
                && heartbeat.folder == self.folder
                && age < PRESENCE_TIMEOUT.as_secs()
            {
                others.push(heartbeat.session_id);
            }
        }
        others.sort();
        others
    }
}

/// The other live sessions working in `folder`, for run records (T080
/// section 6b). Writes no heartbeat; like any scan, it removes day-old
/// heartbeat files.
pub fn other_sessions_in(sessions_dir: &Path, folder: &Path, session_id: &str) -> Vec<String> {
    // A fresh `Presence` owns no heartbeat, so dropping it removes nothing.
    Presence::new(sessions_dir, folder).others(session_id, SystemTime::now())
}

impl Drop for Presence {
    fn drop(&mut self) {
        self.leave();
    }
}

/// Keeps a session present from a background thread, for runs without an
/// interactive loop (`nib run`). Dropping it stops the heartbeat and removes
/// the session's file.
pub struct PresenceHeartbeat {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PresenceHeartbeat {
    pub fn start(sessions_dir: &Path, folder: &Path, session_id: &str) -> Self {
        let mut presence = Presence::new(sessions_dir, folder);
        let session_id = session_id.to_string();
        let (stop, stopped) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("nib-presence".to_string())
            .spawn(move || loop {
                presence.tick(&session_id, Instant::now());
                match stopped.recv_timeout(HEARTBEAT_INTERVAL) {
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
            })
            .ok();
        Self {
            stop: Some(stop),
            thread,
        }
    }
}

impl Drop for PresenceHeartbeat {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A short status-line note, or `None` when the session is alone.
pub fn other_sessions_label(others: usize) -> Option<String> {
    match others {
        0 => None,
        1 => Some("1 other session in this folder".to_string()),
        count => Some(format!("{count} other sessions in this folder")),
    }
}

fn valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 128
        && session_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "presence_tests.rs"]
mod tests;
