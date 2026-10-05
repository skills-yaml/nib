//! Repository-native Workspace Docs 7 validation, shared by Task and fixtures.

mod catalog;
mod memory;
mod structure;
mod versions;

use std::fs;
use std::path::{Path, PathBuf};

pub const STATES: [&str; 5] = ["backlog", "development", "test", "blocked", "done"];
pub type Check = Result<(), String>;

pub fn validate(root: &Path, module: &str) -> Check {
    match module {
        "catalog" => catalog::validate(root),
        "memory" => memory::validate(root),
        "versions" => versions::validate(root),
        "structure" => structure::validate(root),
        "coordination" => structure::coordination(root),
        "all" => {
            for name in ["structure", "catalog", "memory", "versions", "coordination"] {
                validate(root, name)?;
            }
            Ok(())
        }
        _ => Err(format!("unknown Workspace gate: {module}")),
    }
}

pub fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))
}

pub fn files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut pending = vec![root.to_path_buf()];
    let mut result = Vec::new();
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).map_err(|error| format!("{}: {error}", dir.display()))? {
            let entry = entry.map_err(|error| error.to_string())?;
            let kind = entry.file_type().map_err(|error| error.to_string())?;
            let path = entry.path();
            if kind.is_symlink() {
                return Err(format!("linked project record: {}", path.display()));
            } else if kind.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "md") {
                result.push(path);
            }
        }
    }
    result.sort();
    Ok(result)
}

pub fn specs(root: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let mut result = Vec::new();
    for state in STATES {
        for path in files(&root.join("workspace/specs").join(state))? {
            if path.file_name().is_some_and(|name| name == "README.md")
                || path.to_string_lossy().ends_with(".plan.md")
            {
                continue;
            }
            result.push((state.to_owned(), path));
        }
    }
    Ok(result)
}

pub fn section<'a>(text: &'a str, heading: &str) -> Result<&'a str, String> {
    let marker = format!("## {heading}\n");
    let mut parts = text.split(&marker);
    parts.next();
    let value = parts.next().ok_or_else(|| format!("missing {heading}"))?;
    if parts.next().is_some() {
        return Err(format!("duplicate {heading}"));
    }
    Ok(value.split("\n## ").next().unwrap_or(value))
}

pub fn field<'a>(text: &'a str, name: &str) -> Result<&'a str, String> {
    let prefix = format!("{name}: ");
    let values: Vec<_> = text
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .collect();
    if values.len() != 1 || values[0].trim().is_empty() {
        return Err(format!("missing, duplicate or empty {name}"));
    }
    Ok(values[0].trim())
}

pub fn cells(line: &str) -> Vec<&str> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(str::trim)
        .collect()
}

pub fn safe_relative(value: &str) -> bool {
    !value.is_empty()
        && !value.contains('\\')
        && !Path::new(value).is_absolute()
        && Path::new(value)
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
}

pub fn portable_path(path: &Path) -> Result<String, String> {
    path.components()
        .map(|part| {
            part.as_os_str()
                .to_str()
                .ok_or_else(|| "invalid path encoding".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|parts| parts.join("/"))
}

pub fn validate_memory_text(text: &str, state: &str) -> Check {
    memory::text(text, state)
}

pub fn validate_blocked_text(text: &str) -> Check {
    catalog::blocked(text)
}

pub fn validate_coordination_text(text: &str) -> Check {
    structure::coordination_text(text)
}
