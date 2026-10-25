use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;
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
const MAX_FRONTMATTER_CACHE_ENTRIES: usize = 512;

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
pub enum SkillSelectionSource {
    Configured,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSelectionRecord {
    pub skill_name: String,
    pub reason: String,
    pub source: SkillSelectionSource,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillSelection {
    pub skills: Vec<Skill>,
    pub records: Vec<SkillSelectionRecord>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManifestStamp {
    pub(crate) len: u64,
    pub(crate) modified: Option<SystemTime>,
    pub(crate) frontmatter_digest: [u8; 32],
}

#[derive(Debug, Clone)]
struct CachedFrontmatter {
    pub(crate) stamp: ManifestStamp,
    pub(crate) frontmatter: SkillFrontmatter,
}

static FRONTMATTER_CACHE: OnceLock<Mutex<BTreeMap<PathBuf, CachedFrontmatter>>> = OnceLock::new();

#[derive(Clone, Copy)]
struct SkillDiscoveryLimits {
    pub(crate) max_skills: usize,
    pub(crate) max_entries: usize,
    pub(crate) max_depth: usize,
}

const SKILL_DISCOVERY_LIMITS: SkillDiscoveryLimits = SkillDiscoveryLimits {
    max_skills: MAX_DISCOVERED_SKILLS,
    max_entries: MAX_DISCOVERY_ENTRIES,
    max_depth: MAX_SKILL_DISCOVERY_DEPTH,
};

fn default_skill_roots(project_path: &Path) -> Vec<PathBuf> {
    let mut roots = vec![
        crate::paths::NibPaths::project_skills(project_path),
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
    let parent = path
        .parent()
        .ok_or_else(|| SkillError::InvalidResource(path.display().to_string()))?;
    verify_skill_directory_components(parent, &path.display().to_string())?;
    let metadata = fs::symlink_metadata(path)?;
    validate_skill_manifest_metadata(path, &metadata)?;
    let canonical = path.canonicalize()?;
    let (yaml, frontmatter_digest) = read_skill_frontmatter_source(path)?;
    let stamp = ManifestStamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        frontmatter_digest,
    };
    let cache = FRONTMATTER_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    if let Some(cached) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&canonical)
        .filter(|cached| cached.stamp == stamp)
        .cloned()
    {
        return Ok(cached.frontmatter);
    }

    let after = fs::symlink_metadata(path)?;
    validate_skill_manifest_metadata(path, &after)?;
    let after_stamp = ManifestStamp {
        len: after.len(),
        modified: after.modified().ok(),
        frontmatter_digest,
    };
    if after_stamp != stamp || path.canonicalize()? != canonical {
        return Err(SkillError::InvalidResource(path.display().to_string()));
    }
    let mut cache = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if cache.len() >= MAX_FRONTMATTER_CACHE_ENTRIES && !cache.contains_key(&canonical) {
        cache.clear();
    }
    let frontmatter: SkillFrontmatter = serde_yaml::from_str(&yaml)?;
    let frontmatter = validate_skill_frontmatter(frontmatter)?;
    cache.insert(
        canonical,
        CachedFrontmatter {
            stamp,
            frontmatter: frontmatter.clone(),
        },
    );
    Ok(frontmatter)
}

fn read_skill_frontmatter_source(path: &Path) -> Result<(String, [u8; 32]), SkillError> {
    let manifest = open_stable_skill_file(path, |metadata| {
        validate_skill_manifest_metadata(path, metadata)
    })?;
    let opened_identity = crate::fs_security::FileIdentity::from_file(manifest.try_clone()?)?;
    let mut reader = BufReader::new(manifest.take(MAX_SKILL_FILE_BYTES + 1));
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 || line.trim_end_matches(['\r', '\n']) != "---" {
        return Err(SkillError::MissingFrontmatter);
    }
    let mut yaml = String::new();
    let mut bytes = line.len() as u64;
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Err(SkillError::UnterminatedFrontmatter);
        }
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| SkillError::ManifestTooLarge(path.display().to_string()))?;
        if bytes > MAX_SKILL_FILE_BYTES {
            return Err(SkillError::ManifestTooLarge(path.display().to_string()));
        }
        if line.trim_end_matches(['\r', '\n']) == "---" {
            break;
        }
        yaml.push_str(&line);
    }
    let after = open_skill_file_without_following_links(path)?;
    validate_skill_manifest_metadata(path, &after.metadata()?)?;
    let after_identity = crate::fs_security::FileIdentity::from_file(after)?;
    if opened_identity != after_identity {
        return Err(SkillError::InvalidResource(path.display().to_string()));
    }
    let digest = Sha256::digest(yaml.as_bytes()).into();
    Ok((yaml, digest))
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
    let frontmatter = validate_skill_frontmatter(frontmatter)?;
    Ok((frontmatter, body))
}

fn validate_skill_frontmatter(
    frontmatter: SkillFrontmatter,
) -> Result<SkillFrontmatter, SkillError> {
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
    Ok(frontmatter)
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

fn open_skill_file_without_following_links(path: &Path) -> std::io::Result<fs::File> {
    super::skill_io::open_regular_file(path)
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

/// Bounded, no-link read used by skill catalog and activated resource capabilities.
pub fn read_skill_resource_file(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let parent = path.parent().ok_or("skill resource has no parent")?;
    verify_skill_directory_components(parent, "skill resource")
        .map_err(|error| error.to_string())?;
    let file = open_stable_skill_file(path, |metadata| {
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit {
            return Err(SkillError::InvalidResource(
                "skill resource type or byte limit".into(),
            ));
        }
        Ok(())
    })
    .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > limit {
        return Err("skill resource exceeds byte limit".into());
    }
    Ok(bytes)
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

pub fn render_skill_content(skill: &Skill) -> String {
    let mut content = format!("{}\n\n{}", skill.frontmatter.description, skill.body);
    for reference in &skill.references {
        content.push_str(&format!(
            "\n\n#### Skill Reference: {}\n{}",
            reference.path.display(),
            reference.content
        ));
    }
    if !skill.assets.is_empty() {
        content.push_str("\n\nVerified skill assets:\n");
        for asset in &skill.assets {
            content.push_str(&format!("- {}\n", asset.display()));
        }
    }
    content
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

#[cfg(test)]
#[path = "skills_tests.rs"]
mod tests;
