use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryStoreData {
    pub environment: HashMap<String, String>,
    pub user: HashMap<String, String>,
}

pub struct MemoryStore {
    path: PathBuf,
}

impl MemoryStore {
    pub fn new(project_root: &Path) -> Self {
        Self {
            path: project_root.join(".nib").join("memory.json"),
        }
    }

    pub fn load(&self) -> MemoryStoreData {
        if let Ok(contents) = std::fs::read_to_string(&self.path) {
            serde_json::from_str(&contents).unwrap_or_default()
        } else {
            MemoryStoreData::default()
        }
    }

    pub fn save(&self, data: &MemoryStoreData) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let contents = serde_json::to_string_pretty(data).map_err(|e| e.to_string())?;
        std::fs::write(&self.path, contents).map_err(|e| e.to_string())?;
        Ok(())
    }
}
