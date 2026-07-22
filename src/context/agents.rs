//! AGENTS.md discovery and loading.

use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_AGENTS_FILE_BYTES: u64 = 1_048_576;

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
        Some(path) => match read_bounded_agents_file(&path) {
            Ok(content) => format!("# Loaded from {}\n\n{content}", path.display()),
            Err(e) => format!("# Error loading {}: {e}", path.display()),
        },
        None => {
            "# No AGENTS.md found\n\nConsider creating AGENTS.md in the project root.".to_string()
        }
    }
}

fn read_bounded_agents_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let total_bytes = file.metadata()?.len();
    let mut bytes = Vec::with_capacity(
        usize::try_from(total_bytes.min(MAX_AGENTS_FILE_BYTES)).unwrap_or(64 * 1024),
    );
    file.by_ref()
        .take(MAX_AGENTS_FILE_BYTES)
        .read_to_end(&mut bytes)?;
    let mut content = String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if total_bytes > MAX_AGENTS_FILE_BYTES {
        content.push_str("\n\n...[AGENTS.md bounded at 1048576 bytes]...");
    }
    Ok(content)
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
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max).collect::<String>())
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

    #[test]
    fn prompt_truncation_is_utf8_safe() {
        let content = "é".repeat(4_000);
        let truncated = truncate(&content, 3_000);
        assert!(truncated.ends_with("..."));
        assert_eq!(truncated.chars().count(), 3_003);
    }
}
