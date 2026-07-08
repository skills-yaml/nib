//! Context assembly for prompts.

use std::path::Path;

pub mod agents;
pub mod compression;

pub use agents::{find_agents_md, format_context_for_prompt, load_agents_md};

pub fn assemble_context(project_path: &Path, task: Option<&str>) -> String {
    let mut ctx = format_context_for_prompt(project_path, task);

    // Inject Memory Store
    let memory_store = crate::session::memory::MemoryStore::new(project_path);
    let mem = memory_store.load();
    if !mem.environment.is_empty() || !mem.user.is_empty() {
        ctx.push_str("\n\n## Long-Term Memory\n");
        if !mem.environment.is_empty() {
            ctx.push_str("### Environment Facts\n");
            for (k, v) in &mem.environment {
                ctx.push_str(&format!("- {}: {}\n", k, v));
            }
        }
        if !mem.user.is_empty() {
            ctx.push_str("### User Preferences\n");
            for (k, v) in &mem.user {
                ctx.push_str(&format!("- {}: {}\n", k, v));
            }
        }
    }

    ctx
}
