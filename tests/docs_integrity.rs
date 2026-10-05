use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

fn markdown_files(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();

    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir).expect("read documentation directory") {
            let entry = entry.expect("read documentation entry");
            let path = entry.path();
            let kind = entry.file_type().expect("documentation entry type");
            if kind.is_symlink()
                || entry.file_name().to_string_lossy().starts_with('.')
                || entry.file_name() == "target"
                || entry.file_name() == "node_modules"
                || path.ends_with("workspace/instructions/standards/workspace-docs")
            {
                continue;
            }
            if kind.is_dir() {
                pending.push(path);
            } else if path.extension().and_then(|value| value.to_str()) == Some("md") {
                files.push(path);
            }
        }
    }

    files.sort();
    files
}

fn local_target(source: &Path, raw: &str) -> Option<PathBuf> {
    let raw = raw.trim().trim_start_matches('<').trim_end_matches('>');
    let raw = raw.split('#').next().unwrap_or_default();
    if raw.is_empty() || raw.starts_with('#') || raw.starts_with("mailto:") || raw.contains("://") {
        return None;
    }

    let target = PathBuf::from(raw.replace("%20", " "));
    if target.is_absolute() {
        Some(target)
    } else {
        Some(source.parent().expect("markdown parent").join(target))
    }
}

#[test]
fn internal_markdown_links_resolve() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let link_re = Regex::new(r#"\[[^\]]*\]\(([^)]+)\)"#).expect("valid link regex");
    let mut broken = Vec::new();

    let mut sources = markdown_files(&root);
    sources.push(root.join("workspace/instructions/standards/workspace-docs/ADOPTION.md"));
    for source in sources {
        let content = fs::read_to_string(&source).expect("read markdown");
        for captures in link_re.captures_iter(&content) {
            let raw = captures.get(1).expect("link target").as_str();
            if let Some(target) = local_target(&source, raw) {
                if target.is_absolute() && !target.starts_with(&root) {
                    continue;
                }
                if !target.exists() {
                    broken.push(format!(
                        "{} -> {}",
                        source.strip_prefix(&root).unwrap_or(&source).display(),
                        raw
                    ));
                }
            }
        }
    }

    assert!(
        broken.is_empty(),
        "broken internal Markdown links:\n{}",
        broken.join("\n")
    );
}

#[test]
fn spec_ids_are_unique_across_states() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("workspace/specs");
    let heading_re = Regex::new(r"(?m)^#\s+((?:FT|T|D)[-_]?\d+):").expect("valid ID regex");
    let mut ids: HashMap<String, PathBuf> = HashMap::new();
    let mut duplicates = Vec::new();

    for state in ["backlog", "development", "test", "blocked", "done"] {
        for path in markdown_files(&root.join(state)) {
            let content = fs::read_to_string(&path).expect("read spec");
            let Some(captures) = heading_re.captures(&content) else {
                continue;
            };
            let id = captures[1].replace('_', "-");
            if let Some(previous) = ids.insert(id.clone(), path.clone()) {
                duplicates.push(format!(
                    "{id}: {} and {}",
                    previous.display(),
                    path.display()
                ));
            }
        }
    }

    assert!(
        duplicates.is_empty(),
        "duplicate spec IDs:\n{}",
        duplicates.join("\n")
    );
}

#[test]
fn done_specs_do_not_claim_open_acceptance_items() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("workspace/specs/done");
    let mut offenders = Vec::new();

    for path in markdown_files(&root) {
        let content = fs::read_to_string(&path).expect("read done spec");
        if content.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with("- [ ]") || line.starts_with("* [ ]")
        }) {
            offenders.push(path.display().to_string());
        }
    }

    assert!(
        offenders.is_empty(),
        "done specs contain unchecked acceptance items:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn development_specs_have_required_execution_fields() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("workspace/specs/development");
    let required = [
        "scope",
        "acceptance criteria",
        "affected areas",
        "validation gates",
    ];
    let mut offenders = Vec::new();

    for path in markdown_files(&root)
        .into_iter()
        .filter(|path| path.file_name().is_none_or(|name| name != "README.md"))
    {
        let content = fs::read_to_string(&path).expect("read development spec");
        let normalized = content.to_ascii_lowercase();
        let mut missing = required
            .iter()
            .filter(|field| !normalized.contains(**field))
            .copied()
            .collect::<Vec<_>>();
        let headings = content
            .lines()
            .filter_map(|line| {
                let line = line.trim_start();
                line.strip_prefix('#')
                    .map(|heading| heading.trim_start_matches('#').trim().to_ascii_lowercase())
            })
            .collect::<Vec<_>>();
        if !headings.iter().any(|heading| heading.contains("risk")) {
            missing.push("risks");
        }
        if !headings.iter().any(|heading| {
            heading.contains("implementation plan") || heading.contains("rollout plan")
        }) {
            missing.push("implementation or rollout plan");
        }
        if !missing.is_empty() {
            offenders.push(format!("{}: {}", path.display(), missing.join(", ")));
        }
    }

    assert!(
        offenders.is_empty(),
        "development specs are missing required fields:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn explicit_spec_status_matches_state_directory() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("workspace/specs");
    let status_re = Regex::new(r"(?im)^\*\*status:\*\*\s*([^\r\n]+)").expect("valid status regex");
    let mut offenders = Vec::new();

    for (state, forbidden) in [
        (
            "backlog",
            &["done", "in progress", "development"] as &[&str],
        ),
        ("development", &["done"] as &[&str]),
    ] {
        for path in markdown_files(&root.join(state)) {
            let content = fs::read_to_string(&path).expect("read state spec");
            if let Some(status) = status_re
                .captures(&content)
                .and_then(|captures| captures.get(1))
                .map(|value| value.as_str().trim().to_ascii_lowercase())
            {
                if forbidden.iter().any(|value| status.starts_with(value)) {
                    offenders.push(format!("{} declares {status}", path.display()));
                }
            }
        }
    }
    for path in markdown_files(&root.join("done"))
        .into_iter()
        .filter(|path| path.file_name().is_none_or(|name| name != "README.md"))
    {
        let content = fs::read_to_string(&path).expect("read done spec");
        let status = status_re
            .captures(&content)
            .and_then(|captures| captures.get(1))
            .map(|value| value.as_str().trim().to_ascii_lowercase());
        if !status.is_some_and(|status| status.starts_with("done")) {
            offenders.push(format!("{} does not declare Done", path.display()));
        }
    }

    assert!(
        offenders.is_empty(),
        "spec status does not match directory:\n{}",
        offenders.join("\n")
    );
}
