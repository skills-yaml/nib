//! AGENTS.md discovery and loading.

use std::path::{Path, PathBuf};

const AGENTS_FILENAMES: &[&str] = &[
    "AGENTS.md",
    "CLAUDE.md",
    "CLAUDE.local.md",
    "AGENTS.local.md",
];

pub fn find_agents_md(start_path: &Path) -> Option<PathBuf> {
    let mut current = if start_path.is_file() {
        start_path.parent()?.to_path_buf()
    } else {
        start_path.to_path_buf()
    };

    loop {
        for name in AGENTS_FILENAMES {
            let candidate = current.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if !current.pop() {
            break;
        }
    }

    let home = dirs_home()?.join("AGENTS.md");
    if home.is_file() {
        return Some(home);
    }
    None
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub fn load_agents_md(project_path: &Path) -> String {
    match find_agents_md(project_path) {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(content) => format!("# Loaded from {}\n\n{content}", path.display()),
            Err(e) => format!("# Error loading {}: {e}", path.display()),
        },
        None => {
            "# No AGENTS.md found\n\nConsider creating AGENTS.md in the project root.".to_string()
        }
    }
}

pub fn format_context_for_prompt(project_path: &Path, task: Option<&str>) -> String {
    let mut parts = vec![format!(
        "## Project Agent Guidelines\n{}",
        truncate(&load_agents_md(project_path), 3000)
    )];
    if let Some(t) = task {
        parts.push(format!("## Current Task\n{t}"));
    }
    parts.join("\n\n")
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn loads_project_agents_md() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "# Rules\nBe safe.").unwrap();
        let content = load_agents_md(dir.path());
        assert!(content.contains("Be safe."));
    }
}
