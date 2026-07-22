use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use thiserror::Error;

const MAX_SKILL_FILE_BYTES: u64 = 131_072;
const MAX_SKILL_REFERENCES: usize = 32;
const MAX_SKILL_ASSETS: usize = 64;
const MAX_REFERENCE_BYTES: u64 = 32_768;
const MAX_TOTAL_REFERENCE_BYTES: u64 = 65_536;
const MAX_ASSET_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TOTAL_ASSET_BYTES: u64 = 8 * 1024 * 1024;
const MAX_RESOURCE_PATH_BYTES: usize = 1_024;
const MAX_RESOURCE_DEPTH: usize = 8;
const MAX_DISCOVERED_SKILLS: usize = 256;
const MAX_DISCOVERY_ENTRIES: usize = 4_096;
const MAX_SKILL_DISCOVERY_DEPTH: usize = 4;
const MAX_SKILL_ID_BYTES: usize = 128;

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct SkillConstraints {
    #[serde(default)]
    pub deny_tools: Vec<String>,
    #[serde(default)]
    pub deny_commands: Vec<String>,
    #[serde(default)]
    pub require_approval_tools: Vec<String>,
    #[serde(default)]
    pub require_approval_commands: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct SkillHook {
    pub tool: String,
    pub command: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct SkillHooks {
    #[serde(default)]
    pub after_tool: Vec<SkillHook>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SkillFrontmatter {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub references: Vec<String>,
    #[serde(default)]
    pub assets: Vec<String>,
    #[serde(default)]
    pub constraints: SkillConstraints,
    #[serde(default)]
    pub hooks: SkillHooks,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub path: PathBuf,
    pub frontmatter: SkillFrontmatter,
    pub body: String,
    pub references: Vec<SkillReference>,
    pub assets: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillReference {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillPolicyEffect {
    Deny,
    RequireApproval,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPolicyRule {
    pub skill_name: String,
    pub tool_name: Option<String>,
    pub argument_contains: Option<String>,
    pub effect: SkillPolicyEffect,
}

#[derive(Debug, Error)]
pub enum SkillError {
    #[error("failed to read skill: {0}")]
    Io(#[from] std::io::Error),
    #[error("SKILL.md must start with YAML frontmatter")]
    MissingFrontmatter,
    #[error("SKILL.md frontmatter is not terminated")]
    UnterminatedFrontmatter,
    #[error("invalid SKILL.md frontmatter: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("skill name cannot be empty")]
    EmptyName,
    #[error("skill name must be 1-128 characters and contain an ASCII letter or digit")]
    InvalidName,
    #[error("invalid skill resource path: {0}")]
    InvalidResource(String),
    #[error("skill reference exceeds the 32768-byte limit: {0}")]
    ReferenceTooLarge(String),
    #[error("skill manifest exceeds the 131072-byte limit: {0}")]
    ManifestTooLarge(String),
    #[error("skill declares too many {kind}: maximum {maximum}, got {actual}")]
    TooManyResources {
        kind: &'static str,
        maximum: usize,
        actual: usize,
    },
    #[error("skill references exceed the 65536-byte aggregate limit")]
    ReferencesTooLarge,
    #[error("skill asset exceeds the 2097152-byte limit: {0}")]
    AssetTooLarge(String),
    #[error("skill assets exceed the 8388608-byte aggregate limit")]
    AssetsTooLarge,
}

#[derive(Debug, Error)]
pub enum SkillDiscoveryError {
    #[error("failed to inspect skill discovery path {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("skill discovery root is not a directory: {path:?}")]
    NotDirectory { path: PathBuf },
    #[error("skill manifest is not a regular file: {path:?}")]
    InvalidManifestType { path: PathBuf },
    #[error("skill discovery was truncated at the {kind} limit ({limit})")]
    Truncated { kind: &'static str, limit: usize },
}

#[derive(Clone, Copy)]
struct SkillDiscoveryLimits {
    max_skills: usize,
    max_entries: usize,
    max_depth: usize,
}

const SKILL_DISCOVERY_LIMITS: SkillDiscoveryLimits = SkillDiscoveryLimits {
    max_skills: MAX_DISCOVERED_SKILLS,
    max_entries: MAX_DISCOVERY_ENTRIES,
    max_depth: MAX_SKILL_DISCOVERY_DEPTH,
};

fn default_skill_roots(project_path: &Path) -> Vec<PathBuf> {
    let mut roots = vec![
        project_path.join(".nib").join("skills"),
        project_path.join(".skills"),
        project_path.join("skills"),
    ];

    let configured_global = std::env::var_os("NIB_SKILLS_DIR").map(PathBuf::from);
    if let Some(path) = configured_global.as_ref() {
        roots.push(path.clone());
    }

    if let Ok(home) = std::env::var("HOME") {
        let home = PathBuf::from(home);
        if configured_global.is_none() {
            roots.push(home.join(".config").join("nib").join("skills"));
        }
        roots.extend([
            home.join(".grok").join("skills"),
            home.join(".agents").join("skills"),
            home.join("work")
                .join("projects")
                .join("registry")
                .join("skills"),
        ]);
    }

    roots
}

fn discover_skill_files(
    root: &Path,
    depth: usize,
    files: &mut Vec<PathBuf>,
    entries_scanned: &mut usize,
) {
    if depth > MAX_SKILL_DISCOVERY_DEPTH
        || is_skill_install_staging_directory(root)
        || !root.is_dir()
        || files.len() >= MAX_DISCOVERED_SKILLS
        || *entries_scanned >= MAX_DISCOVERY_ENTRIES
    {
        return;
    }

    let direct = root.join("SKILL.md");
    if direct.is_file() {
        files.push(direct);
        return;
    }

    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if files.len() >= MAX_DISCOVERED_SKILLS || *entries_scanned >= MAX_DISCOVERY_ENTRIES {
            break;
        }
        *entries_scanned += 1;
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            discover_skill_files(&path, depth + 1, files, entries_scanned);
        }
    }
}

fn strict_discovery_io(path: &Path, source: io::Error) -> SkillDiscoveryError {
    SkillDiscoveryError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn record_discovered_skill(
    manifest: PathBuf,
    limits: SkillDiscoveryLimits,
    files: &mut Vec<PathBuf>,
    seen: &mut HashSet<PathBuf>,
) -> Result<(), SkillDiscoveryError> {
    let identity = manifest
        .canonicalize()
        .map_err(|error| strict_discovery_io(&manifest, error))?;
    if !seen.insert(identity) {
        return Ok(());
    }
    if files.len() >= limits.max_skills {
        return Err(SkillDiscoveryError::Truncated {
            kind: "skill count",
            limit: limits.max_skills,
        });
    }
    files.push(manifest);
    Ok(())
}

fn discover_skill_files_strict(
    root: &Path,
    depth: usize,
    limits: SkillDiscoveryLimits,
    files: &mut Vec<PathBuf>,
    seen: &mut HashSet<PathBuf>,
    entries_scanned: &mut usize,
) -> Result<(), SkillDiscoveryError> {
    if is_skill_install_staging_directory(root) {
        return Ok(());
    }
    if depth > limits.max_depth {
        return Err(SkillDiscoveryError::Truncated {
            kind: "directory depth",
            limit: limits.max_depth,
        });
    }

    let direct = root.join("SKILL.md");
    match fs::symlink_metadata(&direct) {
        Ok(metadata) if metadata.file_type().is_file() => {
            return record_discovered_skill(direct, limits, files, seen);
        }
        Ok(_) => {
            return Err(SkillDiscoveryError::InvalidManifestType { path: direct });
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(strict_discovery_io(&direct, error)),
    }

    let entries = fs::read_dir(root).map_err(|error| strict_discovery_io(root, error))?;
    let mut sorted_entries = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| strict_discovery_io(root, error))?;
        *entries_scanned += 1;
        if *entries_scanned > limits.max_entries {
            return Err(SkillDiscoveryError::Truncated {
                kind: "directory entry",
                limit: limits.max_entries,
            });
        }
        sorted_entries.push(entry);
    }
    sorted_entries.sort_by_key(|entry| entry.file_name());

    for entry in sorted_entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| strict_discovery_io(&path, error))?;
        if file_type.is_dir() {
            discover_skill_files_strict(&path, depth + 1, limits, files, seen, entries_scanned)?;
        }
    }
    Ok(())
}

fn is_skill_install_staging_directory(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".nib-skill-") && name.ends_with(".tmp"))
}

pub fn find_skills_in_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut entries_scanned = 0usize;
    for root in paths {
        discover_skill_files(root, 0, &mut files, &mut entries_scanned);
        if files.len() >= MAX_DISCOVERED_SKILLS || entries_scanned >= MAX_DISCOVERY_ENTRIES {
            break;
        }
    }

    let mut seen = HashSet::new();
    files.retain(|path| {
        let identity = path.canonicalize().unwrap_or_else(|_| path.clone());
        seen.insert(identity)
    });
    files.sort();
    files
}

fn find_skills_in_paths_strict_with_limits(
    paths: &[PathBuf],
    limits: SkillDiscoveryLimits,
) -> Result<Vec<PathBuf>, SkillDiscoveryError> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    let mut entries_scanned = 0usize;
    for root in paths {
        if is_skill_install_staging_directory(root) {
            continue;
        }
        match fs::symlink_metadata(root) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                return Err(SkillDiscoveryError::NotDirectory { path: root.clone() });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(strict_discovery_io(root, error)),
        }
        discover_skill_files_strict(root, 0, limits, &mut files, &mut seen, &mut entries_scanned)?;
    }
    files.sort();
    Ok(files)
}

pub fn find_skills_in_paths_strict(paths: &[PathBuf]) -> Result<Vec<PathBuf>, SkillDiscoveryError> {
    find_skills_in_paths_strict_with_limits(paths, SKILL_DISCOVERY_LIMITS)
}

pub fn find_skills(project_path: &Path) -> Vec<PathBuf> {
    find_skills_in_paths(&default_skill_roots(project_path))
}

/// Returns the stable directory/retention identifier used for a skill name.
/// Installation and persisted-usage reconciliation must use this exact mapping.
pub fn canonical_skill_id(name: &str) -> Result<String, SkillError> {
    if name.trim().is_empty() {
        return Err(SkillError::EmptyName);
    }
    let id = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|value| {
            if value.is_ascii_alphanumeric() || value == '-' || value == '_' {
                value
            } else {
                '-'
            }
        })
        .collect::<String>();
    let id = id.trim_matches('-').to_string();
    if id.is_empty()
        || id.len() > MAX_SKILL_ID_BYTES
        || !id.chars().any(|value| value.is_ascii_alphanumeric())
    {
        Err(SkillError::InvalidName)
    } else {
        Ok(id)
    }
}

pub fn parse_skill_file(path: &Path) -> Result<Skill, SkillError> {
    let (frontmatter, body) = parse_skill_document(path)?;
    let root = path
        .parent()
        .ok_or_else(|| SkillError::InvalidResource(path.display().to_string()))?
        .canonicalize()?;
    let mut references = Vec::with_capacity(frontmatter.references.len());
    let mut total_reference_bytes = 0_u64;
    for configured in &frontmatter.references {
        let resource = resolve_skill_resource(&root, configured)?;
        let content = read_skill_reference(&resource, configured)?;
        total_reference_bytes = total_reference_bytes
            .checked_add(content.len() as u64)
            .ok_or(SkillError::ReferencesTooLarge)?;
        if total_reference_bytes > MAX_TOTAL_REFERENCE_BYTES {
            return Err(SkillError::ReferencesTooLarge);
        }
        references.push(SkillReference {
            path: PathBuf::from(configured),
            content,
        });
    }
    let mut assets = Vec::with_capacity(frontmatter.assets.len());
    let mut total_asset_bytes = 0_u64;
    for configured in &frontmatter.assets {
        let resource = resolve_skill_resource(&root, configured)?;
        let metadata = fs::symlink_metadata(&resource)?;
        validate_skill_asset_metadata(configured, &metadata)?;
        total_asset_bytes = total_asset_bytes
            .checked_add(metadata.len())
            .ok_or(SkillError::AssetsTooLarge)?;
        if total_asset_bytes > MAX_TOTAL_ASSET_BYTES {
            return Err(SkillError::AssetsTooLarge);
        }
        assets.push(PathBuf::from(configured));
    }

    Ok(Skill {
        path: path.to_path_buf(),
        frontmatter,
        body,
        references,
        assets,
    })
}

/// Parses and bounds the manifest without requiring declared resources to exist yet.
/// Git installation uses this before checking out the exact declared paths.
pub fn parse_skill_frontmatter_file(path: &Path) -> Result<SkillFrontmatter, SkillError> {
    parse_skill_document(path).map(|(frontmatter, _)| frontmatter)
}

fn parse_skill_document(path: &Path) -> Result<(SkillFrontmatter, String), SkillError> {
    let parent = path
        .parent()
        .ok_or_else(|| SkillError::InvalidResource(path.display().to_string()))?;
    verify_skill_directory_components(parent, &path.display().to_string())?;
    let manifest = open_stable_skill_file(path, |metadata| {
        validate_skill_manifest_metadata(path, metadata)
    })?;
    let mut manifest_bytes = Vec::with_capacity(manifest.metadata()?.len() as usize);
    manifest
        .take(MAX_SKILL_FILE_BYTES + 1)
        .read_to_end(&mut manifest_bytes)?;
    if manifest_bytes.len() as u64 > MAX_SKILL_FILE_BYTES {
        return Err(SkillError::ManifestTooLarge(path.display().to_string()));
    }
    let content = String::from_utf8(manifest_bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let content = content
        .strip_prefix("---")
        .ok_or(SkillError::MissingFrontmatter)?;
    let (yaml, body) = content
        .split_once("\n---")
        .ok_or(SkillError::UnterminatedFrontmatter)?;
    let body = body
        .strip_prefix("\r")
        .unwrap_or(body)
        .strip_prefix('\n')
        .unwrap_or(body)
        .trim()
        .to_string();
    let frontmatter: SkillFrontmatter = serde_yaml::from_str(yaml)?;
    canonical_skill_id(&frontmatter.name)?;
    if frontmatter.references.len() > MAX_SKILL_REFERENCES {
        return Err(SkillError::TooManyResources {
            kind: "references",
            maximum: MAX_SKILL_REFERENCES,
            actual: frontmatter.references.len(),
        });
    }
    if frontmatter.assets.len() > MAX_SKILL_ASSETS {
        return Err(SkillError::TooManyResources {
            kind: "assets",
            maximum: MAX_SKILL_ASSETS,
            actual: frontmatter.assets.len(),
        });
    }
    for configured in frontmatter
        .references
        .iter()
        .chain(frontmatter.assets.iter())
    {
        validated_skill_resource_path(configured)?;
    }
    Ok((frontmatter, body))
}

fn validate_skill_manifest_metadata(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), SkillError> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SkillError::InvalidResource(path.display().to_string()));
    }
    if metadata.len() > MAX_SKILL_FILE_BYTES {
        return Err(SkillError::ManifestTooLarge(path.display().to_string()));
    }
    Ok(())
}

fn read_skill_reference(path: &Path, configured: &str) -> Result<String, SkillError> {
    let file = open_stable_skill_file(path, |metadata| {
        validate_skill_reference_metadata(configured, metadata)
    })?;
    let mut bytes = Vec::with_capacity(file.metadata()?.len() as usize);
    file.take(MAX_REFERENCE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_REFERENCE_BYTES {
        return Err(SkillError::ReferenceTooLarge(configured.to_string()));
    }
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error).into())
}

fn open_stable_skill_file(
    path: &Path,
    validate: impl Fn(&fs::Metadata) -> Result<(), SkillError>,
) -> Result<fs::File, SkillError> {
    open_stable_skill_file_with_hook(path, validate, || Ok(()))
}

fn open_stable_skill_file_with_hook(
    path: &Path,
    validate: impl Fn(&fs::Metadata) -> Result<(), SkillError>,
    after_open: impl FnOnce() -> Result<(), SkillError>,
) -> Result<fs::File, SkillError> {
    let before = fs::symlink_metadata(path)?;
    validate(&before)?;

    let before_probe = open_skill_file_without_following_links(path)?;
    let before_opened = before_probe.metadata()?;
    validate(&before_opened)?;
    verify_unix_metadata_identity(path, &before, &before_opened)?;
    let before_identity = crate::fs_security::FileIdentity::from_file(before_probe)?;

    let file = open_skill_file_without_following_links(path)?;
    let opened = file.metadata()?;
    validate(&opened)?;
    let opened_identity = crate::fs_security::FileIdentity::from_file(file.try_clone()?)?;
    if before_identity != opened_identity {
        return Err(SkillError::InvalidResource(path.display().to_string()));
    }
    verify_unix_metadata_identity(path, &before_opened, &opened)?;

    after_open()?;

    let after_probe = open_skill_file_without_following_links(path)?;
    let after_opened = after_probe.metadata()?;
    validate(&after_opened)?;
    let after_identity = crate::fs_security::FileIdentity::from_file(after_probe)?;
    if opened_identity != after_identity {
        return Err(SkillError::InvalidResource(path.display().to_string()));
    }
    let after = fs::symlink_metadata(path)?;
    validate(&after)?;
    verify_unix_metadata_identity(path, &opened, &after)?;
    Ok(file)
}

#[cfg(any(unix, windows))]
fn open_skill_file_without_following_links(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_skill_file_without_following_links(_path: &Path) -> std::io::Result<fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "stable no-follow skill reads are not supported on this platform",
    ))
}

#[cfg(unix)]
fn verify_unix_metadata_identity(
    path: &Path,
    left: &fs::Metadata,
    right: &fs::Metadata,
) -> Result<(), SkillError> {
    use std::os::unix::fs::MetadataExt;

    if left.dev() != right.dev() || left.ino() != right.ino() {
        return Err(SkillError::InvalidResource(path.display().to_string()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_unix_metadata_identity(
    _path: &Path,
    _left: &fs::Metadata,
    _right: &fs::Metadata,
) -> Result<(), SkillError> {
    Ok(())
}

fn validate_skill_reference_metadata(
    configured: &str,
    metadata: &fs::Metadata,
) -> Result<(), SkillError> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SkillError::InvalidResource(configured.to_string()));
    }
    if metadata.len() > MAX_REFERENCE_BYTES {
        return Err(SkillError::ReferenceTooLarge(configured.to_string()));
    }
    Ok(())
}

fn validate_skill_asset_metadata(
    configured: &str,
    metadata: &fs::Metadata,
) -> Result<(), SkillError> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SkillError::InvalidResource(configured.to_string()));
    }
    if metadata.len() > MAX_ASSET_BYTES {
        return Err(SkillError::AssetTooLarge(configured.to_string()));
    }
    Ok(())
}

pub fn validated_skill_resource_path(configured: &str) -> Result<PathBuf, SkillError> {
    let relative = Path::new(configured);
    let depth = relative
        .components()
        .filter(|component| matches!(component, std::path::Component::Normal(_)))
        .count();
    if configured.trim().is_empty()
        || configured.len() > MAX_RESOURCE_PATH_BYTES
        || depth == 0
        || depth > MAX_RESOURCE_DEPTH
        || relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
                    | std::path::Component::CurDir
            )
        })
    {
        return Err(SkillError::InvalidResource(configured.to_string()));
    }
    Ok(relative.to_path_buf())
}

fn resolve_skill_resource(root: &Path, configured: &str) -> Result<PathBuf, SkillError> {
    let relative = validated_skill_resource_path(configured)?;
    let declared = root.join(relative);
    verify_skill_directory_components(root, configured)?;
    let parent = declared
        .parent()
        .ok_or_else(|| SkillError::InvalidResource(configured.to_string()))?;
    verify_skill_directory_components(parent, configured)?;
    let metadata = fs::symlink_metadata(&declared)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(SkillError::InvalidResource(configured.to_string()));
    }
    Ok(declared)
}

fn verify_skill_directory_components(path: &Path, configured: &str) -> Result<(), SkillError> {
    crate::fs_security::verify_directory_without_symlinks(path)
        .map_err(|_| SkillError::InvalidResource(configured.to_string()))
}

pub fn parse_skill(path: &Path) -> Option<Skill> {
    parse_skill_file(path).ok()
}

fn task_tokens(task: &str) -> HashSet<String> {
    task.split(|value: char| !value.is_ascii_alphanumeric() && value != '-' && value != '_')
        .filter(|value| !value.is_empty())
        .map(str::to_lowercase)
        .collect()
}

pub fn skill_matches_task(skill: &Skill, task: &str) -> bool {
    let task_lower = task.to_lowercase();
    let tokens = task_tokens(task);
    let name = skill.frontmatter.name.to_lowercase();

    task_lower.contains(&name)
        || skill
            .frontmatter
            .tags
            .iter()
            .any(|tag| tokens.contains(&tag.to_lowercase()))
        || skill
            .frontmatter
            .description
            .split(|value: char| !value.is_ascii_alphanumeric())
            .filter(|word| word.len() > 3)
            .any(|word| tokens.contains(&word.to_lowercase()))
}

pub fn relevant_skills(project_path: &Path, task: Option<&str>) -> Vec<Skill> {
    let Some(task) = task.filter(|task| !task.trim().is_empty()) else {
        return Vec::new();
    };

    find_skills(project_path)
        .into_iter()
        .filter_map(|path| parse_skill_file(&path).ok())
        .filter(|skill| skill_matches_task(skill, task))
        .collect()
}

pub fn policy_rules_for_skills(skills: &[Skill]) -> Vec<SkillPolicyRule> {
    let mut rules = Vec::new();
    for skill in skills {
        let name = skill.frontmatter.name.clone();
        rules.extend(
            skill
                .frontmatter
                .constraints
                .deny_tools
                .iter()
                .map(|tool| SkillPolicyRule {
                    skill_name: name.clone(),
                    tool_name: Some(tool.clone()),
                    argument_contains: None,
                    effect: SkillPolicyEffect::Deny,
                }),
        );
        rules.extend(
            skill
                .frontmatter
                .constraints
                .deny_commands
                .iter()
                .map(|pattern| SkillPolicyRule {
                    skill_name: name.clone(),
                    tool_name: Some("run_terminal".to_string()),
                    argument_contains: Some(pattern.clone()),
                    effect: SkillPolicyEffect::Deny,
                }),
        );
        rules.extend(
            skill
                .frontmatter
                .constraints
                .require_approval_tools
                .iter()
                .map(|tool| SkillPolicyRule {
                    skill_name: name.clone(),
                    tool_name: Some(tool.clone()),
                    argument_contains: None,
                    effect: SkillPolicyEffect::RequireApproval,
                }),
        );
        rules.extend(
            skill
                .frontmatter
                .constraints
                .require_approval_commands
                .iter()
                .map(|pattern| SkillPolicyRule {
                    skill_name: name.clone(),
                    tool_name: Some("run_terminal".to_string()),
                    argument_contains: Some(pattern.clone()),
                    effect: SkillPolicyEffect::RequireApproval,
                }),
        );
    }
    rules
}

pub fn load_relevant_skills(project_path: &Path, task: Option<&str>) -> String {
    let injected: Vec<String> = relevant_skills(project_path, task)
        .into_iter()
        .map(|skill| {
            let mut rendered = format!(
                "### Skill: {}\n{}\n\n{}",
                skill.frontmatter.name, skill.frontmatter.description, skill.body
            );
            for reference in &skill.references {
                rendered.push_str(&format!(
                    "\n\n#### Skill Reference: {}\n{}",
                    reference.path.display(),
                    reference.content
                ));
            }
            if !skill.assets.is_empty() {
                rendered.push_str("\n\nVerified skill assets:\n");
                for asset in &skill.assets {
                    rendered.push_str(&format!("- {}\n", asset.display()));
                }
            }
            rendered
        })
        .collect();

    if injected.is_empty() {
        String::new()
    } else {
        format!("## Active Skills\n\n{}", injected.join("\n---\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::tempdir;

    fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }

    fn write_skill(root: &Path) -> PathBuf {
        let dir = root.join("rust-safety");
        fs::create_dir_all(&dir).expect("create skill directory");
        fs::create_dir_all(dir.join("references")).expect("reference directory");
        fs::create_dir_all(dir.join("templates")).expect("asset directory");
        fs::write(dir.join("references/checks.md"), "reference checks\n").expect("reference");
        fs::write(dir.join("templates/report.md"), "report template\n").expect("asset");
        let path = dir.join("SKILL.md");
        fs::write(
            &path,
            r#"---
name: rust-safety
description: Validate Rust changes safely
version: "1.2.0"
tags: [rust, cargo]
references: [references/checks.md]
assets: [templates/report.md]
constraints:
  deny_commands: ["cargo publish"]
  require_approval_tools: [apply_patch]
hooks:
  after_tool:
    - tool: apply_patch
      command: task check
---
Run the canonical Task gates after editing.
"#,
        )
        .expect("write skill");
        path
    }

    #[test]
    fn parses_structured_frontmatter() {
        let dir = tempdir().expect("tempdir");
        let path = write_skill(dir.path());
        let skill = parse_skill_file(&path).expect("parse skill");

        assert_eq!(skill.frontmatter.version.as_deref(), Some("1.2.0"));
        assert_eq!(skill.frontmatter.tags, ["rust", "cargo"]);
        assert_eq!(skill.frontmatter.hooks.after_tool[0].command, "task check");
        assert!(skill.body.contains("canonical Task gates"));
        assert_eq!(
            skill.references[0].path,
            PathBuf::from("references/checks.md")
        );
        assert!(skill.references[0].content.contains("reference checks"));
        assert_eq!(skill.assets, [PathBuf::from("templates/report.md")]);
    }

    #[test]
    fn discovers_matches_and_builds_policy_rules() {
        let dir = tempdir().expect("tempdir");
        let path = write_skill(dir.path());
        let paths = find_skills_in_paths(&[dir.path().to_path_buf()]);
        assert_eq!(paths, vec![path]);

        let skill = parse_skill_file(&paths[0]).expect("parse skill");
        assert!(skill_matches_task(&skill, "repair the Rust executor"));
        assert!(!skill_matches_task(&skill, "write a CSS theme"));

        let rules = policy_rules_for_skills(&[skill]);
        assert!(rules.iter().any(|rule| {
            rule.effect == SkillPolicyEffect::Deny
                && rule.argument_contains.as_deref() == Some("cargo publish")
        }));
        assert!(rules.iter().any(|rule| {
            rule.effect == SkillPolicyEffect::RequireApproval
                && rule.tool_name.as_deref() == Some("apply_patch")
        }));
    }

    #[test]
    #[serial]
    fn configured_global_skill_root_is_discovered() {
        let project = tempdir().expect("project tempdir");
        let global = tempdir().expect("global tempdir");
        let manifest = write_skill(global.path());
        let previous = std::env::var_os("NIB_SKILLS_DIR");
        std::env::set_var("NIB_SKILLS_DIR", global.path());

        let discovered = find_skills(project.path());

        restore_env("NIB_SKILLS_DIR", previous);
        assert!(discovered.contains(&manifest));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_regular_file_replacement_between_open_and_recheck() {
        let directory = tempdir().expect("tempdir");
        let manifest = directory.path().join("SKILL.md");
        let replacement = directory.path().join("replacement.md");
        fs::write(&manifest, "---\nname: original\n---\nBody\n").expect("manifest");
        fs::write(&replacement, "---\nname: replaced\n---\nBody\n").expect("replacement");

        let result = open_stable_skill_file_with_hook(
            &manifest,
            |metadata| validate_skill_manifest_metadata(&manifest, metadata),
            || {
                fs::rename(&replacement, &manifest)?;
                Ok(())
            },
        );

        assert!(matches!(result, Err(SkillError::InvalidResource(_))));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_reference_replacement_between_open_and_recheck() {
        let directory = tempdir().expect("tempdir");
        let reference = directory.path().join("reference.md");
        let replacement = directory.path().join("replacement.md");
        fs::write(&reference, "original reference\n").expect("reference");
        fs::write(&replacement, "replaced reference\n").expect("replacement");

        let result = open_stable_skill_file_with_hook(
            &reference,
            |metadata| validate_skill_reference_metadata("reference.md", metadata),
            || {
                fs::rename(&replacement, &reference)?;
                Ok(())
            },
        );

        assert!(matches!(result, Err(SkillError::InvalidResource(_))));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_skill_resources_inside_or_outside_the_skill_root() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().expect("tempdir");
        let skill_dir = dir.path().join("skill");
        fs::create_dir_all(&skill_dir).expect("skill dir");
        let outside = dir.path().join("outside.md");
        fs::write(&outside, "secret").expect("outside");
        symlink(&outside, skill_dir.join("reference.md")).expect("symlink");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: scoped\nreferences: [reference.md]\n---\nBody\n",
        )
        .expect("manifest");

        assert!(matches!(
            parse_skill_file(&skill_dir.join("SKILL.md")),
            Err(SkillError::InvalidResource(_))
        ));

        fs::remove_file(skill_dir.join("reference.md")).expect("remove escape symlink");
        fs::write(skill_dir.join("real.md"), "local reference").expect("local reference");
        symlink("real.md", skill_dir.join("reference.md")).expect("local symlink");
        assert!(matches!(
            parse_skill_file(&skill_dir.join("SKILL.md")),
            Err(SkillError::InvalidResource(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_resource_directory_ancestor() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().expect("tempdir");
        let skill_dir = directory.path().join("skill");
        let outside = directory.path().join("outside");
        fs::create_dir_all(&skill_dir).expect("skill directory");
        fs::create_dir_all(&outside).expect("outside directory");
        fs::write(outside.join("secret.md"), "secret").expect("outside resource");
        symlink(&outside, skill_dir.join("references")).expect("ancestor symlink");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: ancestor-link\nreferences: [references/secret.md]\n---\nBody\n",
        )
        .expect("manifest");

        assert!(matches!(
            parse_skill_file(&skill_dir.join("SKILL.md")),
            Err(SkillError::InvalidResource(_))
        ));
    }

    #[test]
    fn discovery_ignores_atomic_install_staging_directories() {
        let directory = tempdir().expect("tempdir");
        let published = directory.path().join("published");
        let staging = directory.path().join(".nib-skill-deadbeef.tmp");
        fs::create_dir_all(&published).expect("published directory");
        fs::create_dir_all(&staging).expect("staging directory");
        fs::write(
            published.join("SKILL.md"),
            "---\nname: published\n---\nBody\n",
        )
        .expect("published manifest");
        fs::write(
            staging.join("SKILL.md"),
            "---\nname: unpublished\n---\nBody\n",
        )
        .expect("staging manifest");

        let discovered = find_skills_in_paths(&[directory.path().to_path_buf()]);
        assert_eq!(discovered, vec![published.join("SKILL.md")]);
    }

    #[test]
    fn strict_discovery_rejects_directory_skill_manifest() {
        let directory = tempdir().expect("tempdir");
        let skill = directory.path().join("directory-manifest");
        let manifest = skill.join("SKILL.md");
        fs::create_dir_all(&manifest).expect("manifest directory");

        let error = find_skills_in_paths_strict(&[directory.path().to_path_buf()])
            .expect_err("manifest directory must fail strict discovery");
        assert!(matches!(
            error,
            SkillDiscoveryError::InvalidManifestType { path } if path == manifest
        ));
    }

    #[cfg(unix)]
    #[test]
    fn strict_discovery_rejects_dangling_symlink_skill_manifest() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().expect("tempdir");
        let skill = directory.path().join("dangling-manifest");
        fs::create_dir(&skill).expect("skill directory");
        let manifest = skill.join("SKILL.md");
        symlink(directory.path().join("missing-manifest"), &manifest)
            .expect("dangling manifest symlink");

        let error = find_skills_in_paths_strict(&[directory.path().to_path_buf()])
            .expect_err("dangling manifest symlink must fail strict discovery");
        assert!(matches!(
            error,
            SkillDiscoveryError::InvalidManifestType { path } if path == manifest
        ));
    }

    #[cfg(unix)]
    #[test]
    fn strict_discovery_rejects_special_file_skill_manifest() {
        use std::os::unix::net::UnixListener;

        let directory = tempdir().expect("tempdir");
        let skill = directory.path().join("special-manifest");
        fs::create_dir(&skill).expect("skill directory");
        let manifest = skill.join("SKILL.md");
        let _listener = UnixListener::bind(&manifest).expect("manifest socket");

        let error = find_skills_in_paths_strict(&[directory.path().to_path_buf()])
            .expect_err("special-file manifest must fail strict discovery");
        assert!(matches!(
            error,
            SkillDiscoveryError::InvalidManifestType { path } if path == manifest
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_skill_manifest() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().expect("tempdir");
        let outside = directory.path().join("outside.md");
        fs::write(&outside, "---\nname: linked\n---\nBody\n").expect("outside manifest");
        let skill_dir = directory.path().join("skill");
        fs::create_dir(&skill_dir).expect("skill directory");
        symlink(&outside, skill_dir.join("SKILL.md")).expect("manifest symlink");

        assert!(matches!(
            parse_skill_file(&skill_dir.join("SKILL.md")),
            Err(SkillError::InvalidResource(_))
        ));
    }

    #[test]
    fn discovery_caps_the_number_of_skill_manifests() {
        let directory = tempdir().expect("tempdir");
        for index in 0..(MAX_DISCOVERED_SKILLS + 4) {
            let skill = directory.path().join(format!("skill-{index:03}"));
            fs::create_dir(&skill).expect("skill directory");
            fs::write(
                skill.join("SKILL.md"),
                format!("---\nname: skill-{index:03}\n---\nBody\n"),
            )
            .expect("skill manifest");
        }

        let discovered = find_skills_in_paths(&[directory.path().to_path_buf()]);
        assert_eq!(discovered.len(), MAX_DISCOVERED_SKILLS);
    }

    #[test]
    fn strict_discovery_reports_skill_count_truncation() {
        let directory = tempdir().expect("tempdir");
        for name in ["alpha", "beta"] {
            let skill = directory.path().join(name);
            fs::create_dir(&skill).expect("skill directory");
            fs::write(
                skill.join("SKILL.md"),
                format!("---\nname: {name}\n---\nBody\n"),
            )
            .expect("skill manifest");
        }

        let error = find_skills_in_paths_strict_with_limits(
            &[directory.path().to_path_buf()],
            SkillDiscoveryLimits {
                max_skills: 1,
                max_entries: 16,
                max_depth: 4,
            },
        )
        .expect_err("strict discovery must report incomplete inventory");

        assert!(matches!(
            error,
            SkillDiscoveryError::Truncated {
                kind: "skill count",
                limit: 1
            }
        ));
    }

    #[test]
    fn strict_discovery_reports_directory_entry_truncation() {
        let directory = tempdir().expect("tempdir");
        fs::create_dir(directory.path().join("alpha")).expect("first directory");
        fs::create_dir(directory.path().join("beta")).expect("second directory");

        let error = find_skills_in_paths_strict_with_limits(
            &[directory.path().to_path_buf()],
            SkillDiscoveryLimits {
                max_skills: 16,
                max_entries: 1,
                max_depth: 4,
            },
        )
        .expect_err("strict discovery must report incomplete traversal");

        assert!(matches!(
            error,
            SkillDiscoveryError::Truncated {
                kind: "directory entry",
                limit: 1
            }
        ));
    }

    #[test]
    fn rejects_aggregate_reference_content_over_context_bound() {
        let dir = tempdir().expect("tempdir");
        let skill_dir = dir.path().join("bounded");
        fs::create_dir_all(&skill_dir).expect("skill dir");
        let mut configured = Vec::new();
        for index in 0..3 {
            let name = format!("reference-{index}.md");
            fs::write(skill_dir.join(&name), "x".repeat(22_000)).expect("reference");
            configured.push(name);
        }
        fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                "---\nname: bounded\nreferences: [{}]\n---\nBody\n",
                configured.join(", ")
            ),
        )
        .expect("manifest");

        assert!(matches!(
            parse_skill_file(&skill_dir.join("SKILL.md")),
            Err(SkillError::ReferencesTooLarge)
        ));
    }

    #[test]
    fn rejects_oversized_sparse_manifest_and_reference_before_reading() {
        let directory = tempdir().expect("tempdir");
        let skill_dir = directory.path().join("bounded");
        fs::create_dir(&skill_dir).expect("skill directory");
        let manifest = skill_dir.join("SKILL.md");
        fs::File::create(&manifest)
            .and_then(|file| file.set_len(MAX_SKILL_FILE_BYTES + 1))
            .expect("sparse manifest");
        assert!(matches!(
            parse_skill_file(&manifest),
            Err(SkillError::ManifestTooLarge(_))
        ));

        let reference = skill_dir.join("reference.md");
        fs::File::create(&reference)
            .and_then(|file| file.set_len(MAX_REFERENCE_BYTES + 1))
            .expect("sparse reference");
        fs::write(
            &manifest,
            "---\nname: bounded\nreferences: [reference.md]\n---\nBody\n",
        )
        .expect("bounded manifest");
        assert!(matches!(
            parse_skill_file(&manifest),
            Err(SkillError::ReferenceTooLarge(_))
        ));
    }

    #[test]
    fn rejects_oversized_and_aggregate_assets_before_installation() {
        let directory = tempdir().expect("tempdir");
        let skill_dir = directory.path().join("asset-bounds");
        fs::create_dir(&skill_dir).expect("skill directory");
        let manifest = skill_dir.join("SKILL.md");
        let oversized = skill_dir.join("oversized.bin");
        fs::File::create(&oversized)
            .and_then(|file| file.set_len(MAX_ASSET_BYTES + 1))
            .expect("sparse oversized asset");
        fs::write(
            &manifest,
            "---\nname: asset-bounds\nassets: [oversized.bin]\n---\nBody\n",
        )
        .expect("manifest");
        assert!(matches!(
            parse_skill_file(&manifest),
            Err(SkillError::AssetTooLarge(_))
        ));

        let mut assets = Vec::new();
        for index in 0..5 {
            let name = format!("asset-{index}.bin");
            fs::File::create(skill_dir.join(&name))
                .and_then(|file| file.set_len(MAX_ASSET_BYTES))
                .expect("sparse aggregate asset");
            assets.push(name);
        }
        fs::write(
            &manifest,
            format!(
                "---\nname: asset-bounds\nassets: [{}]\n---\nBody\n",
                assets.join(", ")
            ),
        )
        .expect("aggregate manifest");
        assert!(matches!(
            parse_skill_file(&manifest),
            Err(SkillError::AssetsTooLarge)
        ));
    }

    #[test]
    fn rejects_resource_paths_beyond_the_depth_bound_before_resolution() {
        let directory = tempdir().expect("tempdir");
        let skill_dir = directory.path().join("deep-resource");
        fs::create_dir(&skill_dir).expect("skill directory");
        let relative = (0..=MAX_RESOURCE_DEPTH)
            .map(|index| format!("level-{index}"))
            .collect::<Vec<_>>()
            .join("/");
        let manifest = skill_dir.join("SKILL.md");
        fs::write(
            &manifest,
            format!("---\nname: deep-resource\nassets: [{relative}]\n---\nBody\n"),
        )
        .expect("manifest");

        assert!(matches!(
            parse_skill_frontmatter_file(&manifest),
            Err(SkillError::InvalidResource(_))
        ));
    }
}
