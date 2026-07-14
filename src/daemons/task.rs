//! Background task and timer manager.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;

/// Represents a running background task or timer.
pub struct BackgroundTask {
    pub id: String,
    pub status: String,
}

pub struct TaskManager {
    tasks: Arc<Mutex<HashMap<String, BackgroundTask>>>,
}

impl TaskManager {
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Spawns a background timer that will send a message to a channel when done.
    pub fn spawn_timer(&self, id: String, duration_secs: u64, prompt: String, tx: mpsc::Sender<Value>) {
        let tasks = self.tasks.clone();
        
        tasks.lock().unwrap().insert(id.clone(), BackgroundTask {
            id: id.clone(),
            status: "running".to_string(),
        });

        tokio::spawn(async move {
            sleep(Duration::from_secs(duration_secs)).await;
            
            if let Ok(mut map) = tasks.lock() {
                if let Some(task) = map.get_mut(&id) {
                    task.status = "completed".to_string();
                }
            }
            
            let _ = tx.send(serde_json::json!({
                "type": "timer_fired",
                "id": id,
                "prompt": prompt,
            })).await;
        });
    }

    pub fn get_status(&self, id: &str) -> Option<String> {
        self.tasks.lock().unwrap().get(id).map(|t| t.status.clone())
    }
}
