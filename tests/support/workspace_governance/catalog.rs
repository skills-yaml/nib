use super::{cells, field, read, safe_relative, section, specs, Check};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub fn blocked(text: &str) -> Check {
    if !matches!(
        field(text, "Previous State")?,
        "backlog" | "development" | "test"
    ) {
        return Err("invalid blocked previous state".into());
    }
    if !matches!(field(text, "Block Kind")?, "impediment" | "deferred") {
        return Err("invalid block kind".into());
    }
    field(text, "Block Reason")?;
    field(text, "Resume Condition")?;
    Ok(())
}

pub fn validate(root: &Path) -> Check {
    let base = root.join("workspace/specs");
    let index = read(&base.join("README.md"))?;
    let feature_section = section(&index, "Primary Features")?;
    let features: HashSet<_> = feature_section
        .lines()
        .filter_map(|line| line.strip_prefix("- `")?.strip_suffix('`'))
        .collect();
    let feature_re = Regex::new(r"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$").map_err(|e| e.to_string())?;
    if features.is_empty() || features.iter().any(|feature| !feature_re.is_match(feature)) {
        return Err("invalid primary feature definitions".into());
    }
    let start = "<!-- SPEC-CATALOG:START -->";
    let end = "<!-- SPEC-CATALOG:END -->";
    if index.matches(start).count() != 1 || index.matches(end).count() != 1 {
        return Err("catalog requires one balanced marker pair".into());
    }
    let body = index
        .split_once(start)
        .ok_or("missing catalog")?
        .1
        .split_once(end)
        .ok_or("unclosed catalog")?
        .0;
    let link = Regex::new(r"^\[([^]]+)\]\(([^)]+)\)$").map_err(|e| e.to_string())?;
    let mut rows = HashMap::new();
    for line in body.lines().filter(|line| line.starts_with("| [")) {
        let values = cells(line);
        if values.len() != 4 || values[3].is_empty() {
            return Err("catalog row requires link, feature, state and rationale".into());
        }
        let captures = link.captures(values[0]).ok_or("invalid catalog link")?;
        let path = captures[2].to_owned();
        if !safe_relative(&path) || rows.insert(path, values).is_some() {
            return Err("unsafe or duplicate catalog path".into());
        }
    }
    let id_re = Regex::new(r"(?mi)^#\s+((?:FT|T|D)[-_]?\d+):").map_err(|e| e.to_string())?;
    let mut ids = HashSet::new();
    for (state, path) in specs(root)? {
        let relative = path.strip_prefix(&base).map_err(|e| e.to_string())?;
        if relative.components().count() != 3 {
            return Err(format!("spec needs one feature: {}", path.display()));
        }
        let feature = relative
            .components()
            .nth(1)
            .ok_or("missing feature")?
            .as_os_str()
            .to_str()
            .ok_or("invalid feature encoding")?;
        let text = read(&path)?;
        let row = rows
            .remove(&super::portable_path(relative)?)
            .ok_or_else(|| format!("missing catalog row: {}", path.display()))?;
        let id = id_re.captures(&text).ok_or("missing spec ID")?[1]
            .to_uppercase()
            .replace('_', "-");
        if !ids.insert(id.clone()) || !row[0].starts_with(&format!("[{id}]")) {
            return Err(format!("duplicate or mismatched spec ID: {id}"));
        }
        if !features.contains(feature)
            || row[1] != feature
            || row[2] != state
            || field(&text, "State")? != state
            || field(&text, "Primary Feature")? != feature
        {
            return Err(format!(
                "spec/catalog metadata mismatch: {}",
                path.display()
            ));
        }
        if state == "blocked" {
            super::validate_blocked_text(&text)?;
        }
        for suffix in [
            "scope",
            "acceptance criteria",
            "affected areas",
            "validation gates",
        ] {
            if state == "development" && !text.to_lowercase().contains(suffix) {
                return Err(format!(
                    "development spec missing {suffix}: {}",
                    path.display()
                ));
            }
        }
        if state == "test" {
            evidence(&text, "Integration Evidence")?;
        }
    }
    if !rows.is_empty() {
        return Err("catalog contains orphan rows".into());
    }
    validate_plans(root)
}

fn evidence(text: &str, heading: &str) -> Check {
    let evidence = section(text, heading)?;
    let revision = field(evidence, "Revision")?;
    if revision.len() != 40
        || !revision.chars().all(|c| c.is_ascii_hexdigit())
        || field(evidence, "Outcome")? != "passed"
    {
        return Err(format!("invalid {heading}"));
    }
    Ok(())
}

fn validate_plans(root: &Path) -> Check {
    for state in super::STATES {
        for path in super::files(&root.join("workspace/specs").join(state))? {
            let name = path.to_string_lossy();
            if let Some(stem) = name.strip_suffix(".plan.md") {
                if !Path::new(&format!("{stem}.md")).is_file() {
                    return Err(format!("orphan plan: {}", path.display()));
                }
            }
        }
    }
    Ok(())
}
