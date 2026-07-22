use super::{Profile, ProfileError};
use crate::session::memory::MemoryStoreData;
use crate::session::Session;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MIGRATION_VERSION: u32 = 1;
const MARKER_FILE: &str = ".legacy-state-migration-v1.json";
const MAX_LEGACY_SESSION_FILES: usize = 10_000;
const MAX_LEGACY_SESSION_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LEGACY_SESSION_AGGREGATE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_LEGACY_MEMORY_BYTES: u64 = 8 * 1024 * 1024;
const MAX_MIGRATION_MARKER_BYTES: u64 = 64 * 1024;

#[derive(Debug, Serialize, Deserialize)]
struct MigrationMarker {
    version: u32,
    profile_id: String,
    session_count: usize,
    memory_copied: bool,
}

struct CopyCandidate {
    source: PathBuf,
    destination: PathBuf,
    bytes: Vec<u8>,
}

pub(super) fn migrate_legacy_state(base: &Path, profile: &Profile) -> Result<(), ProfileError> {
    profile.ensure_state_dirs()?;
    let marker_path = profile.state_dir().join(MARKER_FILE);
    if path_exists(&marker_path)? {
        validate_marker(&marker_path, profile.id())?;
        return Ok(());
    }

    let base = base.canonicalize()?;
    let legacy_state = base.join(".nib");
    let mut candidates = legacy_session_candidates(&legacy_state, profile)?;
    let memory_copied = if let Some(memory) = legacy_memory_candidate(&legacy_state, profile)? {
        candidates.push(memory);
        true
    } else {
        false
    };

    // Detect every conflict before copying anything. A previous interrupted migration
    // may have left identical complete files, which are safe to resume.
    for candidate in &candidates {
        validate_existing_destination(candidate)?;
    }
    for candidate in &candidates {
        atomic_create_or_verify(candidate)?;
    }

    let marker = MigrationMarker {
        version: MIGRATION_VERSION,
        profile_id: profile.id().to_string(),
        session_count: candidates.len() - usize::from(memory_copied),
        memory_copied,
    };
    let marker_bytes = serde_json::to_vec_pretty(&marker)
        .map_err(|error| migration_error(format!("failed to serialize marker: {error}")))?;
    let _ = atomic_create_bytes(&marker_path, &marker_bytes)?;
    validate_marker(&marker_path, profile.id())
}

fn legacy_session_candidates(
    legacy_state: &Path,
    profile: &Profile,
) -> Result<Vec<CopyCandidate>, ProfileError> {
    let source_dir = legacy_state.join("sessions");
    let metadata = match fs::symlink_metadata(&source_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(migration_error(format!(
            "legacy sessions path must be a local directory: {}",
            source_dir.display()
        )));
    }
    crate::fs_security::verify_directory_without_symlinks(&source_dir)?;
    ensure_legacy_path_is_local(legacy_state, &source_dir, "legacy sessions")?;

    let mut sources = Vec::new();
    for entry in fs::read_dir(&source_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        if sources.len() >= MAX_LEGACY_SESSION_FILES {
            return Err(migration_error(format!(
                "legacy sessions exceed the {MAX_LEGACY_SESSION_FILES}-file migration limit"
            )));
        }
        sources.push(path);
    }
    sources.sort();

    let mut candidates = Vec::with_capacity(sources.len());
    let mut aggregate_bytes = 0u64;
    for source in sources {
        let file_name = source
            .file_name()
            .ok_or_else(|| migration_error("legacy session has no file name"))?
            .to_os_string();
        let expected_id = source
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                migration_error(format!(
                    "legacy session has an invalid file name: {}",
                    source.display()
                ))
            })?;
        let bytes = read_bounded_regular_file(&source, MAX_LEGACY_SESSION_BYTES, "legacy session")?;
        let source_bytes = bytes.len() as u64;
        aggregate_bytes = aggregate_bytes
            .checked_add(source_bytes)
            .ok_or_else(|| migration_error("legacy session aggregate size overflow"))?;
        if aggregate_bytes > MAX_LEGACY_SESSION_AGGREGATE_BYTES {
            return Err(migration_error(format!(
                "legacy sessions exceed the {MAX_LEGACY_SESSION_AGGREGATE_BYTES}-byte aggregate migration limit"
            )));
        }
        let raw: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
            migration_error(format!(
                "failed to parse legacy session {}: {error}",
                source.display()
            ))
        })?;
        let needs_index_migration = raw
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .chain(
                raw.get("events")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten(),
            )
            .any(|entry| entry.get("index").is_none());
        let mut session: Session = serde_json::from_slice(&bytes).map_err(|error| {
            migration_error(format!(
                "failed to parse legacy session {}: {error}",
                source.display()
            ))
        })?;
        if session.id != expected_id {
            return Err(migration_error(format!(
                "legacy session file {} contains id {}",
                source.display(),
                session.id
            )));
        }
        // Pre-profile sessions did not persist message/event indices. Serde fills
        // those fields with zero, so normalize their legacy order before applying
        // the current invariants and writing the migrated representation.
        if needs_index_migration {
            for (index, message) in session.messages.iter_mut().enumerate() {
                message.index = index;
            }
            for (index, event) in session.events.iter_mut().enumerate() {
                event.index = index;
            }
        }
        session.validate().map_err(|error| {
            migration_error(format!(
                "legacy session {} is invalid: {error}",
                source.display()
            ))
        })?;
        let migrated_bytes = if needs_index_migration {
            serde_json::to_vec_pretty(&session).map_err(|error| {
                migration_error(format!(
                    "failed to serialize legacy session {}: {error}",
                    source.display()
                ))
            })?
        } else {
            bytes
        };
        if migrated_bytes.len() as u64 > MAX_LEGACY_SESSION_BYTES {
            return Err(migration_error(format!(
                "migrated session {} exceeds the {MAX_LEGACY_SESSION_BYTES}-byte limit",
                source.display()
            )));
        }
        aggregate_bytes = aggregate_bytes
            .checked_sub(source_bytes)
            .and_then(|bytes| bytes.checked_add(migrated_bytes.len() as u64))
            .ok_or_else(|| migration_error("legacy session aggregate size overflow"))?;
        if aggregate_bytes > MAX_LEGACY_SESSION_AGGREGATE_BYTES {
            return Err(migration_error(format!(
                "migrated sessions exceed the {MAX_LEGACY_SESSION_AGGREGATE_BYTES}-byte aggregate migration limit"
            )));
        }
        candidates.push(CopyCandidate {
            source,
            destination: profile.sessions_dir().join(file_name),
            bytes: migrated_bytes,
        });
    }
    Ok(candidates)
}

fn legacy_memory_candidate(
    legacy_state: &Path,
    profile: &Profile,
) -> Result<Option<CopyCandidate>, ProfileError> {
    let source = legacy_state.join("memory.json");
    match fs::symlink_metadata(&source) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let bytes = read_bounded_regular_file(&source, MAX_LEGACY_MEMORY_BYTES, "legacy memory")?;
    ensure_legacy_path_is_local(legacy_state, &source, "legacy memory")?;
    serde_json::from_slice::<MemoryStoreData>(&bytes).map_err(|error| {
        migration_error(format!(
            "failed to parse legacy memory {}: {error}",
            source.display()
        ))
    })?;
    Ok(Some(CopyCandidate {
        source,
        destination: profile.memory_path().to_path_buf(),
        bytes,
    }))
}

fn validate_existing_destination(candidate: &CopyCandidate) -> Result<(), ProfileError> {
    let metadata = match fs::symlink_metadata(&candidate.destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(migration_error(format!(
            "migration destination must be a regular file: {}",
            candidate.destination.display()
        )));
    }
    if metadata.len() != candidate.bytes.len() as u64 {
        return Err(migration_error(format!(
            "migration conflict: {} differs from legacy source {}",
            candidate.destination.display(),
            candidate.source.display()
        )));
    }
    let existing = read_bounded_regular_file(
        &candidate.destination,
        candidate.bytes.len() as u64,
        "migration destination",
    )?;
    if existing != candidate.bytes {
        return Err(migration_error(format!(
            "migration conflict: {} differs from legacy source {}",
            candidate.destination.display(),
            candidate.source.display()
        )));
    }
    Ok(())
}

fn atomic_create_or_verify(candidate: &CopyCandidate) -> Result<(), ProfileError> {
    if atomic_create_bytes(&candidate.destination, &candidate.bytes)? {
        Ok(())
    } else {
        validate_existing_destination(candidate)
    }
}

fn atomic_create_bytes(path: &Path, bytes: &[u8]) -> Result<bool, ProfileError> {
    let parent = path.parent().ok_or_else(|| {
        migration_error(format!(
            "migration destination has no parent: {}",
            path.display()
        ))
    })?;
    crate::fs_security::ensure_directory_without_symlinks(parent)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".nib-migration-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file_mut().sync_all()?;
    match temporary.persist_noclobber(path) {
        Ok(_) => {
            sync_parent_directory(parent)?;
            Ok(true)
        }
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error.error.into()),
    }
}

fn validate_marker(path: &Path, profile_id: &str) -> Result<(), ProfileError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(migration_error(format!(
            "migration marker must be a regular file: {}",
            path.display()
        )));
    }
    let marker_bytes =
        read_bounded_regular_file(path, MAX_MIGRATION_MARKER_BYTES, "migration marker")?;
    let marker: MigrationMarker = serde_json::from_slice(&marker_bytes).map_err(|error| {
        migration_error(format!(
            "failed to parse migration marker {}: {error}",
            path.display()
        ))
    })?;
    if marker.version != MIGRATION_VERSION || marker.profile_id != profile_id {
        return Err(migration_error(format!(
            "migration marker {} does not match version {MIGRATION_VERSION} and profile {profile_id}",
            path.display()
        )));
    }
    Ok(())
}

fn read_bounded_regular_file(
    path: &Path,
    max_bytes: u64,
    label: &str,
) -> Result<Vec<u8>, ProfileError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(migration_error(format!(
            "{label} must be a regular file: {}",
            path.display()
        )));
    }
    if metadata.len() > max_bytes {
        return Err(migration_error(format!(
            "{label} {} exceeds the {max_bytes}-byte limit",
            path.display()
        )));
    }
    if let Some(parent) = path.parent() {
        crate::fs_security::verify_directory_without_symlinks(parent)?;
    }
    let file = fs::File::open(path)?;
    let opened_metadata = file.metadata()?;
    if !opened_metadata.is_file() || opened_metadata.len() > max_bytes {
        return Err(migration_error(format!(
            "{label} {} exceeds the {max_bytes}-byte limit or is not a regular file",
            path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(opened_metadata.len() as usize);
    file.take(max_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(migration_error(format!(
            "{label} {} exceeds the {max_bytes}-byte limit",
            path.display()
        )));
    }
    let post_metadata = fs::symlink_metadata(path)?;
    if post_metadata.file_type().is_symlink()
        || !post_metadata.is_file()
        || post_metadata.len() > max_bytes
    {
        return Err(migration_error(format!(
            "{label} {} changed or exceeds the {max_bytes}-byte limit",
            path.display()
        )));
    }
    if let Some(parent) = path.parent() {
        crate::fs_security::verify_directory_without_symlinks(parent)?;
    }
    Ok(bytes)
}

fn ensure_legacy_path_is_local(
    legacy_state: &Path,
    path: &Path,
    label: &str,
) -> Result<(), ProfileError> {
    let project_root = legacy_state
        .parent()
        .ok_or_else(|| migration_error("legacy state has no project parent"))?;
    let canonical = path.canonicalize()?;
    if !canonical.starts_with(project_root) {
        return Err(migration_error(format!(
            "{label} path escapes the project: {}",
            path.display()
        )));
    }
    Ok(())
}

fn path_exists(path: &Path) -> Result<bool, ProfileError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn migration_error(message: impl Into<String>) -> ProfileError {
    ProfileError::Migration(message.into())
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<(), ProfileError> {
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> Result<(), ProfileError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProfilesConfig;
    use crate::profile::ProfileRegistry;
    use tempfile::tempdir;

    #[test]
    fn migrates_legacy_state_once_without_removing_sources() {
        let root = tempdir().expect("root");
        let legacy_sessions = root.path().join(".nib/sessions");
        fs::create_dir_all(&legacy_sessions).expect("legacy sessions");
        let legacy_session = valid_session("legacy", "original");
        let legacy_session_path = legacy_sessions.join("legacy.json");
        fs::write(&legacy_session_path, &legacy_session).expect("legacy session");
        let legacy_memory = br#"{
  "environment": {"build": "task test"},
  "user": {"style": "concise"}
}"#;
        let legacy_memory_path = root.path().join(".nib/memory.json");
        fs::write(&legacy_memory_path, legacy_memory).expect("legacy memory");

        let registry = ProfileRegistry::load(root.path(), &ProfilesConfig::default())
            .expect("migrate legacy state");
        let profile = registry.default_profile();
        let destination_session = profile.sessions_dir().join("legacy.json");
        assert_eq!(
            fs::read(&destination_session).expect("migrated session"),
            legacy_session
        );
        assert_eq!(
            fs::read(profile.memory_path()).expect("migrated memory"),
            legacy_memory
        );
        assert_eq!(
            fs::read(&legacy_session_path).expect("preserved session"),
            legacy_session
        );
        assert_eq!(
            fs::read(&legacy_memory_path).expect("preserved memory"),
            legacy_memory
        );
        let marker_path = profile.state_dir().join(MARKER_FILE);
        validate_marker(&marker_path, profile.id()).expect("durable marker");

        let changed_legacy = valid_session("legacy", "changed after migration");
        fs::write(&legacy_session_path, changed_legacy).expect("change legacy source");
        ProfileRegistry::load(root.path(), &ProfilesConfig::default())
            .expect("marker makes migration one-time");
        assert_eq!(
            fs::read(destination_session).expect("unchanged destination"),
            legacy_session
        );
    }

    #[test]
    fn migration_rejects_destination_conflicts_without_copying() {
        let root = tempdir().expect("root");
        let legacy_sessions = root.path().join(".nib/sessions");
        let destination_sessions = root.path().join(".nib/profiles/default/sessions");
        fs::create_dir_all(&legacy_sessions).expect("legacy sessions");
        fs::create_dir_all(&destination_sessions).expect("destination sessions");
        let legacy_session = valid_session("conflict", "legacy");
        let destination_session = valid_session("conflict", "profile");
        fs::write(legacy_sessions.join("conflict.json"), &legacy_session).expect("legacy session");
        let destination_path = destination_sessions.join("conflict.json");
        fs::write(&destination_path, &destination_session).expect("profile session");
        let legacy_memory_path = root.path().join(".nib/memory.json");
        fs::write(&legacy_memory_path, br#"{"environment":{"key":"value"}}"#)
            .expect("legacy memory");

        let error = ProfileRegistry::load(root.path(), &ProfilesConfig::default())
            .expect_err("conflict must fail closed")
            .to_string();
        assert!(error.contains("migration conflict"), "{error}");
        assert_eq!(
            fs::read(&destination_path).expect("preserved destination"),
            destination_session
        );
        assert_eq!(
            fs::read(legacy_sessions.join("conflict.json")).expect("preserved source"),
            legacy_session
        );
        assert!(!root
            .path()
            .join(".nib/profiles/default/memory.json")
            .exists());
        assert!(!root
            .path()
            .join(".nib/profiles/default")
            .join(MARKER_FILE)
            .exists());
    }

    #[test]
    fn migration_validates_every_session_before_copying_or_marking() {
        let root = tempdir().expect("root");
        let legacy_sessions = root.path().join(".nib/sessions");
        fs::create_dir_all(&legacy_sessions).expect("legacy sessions");
        fs::write(
            legacy_sessions.join("a-valid.json"),
            valid_session("a-valid", "valid"),
        )
        .expect("valid legacy session");
        fs::write(
            legacy_sessions.join("z-invalid.json"),
            br#"{
  "id": "z-invalid",
  "messages": [
    {"index": 0, "role": "user", "content": "one"},
    {"index": 1, "role": "user", "content": "two"}
  ]
}"#,
        )
        .expect("invalid legacy session");

        let error = ProfileRegistry::load(root.path(), &ProfilesConfig::default())
            .expect_err("invalid role sequence must stop migration")
            .to_string();
        assert!(error.contains("role transition"), "{error}");
        let profile_state = root.path().join(".nib/profiles/default");
        assert!(!profile_state.join("sessions/a-valid.json").exists());
        assert!(!profile_state.join(MARKER_FILE).exists());
        assert!(legacy_sessions.join("a-valid.json").exists());
        assert!(legacy_sessions.join("z-invalid.json").exists());
    }

    #[test]
    fn migration_normalizes_pre_index_session_messages() {
        let root = tempdir().expect("root");
        let legacy_sessions = root.path().join(".nib/sessions");
        fs::create_dir_all(&legacy_sessions).expect("legacy sessions");
        let source = br#"{
  "id": "pre-index",
  "messages": [
    {"role": "user", "content": "question"},
    {"role": "assistant", "content": "answer"}
  ],
  "tool_calls": []
}"#;
        let source_path = legacy_sessions.join("pre-index.json");
        fs::write(&source_path, source).expect("legacy session");

        let registry = ProfileRegistry::load(root.path(), &ProfilesConfig::default())
            .expect("migrate pre-index state");
        let destination = registry
            .default_profile()
            .sessions_dir()
            .join("pre-index.json");
        let migrated: Session =
            serde_json::from_slice(&fs::read(destination).expect("read migrated session"))
                .expect("parse migrated session");

        migrated.validate().expect("strict current invariants");
        assert_eq!(migrated.messages[0].index, 0);
        assert_eq!(migrated.messages[1].index, 1);
        assert_eq!(fs::read(source_path).expect("preserved source"), source);
    }

    #[test]
    fn migration_rejects_oversized_legacy_state_without_copying() {
        let root = tempdir().expect("root");
        let legacy_sessions = root.path().join(".nib/sessions");
        fs::create_dir_all(&legacy_sessions).expect("legacy sessions");
        let source = legacy_sessions.join("oversized.json");
        fs::File::create(&source)
            .and_then(|file| file.set_len(MAX_LEGACY_SESSION_BYTES + 1))
            .expect("oversized sparse session");

        let error = ProfileRegistry::load(root.path(), &ProfilesConfig::default())
            .expect_err("oversized state must fail closed")
            .to_string();

        assert!(error.contains("exceeds"), "{error}");
        assert!(source.exists(), "legacy source remains intact");
        assert!(!root
            .path()
            .join(".nib/profiles/default/sessions/oversized.json")
            .exists());
        assert!(!root
            .path()
            .join(".nib/profiles/default")
            .join(MARKER_FILE)
            .exists());
    }

    fn valid_session(id: &str, content: &str) -> Vec<u8> {
        serde_json::to_vec_pretty(&serde_json::json!({
            "id": id,
            "messages": [{"index": 0, "role": "user", "content": content}],
            "tool_calls": []
        }))
        .expect("serialize session")
    }
}
