//! Workspace profile resolution and isolated runtime paths.

use crate::config::{ProfileConfig, ProfilesConfig};
use crate::session::memory::MemoryStore;
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

const MAX_ENV_FILE_BYTES: u64 = 1_048_576;
const MAX_ENV_ENTRIES: usize = 256;
const MAX_ENV_KEY_BYTES: usize = 128;
const MAX_ENV_VALUE_BYTES: usize = 65_536;

mod migration;

#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("profile configuration is invalid: {0}")]
    Invalid(String),
    #[error("profile path error: {0}")]
    Io(#[from] std::io::Error),
    #[error("legacy state migration failed: {0}")]
    Migration(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    id: String,
    root_path: PathBuf,
    custom_env: HashMap<String, String>,
    active_skills: Vec<String>,
    skill_paths: Vec<PathBuf>,
    state_dir: PathBuf,
    memory_path: PathBuf,
    context_path: PathBuf,
    sessions_dir: PathBuf,
    daemon_dir: PathBuf,
    managed_skills_dir: PathBuf,
}

impl Profile {
    pub fn from_config(base: &Path, config: &ProfileConfig) -> Result<Self, ProfileError> {
        validate_id(&config.id)?;
        if config.root.as_os_str().is_empty() {
            return Err(ProfileError::Invalid(format!(
                "profile {} has an empty root",
                config.id
            )));
        }

        let root_candidate = if config.root.is_absolute() {
            config.root.clone()
        } else {
            base.join(&config.root)
        };
        let root_path = root_candidate.canonicalize()?;
        if !root_path.is_dir() {
            return Err(ProfileError::Invalid(format!(
                "profile {} root is not a directory: {}",
                config.id,
                root_path.display()
            )));
        }

        let state_relative = config
            .state_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(".nib").join("profiles").join(&config.id));
        validate_relative_path(&state_relative, "state_dir")?;
        let state_dir = resolve_scoped_path(&root_path, &state_relative, "state_dir")?;

        let custom_env = match &config.env_file {
            Some(env_file) => {
                let env_path = resolve_scoped_path(&root_path, env_file, "env_file")?;
                load_env_file(&env_path)?
            }
            None => HashMap::new(),
        };

        let mut skill_paths = Vec::with_capacity(config.skill_paths.len());
        for path in &config.skill_paths {
            skill_paths.push(resolve_scoped_path(&root_path, path, "skill_paths")?);
        }
        for skill in &config.active_skills {
            let mut components = Path::new(skill).components();
            if skill.trim().is_empty()
                || !matches!(components.next(), Some(Component::Normal(_)))
                || components.next().is_some()
            {
                return Err(ProfileError::Invalid(format!(
                    "profile {} has an invalid active skill id: {skill}",
                    config.id
                )));
            }
        }

        Ok(Self {
            id: config.id.clone(),
            root_path,
            custom_env,
            active_skills: config.active_skills.clone(),
            skill_paths,
            memory_path: state_dir.join("memory.json"),
            context_path: state_dir.join("context.json"),
            sessions_dir: state_dir.join("sessions"),
            daemon_dir: state_dir.join("daemons"),
            managed_skills_dir: state_dir.join("managed-skills"),
            state_dir,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    pub fn custom_env(&self) -> &HashMap<String, String> {
        &self.custom_env
    }

    pub fn active_skills(&self) -> &[String] {
        &self.active_skills
    }

    pub fn skill_paths(&self) -> &[PathBuf] {
        &self.skill_paths
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn memory_path(&self) -> &Path {
        &self.memory_path
    }

    pub fn context_path(&self) -> &Path {
        &self.context_path
    }

    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    pub fn daemon_dir(&self) -> &Path {
        &self.daemon_dir
    }

    pub fn managed_skills_dir(&self) -> &Path {
        &self.managed_skills_dir
    }

    pub fn memory_store(&self) -> MemoryStore {
        MemoryStore::at_path(self.memory_path.clone())
    }

    pub fn ensure_state_dirs(&self) -> Result<(), ProfileError> {
        let canonical_state = crate::fs_security::ensure_directory_without_symlinks(
            &self.state_dir,
        )
        .map_err(|error| {
            ProfileError::Invalid(format!(
                "profile {} state directory is unsafe: {error}",
                self.id
            ))
        })?;
        if !canonical_state.starts_with(&self.root_path) {
            return Err(ProfileError::Invalid(format!(
                "profile {} state directory escapes its workspace: {}",
                self.id,
                canonical_state.display()
            )));
        }
        for directory in [
            &self.sessions_dir,
            &self.daemon_dir,
            &self.managed_skills_dir,
        ] {
            let canonical = crate::fs_security::ensure_directory_without_symlinks(directory)
                .map_err(|error| {
                    ProfileError::Invalid(format!(
                        "profile {} state child escapes or is unsafe: {} ({error})",
                        self.id,
                        directory.display()
                    ))
                })?;
            if !canonical.starts_with(&canonical_state) {
                return Err(ProfileError::Invalid(format!(
                    "profile {} state child escapes its state directory: {}",
                    self.id,
                    directory.display()
                )));
            }
        }
        for file in [&self.memory_path, &self.context_path] {
            if std::fs::symlink_metadata(file)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                return Err(ProfileError::Invalid(format!(
                    "profile {} state file must not be a symlink: {}",
                    self.id,
                    file.display()
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ProfileRegistry {
    default_id: String,
    profiles: BTreeMap<String, Profile>,
}

impl ProfileRegistry {
    pub fn load(base: &Path, config: &ProfilesConfig) -> Result<Self, ProfileError> {
        let entries = if config.active.is_empty() {
            vec![ProfileConfig {
                id: config.default.clone(),
                root: PathBuf::from("."),
                ..ProfileConfig::default()
            }]
        } else {
            config.active.clone()
        };

        let mut profiles = BTreeMap::new();
        let mut state_owners = BTreeMap::new();
        for entry in entries {
            let profile = Profile::from_config(base, &entry)?;
            if let Some(existing) =
                state_owners.insert(profile.state_dir().to_path_buf(), profile.id().to_string())
            {
                return Err(ProfileError::Invalid(format!(
                    "profiles {existing} and {} share state directory {}",
                    profile.id(),
                    profile.state_dir().display()
                )));
            }
            if profiles.insert(entry.id.clone(), profile).is_some() {
                return Err(ProfileError::Invalid(format!(
                    "duplicate profile id: {}",
                    entry.id
                )));
            }
        }
        if !profiles.contains_key(&config.default) {
            return Err(ProfileError::Invalid(format!(
                "default profile does not exist: {}",
                config.default
            )));
        }

        let registry = Self {
            default_id: config.default.clone(),
            profiles,
        };
        migration::migrate_legacy_state(base, registry.default_profile())?;
        Ok(registry)
    }

    pub fn default_profile(&self) -> &Profile {
        self.profiles
            .get(&self.default_id)
            .expect("validated profile registry has a default")
    }

    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.profiles.get(id)
    }

    pub fn all(&self) -> impl Iterator<Item = &Profile> {
        self.profiles.values()
    }

    pub fn for_workspace(&self, workspace: &Path) -> Option<&Profile> {
        let workspace = workspace.canonicalize().ok()?;
        let default = self.default_profile();
        if default.root_path == workspace {
            return Some(default);
        }
        self.profiles
            .values()
            .find(|profile| profile.root_path == workspace)
    }
}

fn validate_id(id: &str) -> Result<(), ProfileError> {
    if id.is_empty()
        || !id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(ProfileError::Invalid(format!(
            "profile id contains unsupported characters: {id}"
        )));
    }
    Ok(())
}

fn validate_relative_path(path: &Path, field: &str) -> Result<(), ProfileError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ProfileError::Invalid(format!(
            "{field} must be a scoped relative path: {}",
            path.display()
        )));
    }
    Ok(())
}

fn resolve_scoped_path(root: &Path, relative: &Path, field: &str) -> Result<PathBuf, ProfileError> {
    validate_relative_path(relative, field)?;
    let mut resolved = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            continue;
        };
        let candidate = resolved.join(part);
        match std::fs::symlink_metadata(&candidate) {
            Ok(_) => {
                let canonical = candidate.canonicalize()?;
                if !canonical.starts_with(root) {
                    return Err(ProfileError::Invalid(format!(
                        "{field} escapes the profile workspace: {}",
                        relative.display()
                    )));
                }
                resolved = canonical;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                resolved = candidate;
            }
            Err(error) => return Err(ProfileError::Io(error)),
        }
    }
    Ok(resolved)
}

fn load_env_file(path: &Path) -> Result<HashMap<String, String>, ProfileError> {
    let metadata = std::fs::symlink_metadata(path)?;
    validate_env_file_metadata(path, &metadata)?;
    let file = std::fs::File::open(path)?;
    validate_env_file_metadata(path, &file.metadata()?)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_ENV_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_ENV_FILE_BYTES {
        return Err(env_file_too_large(path));
    }
    validate_env_file_metadata(path, &std::fs::symlink_metadata(path)?)?;
    let contents = String::from_utf8(bytes).map_err(|error| {
        ProfileError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    })?;
    let mut env = HashMap::new();
    for (index, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (key, raw_value) = line.split_once('=').ok_or_else(|| {
            ProfileError::Invalid(format!(
                "invalid environment entry at {}:{}",
                path.display(),
                index + 1
            ))
        })?;
        let key = key.trim();
        if key.len() > MAX_ENV_KEY_BYTES {
            return Err(ProfileError::Invalid(format!(
                "environment key exceeds the {MAX_ENV_KEY_BYTES}-byte limit at {}:{}",
                path.display(),
                index + 1
            )));
        }
        let mut chars = key.chars();
        let valid_first = chars
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_');
        if !valid_first || !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
            return Err(ProfileError::Invalid(format!(
                "invalid environment key at {}:{}",
                path.display(),
                index + 1
            )));
        }
        let value = raw_value.trim();
        let value = if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            &value[1..value.len() - 1]
        } else {
            value
        };
        if value.len() > MAX_ENV_VALUE_BYTES {
            return Err(ProfileError::Invalid(format!(
                "environment value exceeds the {MAX_ENV_VALUE_BYTES}-byte limit at {}:{}",
                path.display(),
                index + 1
            )));
        }
        env.insert(key.to_string(), value.to_string());
        if env.len() > MAX_ENV_ENTRIES {
            return Err(ProfileError::Invalid(format!(
                "environment file contains more than {MAX_ENV_ENTRIES} entries: {}",
                path.display()
            )));
        }
    }
    Ok(env)
}

fn validate_env_file_metadata(
    path: &Path,
    metadata: &std::fs::Metadata,
) -> Result<(), ProfileError> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ProfileError::Invalid(format!(
            "environment path must be a regular local file: {}",
            path.display()
        )));
    }
    if metadata.len() > MAX_ENV_FILE_BYTES {
        return Err(env_file_too_large(path));
    }
    Ok(())
}

fn env_file_too_large(path: &Path) -> ProfileError {
    ProfileError::Invalid(format!(
        "environment file exceeds the {MAX_ENV_FILE_BYTES}-byte limit: {}",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProfilesConfig;
    use tempfile::tempdir;

    #[test]
    fn profiles_isolate_environment_skills_and_memory() {
        let base = tempdir().expect("base");
        let first = base.path().join("first");
        let second = base.path().join("second");
        std::fs::create_dir_all(&first).expect("first workspace");
        std::fs::create_dir_all(&second).expect("second workspace");
        std::fs::write(first.join(".env.nib"), "COLOR=green\n").expect("first env");
        std::fs::write(second.join(".env.nib"), "COLOR=blue\n").expect("second env");

        let config = ProfilesConfig {
            default: "first".to_string(),
            active: vec![
                ProfileConfig {
                    id: "first".to_string(),
                    root: first.clone(),
                    env_file: Some(PathBuf::from(".env.nib")),
                    active_skills: vec!["rust".to_string()],
                    ..ProfileConfig::default()
                },
                ProfileConfig {
                    id: "second".to_string(),
                    root: second.clone(),
                    env_file: Some(PathBuf::from(".env.nib")),
                    active_skills: vec!["docs".to_string()],
                    ..ProfileConfig::default()
                },
            ],
        };

        let registry = ProfileRegistry::load(base.path(), &config).expect("profiles");
        let first_profile = registry.get("first").expect("first profile");
        let second_profile = registry.get("second").expect("second profile");
        first_profile.ensure_state_dirs().expect("first state");
        second_profile.ensure_state_dirs().expect("second state");

        assert_eq!(first_profile.custom_env().get("COLOR").unwrap(), "green");
        assert_eq!(second_profile.custom_env().get("COLOR").unwrap(), "blue");
        assert_eq!(first_profile.active_skills(), &["rust"]);
        assert_eq!(second_profile.active_skills(), &["docs"]);
        assert_ne!(first_profile.memory_path(), second_profile.memory_path());
        assert_ne!(
            first_profile.managed_skills_dir(),
            second_profile.managed_skills_dir()
        );
        assert!(first_profile.managed_skills_dir().is_dir());
        assert!(second_profile.managed_skills_dir().is_dir());

        first_profile
            .memory_store()
            .set_environment("build", "task check")
            .expect("persist first memory");
        assert_eq!(
            first_profile.memory_store().environment("build").as_deref(),
            Some("task check")
        );
        assert_eq!(second_profile.memory_store().environment("build"), None);

        let reloaded = ProfileRegistry::load(base.path(), &config).expect("reload profiles");
        assert_eq!(
            reloaded
                .get("first")
                .unwrap()
                .memory_store()
                .environment("build")
                .as_deref(),
            Some("task check")
        );
    }

    #[test]
    fn profile_environment_is_bounded() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("large.env");
        std::fs::write(&path, "x".repeat((MAX_ENV_FILE_BYTES + 1) as usize))
            .expect("large environment fixture");

        let error = load_env_file(&path).expect_err("oversized environment rejected");
        assert!(error.to_string().contains("exceeds"));
    }

    #[test]
    fn rejects_profile_state_path_escape() {
        let base = tempdir().expect("base");
        let config = ProfileConfig {
            id: "unsafe".to_string(),
            root: base.path().to_path_buf(),
            state_dir: Some(PathBuf::from("../shared")),
            ..ProfileConfig::default()
        };

        assert!(Profile::from_config(base.path(), &config).is_err());
    }

    #[test]
    fn rejects_shared_profile_state_directory() {
        let base = tempdir().expect("base");
        let shared = Some(PathBuf::from(".nib/shared"));
        let config = ProfilesConfig {
            default: "first".to_string(),
            active: vec![
                ProfileConfig {
                    id: "first".to_string(),
                    root: PathBuf::from("."),
                    state_dir: shared.clone(),
                    ..ProfileConfig::default()
                },
                ProfileConfig {
                    id: "second".to_string(),
                    root: PathBuf::from("."),
                    state_dir: shared,
                    ..ProfileConfig::default()
                },
            ],
        };

        let error = ProfileRegistry::load(base.path(), &config)
            .expect_err("shared state must violate profile isolation")
            .to_string();
        assert!(error.contains("share state directory"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_profile_state_child_symlink_escape() {
        use std::os::unix::fs::symlink;

        let base = tempdir().expect("base");
        let outside = tempdir().expect("outside");
        let profile = Profile::from_config(
            base.path(),
            &ProfileConfig {
                id: "isolated".to_string(),
                root: PathBuf::from("."),
                ..ProfileConfig::default()
            },
        )
        .expect("profile");
        std::fs::create_dir_all(profile.state_dir()).expect("profile state");
        symlink(outside.path(), profile.sessions_dir()).expect("sessions symlink");

        let error = profile
            .ensure_state_dirs()
            .expect_err("state child symlink must be rejected")
            .to_string();
        assert!(error.contains("state child escapes"));
    }

    #[cfg(unix)]
    #[test]
    fn state_creation_rechecks_ancestors_after_profile_construction() {
        use std::os::unix::fs::symlink;

        let base = tempdir().expect("base");
        let outside = tempdir().expect("outside");
        let profile = Profile::from_config(
            base.path(),
            &ProfileConfig {
                id: "raced".to_string(),
                root: PathBuf::from("."),
                ..ProfileConfig::default()
            },
        )
        .expect("construct profile before state exists");
        symlink(outside.path(), base.path().join(".nib")).expect("replace state ancestor");

        assert!(profile.ensure_state_dirs().is_err());
        assert!(!outside.path().join("profiles/raced").exists());
    }
}
