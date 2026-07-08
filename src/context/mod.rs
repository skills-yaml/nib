//! Context assembly for prompts.

use std::path::Path;

pub mod agents;

pub use agents::{find_agents_md, format_context_for_prompt, load_agents_md};

pub fn assemble_context(project_path: &Path, task: Option<&str>) -> String {
    format_context_for_prompt(project_path, task)
}
