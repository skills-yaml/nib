use super::{files, read, safe_relative, Check};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Deserialize)]
struct Manifest {
    version: String,
    required_root_files: Vec<String>,
    required_workspace_directories: Vec<String>,
    required_spec_files: Vec<String>,
    required_memory_files: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Integrity {
    schema_version: u32,
    source_revision: String,
    files: BTreeMap<String, String>,
}

pub fn validate(root: &Path) -> Check {
    let package = root.join("workspace/instructions/standards/workspace-docs");
    let manifest: Manifest = serde_yaml::from_str(&read(&package.join("v7.0.0/manifest.yaml"))?)
        .map_err(|error| error.to_string())?;
    if manifest.version != "7.0.0" {
        return Err("wrong standard version".into());
    }
    for name in manifest
        .required_root_files
        .iter()
        .chain(&manifest.required_spec_files)
        .chain(&manifest.required_memory_files)
    {
        let path = root.join(name);
        if !safe_relative(name) || !path.is_file() || path.is_symlink() {
            return Err(format!("missing or linked required file: {name}"));
        }
    }
    for name in manifest.required_workspace_directories {
        let path = root.join(&name);
        if !safe_relative(&name) || !path.is_dir() || path.is_symlink() {
            return Err(format!("missing or linked required directory: {name}"));
        }
    }
    for alias in ["default", "latest"] {
        if !valid_alias(&package.join(alias)) {
            return Err(format!("unsafe standard alias: {alias}"));
        }
    }
    let agents = read(&root.join("AGENTS.md"))?;
    let template = read(&package.join("v7.0.0/agents-template.md"))?;
    if block(&agents)? != block(&template)? {
        return Err("generated context drift".into());
    }
    for legacy in ["docs/tech", "docs/specs", "agents/memory", "docs/standards"] {
        if root.join(legacy).exists() {
            return Err(format!("duplicate active location: {legacy}"));
        }
    }
    integrity(&package)
}

fn valid_alias(path: &Path) -> bool {
    if let Ok(target) = std::fs::read_link(path) {
        return target == Path::new("v7.0.0");
    }
    // Git on Windows can check mode-120000 aliases out as regular pointer files.
    // The committed representation remains a contained relative symlink.
    cfg!(windows) && !path.is_symlink() && read(path).is_ok_and(|value| value == "v7.0.0")
}

fn block(text: &str) -> Result<&str, String> {
    let start = "<!-- AGENT-CONTEXT:START workspace-docs@7.0.0 -->";
    let end = "<!-- AGENT-CONTEXT:END -->";
    if text.matches("<!-- AGENT-CONTEXT:START").count() != 1 || text.matches(end).count() != 1 {
        return Err("generated context requires one balanced marker pair".into());
    }
    let begin = text.find(start).ok_or("wrong generated context pin")?;
    let finish = text.find(end).ok_or("missing context end")? + end.len();
    text.get(begin..finish)
        .ok_or_else(|| "unbalanced generated context".into())
}

fn integrity(package: &Path) -> Check {
    let manifest: Integrity = serde_json::from_str(&read(&package.join("integrity.json"))?)
        .map_err(|error| error.to_string())?;
    if manifest.schema_version != 1
        || manifest.source_revision.len() != 40
        || manifest.files.is_empty()
    {
        return Err("invalid standard integrity manifest".into());
    }
    for (name, expected) in manifest.files {
        let path = package.join(&name);
        if !safe_relative(&name) || path.is_symlink() {
            return Err("unsafe package path".into());
        }
        let data = std::fs::read(&path).map_err(|error| error.to_string())?;
        let digest = format!("{:x}", Sha256::digest(data));
        if digest != expected {
            return Err(format!("modified source package: {name}"));
        }
    }
    Ok(())
}

fn record_metadata(text: &str) -> Result<serde_yaml::Value, String> {
    let body = text
        .strip_prefix("---\n")
        .ok_or("missing WIP frontmatter")?
        .split_once("\n---\n")
        .ok_or("unclosed WIP frontmatter")?
        .0;
    serde_yaml::from_str(body).map_err(|error| error.to_string())
}

fn metadata_text<'a>(metadata: &'a serde_yaml::Value, key: &str) -> Result<&'a str, String> {
    metadata
        .get(key)
        .and_then(serde_yaml::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("missing WIP {key}"))
}

fn common_metadata(metadata: &serde_yaml::Value) -> Check {
    if metadata
        .get("schema_version")
        .and_then(serde_yaml::Value::as_u64)
        != Some(1)
    {
        return Err("invalid WIP schema".into());
    }
    let base = metadata_text(metadata, "base_revision")?;
    if base.len() != 40 || !base.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("invalid WIP base revision".into());
    }
    if !matches!(
        metadata_text(metadata, "status")?,
        "active" | "handoff" | "review" | "complete" | "blocked" | "interrupted"
    ) {
        return Err("invalid WIP status".into());
    }
    metadata_text(metadata, "coordination_id")?;
    metadata_text(metadata, "updated_at")?;
    Ok(())
}

pub fn coordination_text(text: &str) -> Check {
    let metadata = record_metadata(text)?;
    common_metadata(&metadata)?;
    metadata_text(&metadata, "agent_id")?;
    metadata_text(&metadata, "role")?;
    metadata_text(&metadata, "task_ref")?;
    metadata_text(&metadata, "branch_authorization")?;
    let scopes = metadata
        .get("scope")
        .and_then(serde_yaml::Value::as_sequence)
        .ok_or("missing WIP scope")?;
    if scopes.is_empty()
        || scopes.len() > 64
        || scopes
            .iter()
            .any(|value| !value.as_str().is_some_and(safe_relative))
    {
        return Err("unsafe WIP scope".into());
    }
    for heading in [
        "Assignment",
        "Actions",
        "Validation",
        "Blockers and Dependencies",
        "Next Step",
        "Handoff",
    ] {
        if super::section(text, heading)?.trim().is_empty() {
            return Err(format!("empty WIP {heading}"));
        }
    }
    Ok(())
}

fn task_index(text: &str) -> Check {
    common_metadata(&record_metadata(text)?)?;
    for heading in ["Assignments", "Dependencies", "Integration"] {
        if super::section(text, heading)?.trim().is_empty() {
            return Err(format!("empty task index {heading}"));
        }
    }
    Ok(())
}

pub fn coordination(root: &Path) -> Check {
    for path in files(&root.join("workspace/docs/work/multi-agent"))? {
        if path.file_name().is_some_and(|name| name == "README.md") {
            if path.parent() != Some(root.join("workspace/docs/work/multi-agent").as_path()) {
                task_index(&read(&path)?)?;
            }
            continue;
        }
        super::validate_coordination_text(&read(&path)?)?;
    }
    Ok(())
}
