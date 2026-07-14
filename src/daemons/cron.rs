use crate::daemons::curator::Curator;
use std::path::Path;

pub struct Cron {
    // Basic wrapper for daemon jobs
}

impl Cron {
    pub fn run_maintenance(project_root: &Path, retention_days: i64) {
        let curator = Curator::new(project_root, retention_days);
        if let Ok(count) = curator.cleanup_old_sessions() {
            if count > 0 {
                println!("Curator: Cleaned up {} old sessions.", count);
            }
        }
    }
}
