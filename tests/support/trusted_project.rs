//! Marks fixture projects trusted for nib's permission rules (T080 phase 3).
//!
//! Allow lines in a project's AGENTS.md apply only after the user trusts the
//! project. The trust flag lives in `$NIB_HOME/permissions.json`. `task test`
//! sets `NIB_HOME`; otherwise the test binary points it at one temporary
//! directory, and nib processes started by the tests inherit it.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

fn nib_home() -> &'static PathBuf {
    static HOME: OnceLock<(PathBuf, Option<tempfile::TempDir>)> = OnceLock::new();
    &HOME
        .get_or_init(|| match std::env::var_os("NIB_HOME") {
            Some(path) if !path.is_empty() => (PathBuf::from(path), None),
            _ => {
                let directory = tempfile::tempdir().expect("nib home");
                std::env::set_var("NIB_HOME", directory.path());
                (directory.path().to_path_buf(), Some(directory))
            }
        })
        .0
}

pub fn trust_project(root: &Path) {
    static WRITE: Mutex<()> = Mutex::new(());
    let home = nib_home();
    let _write = WRITE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::fs::create_dir_all(home).expect("nib home");
    let path = home.join("permissions.json");
    let mut file = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .unwrap_or_else(|| serde_json::json!({"version": 1, "projects": {}}));
    let key = root
        .canonicalize()
        .expect("fixture root")
        .display()
        .to_string();
    file["projects"][key] = serde_json::json!({"trusted": true});
    std::fs::write(&path, serde_json::to_vec_pretty(&file).expect("encode"))
        .expect("trust fixture project");
}
