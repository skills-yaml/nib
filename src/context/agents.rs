//! Scoped project-instruction discovery with bounded, identity-checked reads.

use crate::tools::classifier::{classify_command, ToolRisk};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

const MAX_INSTRUCTION_FILE_BYTES: u64 = 64 * 1024;
const MAX_INSTRUCTION_TOTAL_BYTES: usize = 256 * 1024;
const MAX_INSTRUCTION_SCOPES: usize = 32;
const MAX_INSTRUCTION_DEPTH: usize = 64;

const BASE_FILENAMES: &[&str] = &["AGENTS.md", "CLAUDE.md"];
const LOCAL_FILENAMES: &[&str] = &["AGENTS.local.md", "CLAUDE.local.md"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInstructionSource {
    pub path: PathBuf,
    pub scope: String,
    pub identity: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInstructions {
    pub sources: Vec<ResolvedInstructionSource>,
    pub identity: String,
}

impl ResolvedInstructions {
    pub fn render(&self) -> String {
        if self.sources.is_empty() {
            return "# No applicable project instructions found".to_string();
        }
        self.sources
            .iter()
            .map(|source| {
                format!(
                    "# Instruction source: {}\nScope: {}\nContent identity: {}\n\n{}",
                    source.path.display(),
                    source.scope,
                    source.identity,
                    source.content
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MetadataIdentity {
    file: crate::fs_security::FileIdentitySnapshot,
    len: u64,
    modified_nanos: u128,
    changed_nanos: i128,
}

#[derive(Debug, Clone)]
struct CachedInstruction {
    metadata: MetadataIdentity,
    identity: String,
    content: String,
}

/// Reuses unchanged bounded reads while revalidating metadata on each affected scope.
pub struct InstructionResolver {
    repository_root: PathBuf,
    home: Option<PathBuf>,
    cache: HashMap<PathBuf, CachedInstruction>,
    physical_reads: usize,
}

impl InstructionResolver {
    pub fn new(repository_root: &Path) -> Result<Self, String> {
        let repository_root = repository_root.canonicalize().map_err(|error| {
            format!(
                "failed to resolve project instruction root {}: {error}",
                repository_root.display()
            )
        })?;
        let metadata = fs::symlink_metadata(&repository_root).map_err(|error| {
            format!(
                "failed to inspect project instruction root {}: {error}",
                repository_root.display()
            )
        })?;
        if !metadata.is_dir() || crate::fs_security::metadata_is_link_or_reparse(&metadata) {
            return Err("project instruction root must be a real local directory".to_string());
        }
        Ok(Self {
            repository_root,
            home: std::env::var_os("HOME").map(PathBuf::from),
            cache: HashMap::new(),
            physical_reads: 0,
        })
    }

    #[cfg(test)]
    pub fn physical_reads(&self) -> usize {
        self.physical_reads
    }

    pub fn resolve_for_scopes<I, P>(&mut self, scopes: I) -> Result<ResolvedInstructions, String>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut normalized = scopes
            .into_iter()
            .map(|scope| self.normalize_scope(scope.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;
        if normalized.is_empty() {
            normalized.push(self.repository_root.clone());
        }
        normalized.sort();
        normalized.dedup();
        if normalized.len() > MAX_INSTRUCTION_SCOPES {
            return Err(format!(
                "instruction discovery requires {} scopes; maximum is {MAX_INSTRUCTION_SCOPES}",
                normalized.len()
            ));
        }

        let mut selected = BTreeMap::<PathBuf, String>::new();
        if let Some(home) = self.home.clone() {
            let global = home.join("AGENTS.md");
            if !global.starts_with(&self.repository_root) {
                self.select_existing(&global, "global".to_string(), &mut selected)?;
            }
        }
        for scope in &normalized {
            for directory in self.directory_chain(scope)? {
                let scope_label = directory
                    .strip_prefix(&self.repository_root)
                    .ok()
                    .filter(|path| !path.as_os_str().is_empty())
                    .map(|path| format!("{}/**", path.display()))
                    .unwrap_or_else(|| "repository/**".to_string());
                self.select_first(&directory, BASE_FILENAMES, &scope_label, &mut selected)?;
                self.select_first(&directory, LOCAL_FILENAMES, &scope_label, &mut selected)?;
            }
        }

        let mut total = 0usize;
        let mut sources = Vec::new();
        for (path, scope) in selected {
            let cached = self.read_instruction(&path)?;
            total = total
                .checked_add(cached.content.len())
                .ok_or_else(|| "aggregate project instruction size overflowed".to_string())?;
            if total > MAX_INSTRUCTION_TOTAL_BYTES {
                return Err(format!(
                    "applicable project instructions exceed the {MAX_INSTRUCTION_TOTAL_BYTES}-byte aggregate limit"
                ));
            }
            sources.push(ResolvedInstructionSource {
                path,
                scope,
                identity: cached.identity,
                content: cached.content,
            });
        }
        sources.sort_by(|left, right| {
            instruction_order_key(&self.repository_root, &left.path)
                .cmp(&instruction_order_key(&self.repository_root, &right.path))
        });
        let identity_material = sources
            .iter()
            .flat_map(|source| {
                let source_identity_path = source
                    .path
                    .strip_prefix(&self.repository_root)
                    .unwrap_or(&source.path);
                [
                    source_identity_path.to_string_lossy().as_bytes().to_vec(),
                    source.scope.as_bytes().to_vec(),
                    source.identity.as_bytes().to_vec(),
                ]
            })
            .flatten()
            .collect::<Vec<_>>();
        Ok(ResolvedInstructions {
            sources,
            identity: digest_bytes(&identity_material),
        })
    }

    fn normalize_scope(&self, scope: &Path) -> Result<PathBuf, String> {
        let candidate = if scope.is_absolute() {
            scope.to_path_buf()
        } else {
            self.repository_root.join(scope)
        };
        let mut normalized = self.repository_root.clone();
        let relative = candidate.strip_prefix(&self.repository_root).map_err(|_| {
            format!(
                "instruction scope {} is outside the active worktree",
                candidate.display()
            )
        })?;
        for component in relative.components() {
            match component {
                Component::Normal(component) => normalized.push(component),
                Component::CurDir => {}
                _ => {
                    return Err(format!(
                        "instruction scope {} is not a bounded worktree path",
                        scope.display()
                    ))
                }
            }
        }
        let existing = nearest_existing_ancestor(&normalized).ok_or_else(|| {
            format!(
                "instruction scope {} has no inspectable ancestor",
                normalized.display()
            )
        })?;
        let metadata = fs::symlink_metadata(&existing).map_err(|error| {
            format!(
                "failed to inspect instruction scope {}: {error}",
                existing.display()
            )
        })?;
        if crate::fs_security::metadata_is_link_or_reparse(&metadata) {
            return Err(format!(
                "instruction scope traverses a linked path: {}",
                existing.display()
            ));
        }
        let canonical = existing.canonicalize().map_err(|error| {
            format!(
                "failed to resolve instruction scope {}: {error}",
                existing.display()
            )
        })?;
        if !canonical.starts_with(&self.repository_root) {
            return Err(format!(
                "instruction scope resolves outside the active worktree: {}",
                scope.display()
            ));
        }
        if normalized.is_file() {
            normalized.pop();
        }
        Ok(normalized)
    }

    fn directory_chain(&self, scope: &Path) -> Result<Vec<PathBuf>, String> {
        let relative = scope.strip_prefix(&self.repository_root).map_err(|_| {
            "instruction scope escaped the active worktree after validation".to_string()
        })?;
        let mut directories = vec![self.repository_root.clone()];
        let mut current = self.repository_root.clone();
        for component in relative.components() {
            if directories.len() >= MAX_INSTRUCTION_DEPTH {
                return Err(format!(
                    "instruction scope exceeds the {MAX_INSTRUCTION_DEPTH}-directory depth limit"
                ));
            }
            if let Component::Normal(component) = component {
                current.push(component);
                if current.is_dir() {
                    let metadata = fs::symlink_metadata(&current).map_err(|error| {
                        format!(
                            "failed to inspect instruction directory {}: {error}",
                            current.display()
                        )
                    })?;
                    if crate::fs_security::metadata_is_link_or_reparse(&metadata) {
                        return Err(format!(
                            "instruction scope contains a linked directory: {}",
                            current.display()
                        ));
                    }
                    directories.push(current.clone());
                }
            }
        }
        Ok(directories)
    }

    fn select_first(
        &mut self,
        directory: &Path,
        filenames: &[&str],
        scope: &str,
        selected: &mut BTreeMap<PathBuf, String>,
    ) -> Result<(), String> {
        for filename in filenames {
            let path = directory.join(filename);
            match fs::symlink_metadata(&path) {
                Ok(_) => {
                    self.select_existing(&path, scope.to_string(), selected)?;
                    return Ok(());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "failed to inspect required instruction {}: {error}",
                        path.display()
                    ))
                }
            }
        }
        Ok(())
    }

    fn select_existing(
        &mut self,
        path: &Path,
        scope: String,
        selected: &mut BTreeMap<PathBuf, String>,
    ) -> Result<(), String> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if crate::fs_security::metadata_is_link_or_reparse(&metadata) || !metadata.is_file()
                {
                    return Err(format!(
                        "required instruction must be a regular local file: {}",
                        path.display()
                    ));
                }
                if metadata.len() > MAX_INSTRUCTION_FILE_BYTES {
                    return Err(format!(
                        "required instruction {} exceeds the {MAX_INSTRUCTION_FILE_BYTES}-byte limit",
                        path.display()
                    ));
                }
                selected.insert(path.to_path_buf(), scope);
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!(
                "failed to inspect required instruction {}: {error}",
                path.display()
            )),
        }
    }

    fn read_instruction(&mut self, path: &Path) -> Result<ResolvedInstructionSource, String> {
        let (mut file, before) = open_instruction(path)?;
        if let Some(cached) = self
            .cache
            .get(path)
            .filter(|cached| cached.metadata == before)
        {
            return Ok(ResolvedInstructionSource {
                path: path.to_path_buf(),
                scope: String::new(),
                identity: cached.identity.clone(),
                content: cached.content.clone(),
            });
        }
        let mut bytes = Vec::with_capacity(before.len as usize);
        file.by_ref()
            .take(MAX_INSTRUCTION_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| {
                format!(
                    "failed to read required instruction {}: {error}",
                    path.display()
                )
            })?;
        if bytes.len() as u64 > MAX_INSTRUCTION_FILE_BYTES {
            return Err(format!(
                "required instruction {} exceeds the {MAX_INSTRUCTION_FILE_BYTES}-byte limit",
                path.display()
            ));
        }
        let (_, after) = open_instruction(path)?;
        if before != after {
            return Err(format!(
                "required instruction changed while being read: {}",
                path.display()
            ));
        }
        let content = String::from_utf8(bytes).map_err(|error| {
            format!(
                "required instruction {} is not UTF-8: {error}",
                path.display()
            )
        })?;
        let identity = digest_bytes(content.as_bytes());
        self.physical_reads += 1;
        self.cache.insert(
            path.to_path_buf(),
            CachedInstruction {
                metadata: before,
                identity: identity.clone(),
                content: content.clone(),
            },
        );
        Ok(ResolvedInstructionSource {
            path: path.to_path_buf(),
            scope: String::new(),
            identity,
            content,
        })
    }
}

fn open_instruction(path: &Path) -> Result<(File, MetadataIdentity), String> {
    let path_metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "failed to inspect required instruction {}: {error}",
            path.display()
        )
    })?;
    if crate::fs_security::metadata_is_link_or_reparse(&path_metadata) || !path_metadata.is_file() {
        return Err(format!(
            "required instruction must remain a regular local file: {}",
            path.display()
        ));
    }
    let file = open_instruction_without_following_links(path).map_err(|error| {
        format!(
            "failed to open required instruction {} without following links: {error}",
            path.display()
        )
    })?;
    let metadata = file.metadata().map_err(|error| {
        format!(
            "failed to inspect opened required instruction {}: {error}",
            path.display()
        )
    })?;
    if crate::fs_security::metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(format!(
            "required instruction must remain a regular local file: {}",
            path.display()
        ));
    }
    if metadata.len() > MAX_INSTRUCTION_FILE_BYTES {
        return Err(format!(
            "required instruction {} exceeds the {MAX_INSTRUCTION_FILE_BYTES}-byte limit",
            path.display()
        ));
    }
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_nanos())
        .unwrap_or(0);
    let file_identity = crate::fs_security::file_identity_snapshot(&file).map_err(|error| {
        format!(
            "failed to identify opened required instruction {}: {error}",
            path.display()
        )
    })?;
    Ok((
        file,
        MetadataIdentity {
            file: file_identity,
            len: metadata.len(),
            modified_nanos,
            changed_nanos: metadata_change_nanos(&metadata),
        },
    ))
}

#[cfg(any(unix, windows))]
fn open_instruction_without_following_links(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
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
fn open_instruction_without_following_links(_path: &Path) -> std::io::Result<File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "stable no-follow instruction reads are unavailable on this platform",
    ))
}

#[cfg(unix)]
fn metadata_change_nanos(metadata: &fs::Metadata) -> i128 {
    use std::os::unix::fs::MetadataExt;
    i128::from(metadata.ctime()) * 1_000_000_000 + i128::from(metadata.ctime_nsec())
}

#[cfg(windows)]
fn metadata_change_nanos(metadata: &fs::Metadata) -> i128 {
    use std::os::windows::fs::MetadataExt;
    i128::from(metadata.last_write_time())
}

#[cfg(not(any(unix, windows)))]
fn metadata_change_nanos(_metadata: &fs::Metadata) -> i128 {
    0
}

fn instruction_order_key(root: &Path, path: &Path) -> (usize, String, usize) {
    if !path.starts_with(root) {
        return (0, path.to_string_lossy().into_owned(), 0);
    }
    let parent = path.parent().unwrap_or(root);
    let depth = parent
        .strip_prefix(root)
        .map_or(0, |path| path.components().count());
    let local_rank = path
        .file_name()
        .and_then(|name| name.to_str())
        .map_or(0, |name| usize::from(name.contains(".local.")));
    (depth + 1, parent.to_string_lossy().into_owned(), local_rank)
}

fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        if fs::symlink_metadata(&current).is_ok() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Returns path scopes declared by a tool call. The executor still performs final
/// path validation; this function never treats command-text filename guesses as proof.
pub fn tool_instruction_scopes(
    project_root: &Path,
    tool_name: &str,
    arguments: &Value,
) -> Result<Vec<PathBuf>, String> {
    let declared = |key: &str| {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .map(|value| project_root.join(value))
    };
    let mut scopes = match tool_name {
        "read_file" | "list_directory" | "grep" => {
            vec![declared("path").unwrap_or_else(|| project_root.to_path_buf())]
        }
        "run_terminal" => {
            let mut paths = vec![declared("cwd").unwrap_or_else(|| project_root.to_path_buf())];
            let affected = arguments.get("affected_paths").and_then(Value::as_array);
            if let Some(affected) = affected {
                if affected.is_empty() {
                    return Err(
                        "run_terminal affected_paths cannot be empty for bounded instruction coverage"
                            .to_string(),
                    );
                }
                paths.extend(
                    affected
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|path| project_root.join(path)),
                );
            } else {
                let risk = arguments
                    .get("command")
                    .and_then(Value::as_str)
                    .map(classify_command)
                    .unwrap_or(ToolRisk::RequiresApproval);
                if risk != ToolRisk::Safe {
                    return Err(
                        "opaque run_terminal mutation requires explicit affected_paths for repository-wide instruction coverage"
                            .to_string(),
                    );
                }
            }
            paths
        }
        "apply_patch" => patch_scopes(project_root, arguments)?,
        "write_plan" => vec![project_root.join(".nib/plans")],
        _ => vec![project_root.to_path_buf()],
    };
    scopes.sort();
    scopes.dedup();
    Ok(scopes)
}

fn patch_scopes(project_root: &Path, arguments: &Value) -> Result<Vec<PathBuf>, String> {
    let patch = arguments
        .get("patch")
        .and_then(Value::as_str)
        .ok_or_else(|| "apply_patch instruction preflight requires a patch".to_string())?;
    let mut paths = Vec::new();
    for line in patch.lines() {
        let Some(raw) = line
            .strip_prefix("+++ ")
            .or_else(|| line.strip_prefix("--- "))
        else {
            continue;
        };
        let raw = raw.split_whitespace().next().unwrap_or_default();
        if raw == "/dev/null" {
            continue;
        }
        let relative = raw
            .strip_prefix("a/")
            .or_else(|| raw.strip_prefix("b/"))
            .unwrap_or(raw);
        if !relative.is_empty() {
            paths.push(project_root.join(relative));
        }
    }
    if paths.is_empty() {
        return Err("apply_patch does not declare a bounded affected path".to_string());
    }
    Ok(paths)
}

pub fn find_agents_md(start_path: &Path) -> Option<PathBuf> {
    let mut current = if start_path.is_file() {
        start_path.parent()?.to_path_buf()
    } else {
        start_path.to_path_buf()
    };
    loop {
        for filename in LOCAL_FILENAMES.iter().chain(BASE_FILENAMES) {
            let candidate = current.join(filename);
            let metadata = fs::symlink_metadata(&candidate).ok();
            if metadata.is_some_and(|metadata| {
                metadata.is_file() && !crate::fs_security::metadata_is_link_or_reparse(&metadata)
            }) {
                return Some(candidate);
            }
        }
        if !current.pop() {
            break;
        }
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("AGENTS.md"))
        .filter(|path| path.is_file())
}

pub fn load_agents_md(project_path: &Path) -> String {
    InstructionResolver::new(project_path)
        .and_then(|mut resolver| resolver.resolve_for_scopes([project_path]))
        .map(|instructions| instructions.render())
        .unwrap_or_else(|error| format!("# Error loading project instructions: {error}"))
}

pub fn format_context_for_prompt(project_path: &Path, task: Option<&str>) -> String {
    let mut parts = vec![format!(
        "## Project Agent Guidelines\n{}",
        load_agents_md(project_path)
    )];
    if let Some(task) = task {
        parts.push(format!("## Current Task\n{task}"));
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn resolves_root_nested_and_local_precedence_then_refreshes() {
        let home = tempdir().expect("home");
        let project = tempdir().expect("project");
        fs::write(home.path().join("AGENTS.md"), "global rule").expect("global");
        fs::write(project.path().join("AGENTS.md"), "root base").expect("root");
        fs::write(project.path().join("AGENTS.local.md"), "root local").expect("local");
        fs::create_dir_all(project.path().join("src/nested")).expect("nested");
        fs::write(project.path().join("src/CLAUDE.md"), "nested base").expect("nested base");

        let mut resolver = InstructionResolver::new(project.path()).expect("resolver");
        resolver.home = Some(home.path().to_path_buf());
        let first = resolver
            .resolve_for_scopes([project.path().join("src/nested/file.rs")])
            .expect("first resolution");
        let rendered = first.render();
        let markers = ["global rule", "root base", "root local", "nested base"];
        let positions = markers.map(|marker| rendered.find(marker).expect("marker"));
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        let reads = resolver.physical_reads();
        let unchanged = resolver
            .resolve_for_scopes([project.path().join("src/nested/file.rs")])
            .expect("cached resolution");
        assert_eq!(first.identity, unchanged.identity);
        assert_eq!(resolver.physical_reads(), reads);

        fs::write(
            project.path().join("src/CLAUDE.md"),
            "nested changed content",
        )
        .expect("change nested");
        let refreshed = resolver
            .resolve_for_scopes([project.path().join("src/nested/file.rs")])
            .expect("refreshed resolution");
        assert_ne!(first.identity, refreshed.identity);
        assert!(refreshed.render().contains("nested changed content"));
        assert_eq!(resolver.physical_reads(), reads + 1);
    }

    #[test]
    fn multi_scope_resolution_keeps_both_nested_rules() {
        let project = tempdir().expect("project");
        for directory in ["frontend", "backend"] {
            fs::create_dir(project.path().join(directory)).expect("scope");
            fs::write(
                project.path().join(directory).join("AGENTS.md"),
                format!("{directory} rule"),
            )
            .expect("instruction");
        }
        let mut resolver = InstructionResolver::new(project.path()).expect("resolver");
        resolver.home = None;
        let resolved = resolver
            .resolve_for_scopes([
                project.path().join("frontend/app.rs"),
                project.path().join("backend/api.rs"),
            ])
            .expect("multi scope");
        let rendered = resolved.render();
        assert!(rendered.contains("frontend rule"));
        assert!(rendered.contains("backend rule"));
        assert!(rendered.contains("frontend/**"));
        assert!(rendered.contains("backend/**"));
    }

    #[test]
    fn replacement_with_identical_content_is_revalidated_by_file_identity() {
        let project = tempdir().expect("project");
        let instructions = project.path().join("AGENTS.md");
        fs::write(&instructions, "stable rule").expect("instructions");
        let mut resolver = InstructionResolver::new(project.path()).expect("resolver");
        resolver.home = None;
        let first = resolver
            .resolve_for_scopes([project.path()])
            .expect("first resolution");
        let first_reads = resolver.physical_reads();

        let displaced = project.path().join("AGENTS.previous.md");
        fs::rename(&instructions, &displaced).expect("displace prior inode");
        fs::write(&instructions, "stable rule").expect("replace instructions");
        let second = resolver
            .resolve_for_scopes([project.path()])
            .expect("replacement resolution");

        assert_eq!(
            first.identity, second.identity,
            "content identity is stable"
        );
        assert_eq!(resolver.physical_reads(), first_reads + 1);
    }

    #[test]
    fn linked_and_oversized_required_instructions_fail_closed() {
        let project = tempdir().expect("project");
        fs::write(
            project.path().join("AGENTS.md"),
            vec![b'x'; MAX_INSTRUCTION_FILE_BYTES as usize + 1],
        )
        .expect("oversized");
        let mut resolver = InstructionResolver::new(project.path()).expect("resolver");
        resolver.home = None;
        let error = resolver
            .resolve_for_scopes([project.path()])
            .expect_err("oversized instructions fail");
        assert!(error.contains("exceeds"));

        fs::remove_file(project.path().join("AGENTS.md")).expect("remove oversized");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("outside.md", project.path().join("AGENTS.md"))
                .expect("link");
            let error = resolver
                .resolve_for_scopes([project.path()])
                .expect_err("linked instructions fail");
            assert!(error.contains("regular local file"));
        }
    }

    #[test]
    fn patch_scope_uses_headers_and_never_command_text_guessing() {
        let project = Path::new("/project");
        let scopes = tool_instruction_scopes(
            project,
            "apply_patch",
            &serde_json::json!({"patch": "--- a/src/a.rs\n+++ b/src/a.rs\n"}),
        )
        .expect("patch scope");
        assert_eq!(scopes, vec![project.join("src/a.rs")]);
        let terminal = tool_instruction_scopes(
            project,
            "run_terminal",
            &serde_json::json!({
                "command": "opaque helper",
                "cwd": "src",
                "affected_paths": ["nested/secret.rs"]
            }),
        )
        .expect("terminal scope");
        assert_eq!(
            terminal,
            vec![project.join("nested/secret.rs"), project.join("src")]
        );
        let safe = tool_instruction_scopes(
            project,
            "run_terminal",
            &serde_json::json!({"command": "cargo check", "cwd": "src"}),
        )
        .expect("classified safe command may use its cwd scope");
        assert_eq!(safe, vec![project.join("src")]);
        let error = tool_instruction_scopes(
            project,
            "run_terminal",
            &serde_json::json!({"command": "cat nested/secret.rs", "cwd": "src"}),
        )
        .expect_err("command text cannot prove instruction scope");
        assert!(error.contains("requires explicit affected_paths"));
    }
}
