//! T082 phase 2: configuration layers and the project-preference boundary.
//!
//! A project's effective configuration is built from, in rising precedence:
//!
//! 1. the user configuration, `~/.nib/config.toml` (`NIB_HOME` overrides
//!    the directory), which `nib auth` writes;
//! 2. the project's preferences, `<project>/.nib/config.toml`;
//! 3. the user's settings for this project,
//!    `~/.nib/projects/<key>/config.toml`.
//!
//! Provider credentials in the environment keep their precedence where they
//! are read. Project preferences choose a provider and model, skills and
//! agent behavior. They cannot set credentials, endpoints, the transport,
//! approvals, the sandbox level or trust: a cloned repository must not be
//! able to send the user's credentials elsewhere or relax safety. Those
//! values live only in the user home, and project preferences that set
//! them are rejected with a diagnostic.
//!
//! Configuration written by older versions kept those values in the
//! project. On first load such a local (untracked) file is migrated: the
//! original is kept under `<project>/.nib/legacy-<time>/`, its values move
//! to this project's user settings without overwriting any, and the
//! project file is rewritten as preferences only.

use super::*;
use crate::daemons::state::{FileExpectation, StableDirectory};
use crate::paths::NibPaths;
use std::collections::BTreeMap;

/// The settings file inside a project's user directory.
const PROJECT_SETTINGS_FILE: &str = "config.toml";
/// The identity file inside a project's user directory.
const PROJECT_IDENTITY_FILE: &str = "project.toml";
/// The user configuration table that holds permission rules (T080).
const PERMISSIONS_TABLE: &str = "permissions";
/// Prefix of the directories that keep a migrated project's originals.
pub(crate) const LEGACY_DIRECTORY_PREFIX: &str = "legacy-";
const LEGACY_TEMPORARY_PREFIX: &str = ".legacy-config.tmp-";
const IDENTITY_TEMPORARY_PREFIX: &str = ".project.toml.tmp-";
/// Bounded scan of a project's nib directory for an existing backup.
const MAX_LEGACY_SCAN_ENTRIES: usize = 10_000;

/// Provider fields only the user may set.
pub(crate) const USER_ONLY_PROVIDER_KEYS: [&str; 4] = ["api_key", "api_keys", "base_url", "api"];

/// Credentials, endpoint and transport the user configured for one provider
/// in one project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_keys: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<LlmApiMode>,
}

impl ProviderSettings {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    fn has_credentials(&self) -> bool {
        self.api_key.is_some() || self.api_keys.is_some()
    }

    /// Drops values that mean the same as an absent field.
    fn without_defaults(mut self) -> Self {
        if self.api_keys.as_ref().is_some_and(Vec::is_empty) {
            self.api_keys = None;
        }
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectLlmSettings {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ProviderSettings>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectApprovalSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectExecutionSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectWorkspaceSettings {
    /// The interactive grant to work in this project directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed: Option<bool>,
}

/// When the project's own configuration was migrated, and where its
/// original is kept.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectMigrationRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_config: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
}

/// The user's settings for one project: what a project's own preferences
/// must not set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectSettings {
    #[serde(default, skip_serializing_if = "is_default")]
    pub llm: ProjectLlmSettings,
    #[serde(default, skip_serializing_if = "is_default")]
    pub approvals: ProjectApprovalSettings,
    #[serde(default, skip_serializing_if = "is_default")]
    pub execution: ProjectExecutionSettings,
    #[serde(default, skip_serializing_if = "is_default")]
    pub workspace: ProjectWorkspaceSettings,
    #[serde(default, skip_serializing_if = "is_default")]
    pub migration: ProjectMigrationRecord,
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

impl ProjectSettings {
    /// Whether this project keeps its own credentials for `provider`.
    pub fn has_provider_credentials(&self, provider: &str) -> bool {
        self.llm
            .providers
            .get(provider)
            .is_some_and(ProviderSettings::has_credentials)
    }

    /// Every value these settings hold, as `(key, value)` pairs, migration
    /// record excluded.
    fn entries(&self) -> Vec<(String, toml::Value)> {
        let mut entries = Vec::new();
        for (name, provider) in &self.llm.providers {
            let table =
                toml::Value::try_from(provider).unwrap_or(toml::Value::Table(Default::default()));
            if let toml::Value::Table(table) = table {
                for (key, value) in table {
                    entries.push((format!("llm.providers.{name}.{key}"), value));
                }
            }
        }
        if let Some(mode) = &self.approvals.mode {
            entries.push(("approvals.mode".to_string(), mode.clone().into()));
        }
        if let Some(sandbox) = &self.execution.sandbox {
            entries.push(("execution.sandbox".to_string(), sandbox.clone().into()));
        }
        if let Some(allowed) = self.workspace.allowed {
            entries.push(("workspace.allowed".to_string(), allowed.into()));
        }
        entries
    }

    /// Keys whose value in `incoming` differs from a value already set here.
    fn conflicts_with(&self, incoming: &Self) -> Vec<String> {
        let current: BTreeMap<String, toml::Value> = self.entries().into_iter().collect();
        incoming
            .entries()
            .into_iter()
            .filter(|(key, value)| current.get(key).is_some_and(|existing| existing != value))
            .map(|(key, _)| key)
            .collect()
    }

    /// Whether every value of `incoming` is already set here.
    fn contains(&self, incoming: &Self) -> bool {
        let current: BTreeMap<String, toml::Value> = self.entries().into_iter().collect();
        incoming
            .entries()
            .iter()
            .all(|(key, value)| current.get(key) == Some(value))
    }

    /// Adds the values of `incoming` that are not set here. Callers check
    /// [`Self::conflicts_with`] first; existing values are never replaced.
    fn add_missing(&mut self, incoming: &Self) {
        for (name, provider) in &incoming.llm.providers {
            let entry = self.llm.providers.entry(name.clone()).or_default();
            entry.api_key = entry.api_key.take().or_else(|| provider.api_key.clone());
            entry.api_keys = entry.api_keys.take().or_else(|| provider.api_keys.clone());
            entry.base_url = entry.base_url.take().or_else(|| provider.base_url.clone());
            entry.api = entry.api.or(provider.api);
        }
        self.approvals.mode = self
            .approvals
            .mode
            .take()
            .or_else(|| incoming.approvals.mode.clone());
        self.execution.sandbox = self
            .execution
            .sandbox
            .take()
            .or_else(|| incoming.execution.sandbox.clone());
        self.workspace.allowed = self.workspace.allowed.or(incoming.workspace.allowed);
    }

    /// Applies these settings over a configuration. A provider that only
    /// these settings name gets the registry's default model.
    pub(crate) fn apply(&self, config: &mut NibConfig) {
        for (name, provider) in &self.llm.providers {
            let entry = config
                .llm
                .providers
                .entry(name.clone())
                .or_insert_with(|| ProviderEntry {
                    model: crate::llm::registry::provider_descriptor(name)
                        .map(|descriptor| descriptor.default_model().to_string())
                        .unwrap_or_default(),
                    ..ProviderEntry::default()
                });
            if let Some(api_key) = &provider.api_key {
                entry.api_key = Some(api_key.clone());
            }
            if let Some(api_keys) = &provider.api_keys {
                entry.api_keys.clone_from(api_keys);
            }
            if let Some(base_url) = &provider.base_url {
                entry.base_url = Some(base_url.clone());
            }
            if let Some(api) = provider.api {
                entry.api = Some(api);
            }
        }
        if let Some(mode) = &self.approvals.mode {
            config.approvals.mode.clone_from(mode);
        }
        if let Some(sandbox) = &self.execution.sandbox {
            config.execution.sandbox.clone_from(sandbox);
        }
        if let Some(allowed) = self.workspace.allowed {
            config.workspace.allowed = allowed;
        }
    }

    /// Rejects values that no configuration could use, before they are saved.
    fn validate(&self) -> Result<(), ConfigError> {
        let mut config = NibConfig::default();
        let mut scalar = self.clone();
        scalar.llm = ProjectLlmSettings::default();
        scalar.apply(&mut config);
        config.validate()?;
        for (name, provider) in &self.llm.providers {
            let mut config = NibConfig::default();
            let mut entry = ProviderEntry {
                model: "model".to_string(),
                ..ProviderEntry::default()
            };
            entry.api_key.clone_from(&provider.api_key);
            entry.api_keys = provider.api_keys.clone().unwrap_or_default();
            entry.base_url.clone_from(&provider.base_url);
            entry.api = provider.api;
            config.llm.providers.insert(name.clone(), entry);
            config.validate()?;
        }
        Ok(())
    }
}

/// User-only values found in a project document.
#[derive(Debug, Default)]
pub(crate) struct UserOnlyValues {
    /// Values that differ from their defaults.
    pub(crate) settings: ProjectSettings,
    /// Every user-only key present, default-valued ones included.
    pub(crate) keys: Vec<String>,
}

impl UserOnlyValues {
    pub(crate) fn has_values(&self) -> bool {
        self.settings != ProjectSettings::default()
    }

    /// The keys whose values differ from their defaults.
    pub(crate) fn value_keys(&self) -> Vec<String> {
        self.settings
            .entries()
            .into_iter()
            .map(|(key, _)| key)
            .collect()
    }

    /// Whether the user's settings already hold every one of these values.
    pub(crate) fn is_in(&self, settings: &ProjectSettings) -> bool {
        settings.contains(&self.settings)
    }
}

/// Removes every user-only key from a project document and returns them.
pub(crate) fn split_user_only(document: &mut toml::Value) -> Result<UserOnlyValues, ConfigError> {
    let mut found = UserOnlyValues::default();
    let Some(table) = document.as_table_mut() else {
        return Ok(found);
    };
    if let Some(providers) = table
        .get_mut("llm")
        .and_then(|llm| llm.get_mut("providers"))
        .and_then(toml::Value::as_table_mut)
    {
        for (name, entry) in providers.iter_mut() {
            let Some(entry) = entry.as_table_mut() else {
                continue;
            };
            let mut removed = toml::Table::new();
            for key in USER_ONLY_PROVIDER_KEYS {
                if let Some(value) = entry.remove(key) {
                    found.keys.push(format!("llm.providers.{name}.{key}"));
                    removed.insert(key.to_string(), value);
                }
            }
            if removed.is_empty() {
                continue;
            }
            let provider: ProviderSettings = toml::Value::Table(removed).try_into()?;
            let provider = provider.without_defaults();
            if !provider.is_empty() {
                found.settings.llm.providers.insert(name.clone(), provider);
            }
        }
    }
    if let Some(approvals) = table.remove("approvals") {
        found.keys.push("approvals".to_string());
        let approvals: ProjectApprovalSettings = approvals.try_into()?;
        found.settings.approvals.mode = approvals
            .mode
            .filter(|mode| *mode != default_approval_mode());
    }
    if let Some(execution) = table
        .get_mut("execution")
        .and_then(toml::Value::as_table_mut)
    {
        if let Some(sandbox) = execution.remove("sandbox") {
            found.keys.push("execution.sandbox".to_string());
            let sandbox: String = sandbox.try_into()?;
            found.settings.execution.sandbox =
                Some(sandbox).filter(|sandbox| *sandbox != default_sandbox());
        }
    }
    if let Some(workspace) = table.remove("workspace") {
        found.keys.push("workspace".to_string());
        let workspace: ProjectWorkspaceSettings = workspace.try_into()?;
        found.settings.workspace.allowed = workspace.allowed.filter(|allowed| *allowed);
    }
    Ok(found)
}

/// The diagnostic for user-only values in a project's preferences.
pub(crate) fn user_only_error(path: &Path, keys: &[String], reason: &str) -> ConfigError {
    ConfigError::Operation(format!(
        "{} sets {}, which only the user may set ({reason}). Remove them from this file; set credentials with `nib auth` and approvals or trust with `/permissions` (they are kept in nib's user home)",
        path.display(),
        keys.join(", ")
    ))
}

/// The document a configuration saves to: values equal to their defaults
/// are left out unless `original` already had them, so a lower layer can
/// still supply them.
pub(crate) fn minimal_document(
    config: &NibConfig,
    original: Option<&toml::Value>,
) -> Result<toml::Value, ConfigError> {
    let mut document = toml::Value::try_from(config)?;
    let defaults = toml::Value::try_from(toml::from_str::<NibConfig>("")?)?;
    prune_defaults(&mut document, &defaults, original);
    prune_empty_provider_keys(&mut document, original);
    Ok(document)
}

/// Provider entries have no default entry to compare with; an empty key
/// list is their only default-valued field that is always serialized.
fn prune_empty_provider_keys(document: &mut toml::Value, original: Option<&toml::Value>) {
    let Some(providers) = document
        .get_mut("llm")
        .and_then(|llm| llm.get_mut("providers"))
        .and_then(toml::Value::as_table_mut)
    else {
        return;
    };
    for (name, entry) in providers.iter_mut() {
        let had_keys = original
            .and_then(|original| original.get("llm"))
            .and_then(|llm| llm.get("providers"))
            .and_then(|providers| providers.get(name))
            .is_some_and(|entry| entry.get("api_keys").is_some());
        if let Some(entry) = entry.as_table_mut() {
            if !had_keys
                && entry
                    .get("api_keys")
                    .and_then(toml::Value::as_array)
                    .is_some_and(Vec::is_empty)
            {
                entry.remove("api_keys");
            }
        }
    }
}

fn prune_defaults(value: &mut toml::Value, defaults: &toml::Value, original: Option<&toml::Value>) {
    let (Some(table), Some(defaults)) = (value.as_table_mut(), defaults.as_table()) else {
        return;
    };
    table.retain(|key, child| {
        let original_child = original.and_then(|original| original.get(key));
        let Some(default) = defaults.get(key) else {
            return true;
        };
        if child.is_table() {
            prune_defaults(child, default, original_child);
            let empty = child.as_table().is_some_and(toml::Table::is_empty);
            return !empty || original_child.is_some();
        }
        child != default || original_child.is_some()
    });
}

/// Overlays `overlay` onto `base`: tables merge key by key, and any other
/// value of `overlay` replaces the one in `base`.
pub(crate) fn deep_merge(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => deep_merge(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn in_file(path: &Path, error: ConfigError) -> ConfigError {
    ConfigError::Operation(format!("{}: {error}", path.display()))
}

fn no_user_home() -> ConfigError {
    ConfigError::Operation("nib's user home cannot be found: set NIB_HOME or HOME".to_string())
}

fn user_file_paths(directory: &Path, file: &str) -> ConfigPaths {
    ConfigPaths {
        nib_dir: directory.to_path_buf(),
        toml: directory.join(file),
        json: directory.join("config.json"),
        json_backup: directory.join("config.json.bak"),
    }
}

/// Reads a TOML document when its directory and file exist.
fn read_optional_document(
    directory: &Path,
    file: &Path,
) -> Result<Option<toml::Value>, ConfigError> {
    match fs::symlink_metadata(directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let directory = StableDirectory::open(directory).map_err(config_state_error)?;
    if !regular_file_exists(&directory, file)? {
        return Ok(None);
    }
    let (content, _) = read_regular_file(&directory, file).map_err(|error| in_file(file, error))?;
    toml::from_str(&content)
        .map(Some)
        .map_err(|error| in_file(file, error.into()))
}

/// Reads a TOML document inside a locked directory.
fn read_locked_document(
    directory: &StableDirectory,
    path: &Path,
) -> Result<(toml::Value, Option<File>), ConfigError> {
    if !regular_file_exists(directory, path)? {
        return Ok((toml::Value::Table(toml::Table::new()), None));
    }
    let (content, file) = read_regular_file(directory, path)?;
    let document = toml::from_str(&content).map_err(|error| in_file(path, error.into()))?;
    Ok((document, Some(file)))
}

fn write_document(
    directory: &StableDirectory,
    path: &Path,
    document: &toml::Value,
    expected: FileExpectation<'_>,
) -> Result<(), ConfigError> {
    let content = toml::to_string_pretty(document)?;
    if content.len() as u64 > MAX_CONFIG_FILE_BYTES {
        return Err(ConfigError::FileTooLarge {
            path: path.display().to_string(),
            size: content.len() as u64,
            max: MAX_CONFIG_FILE_BYTES,
        });
    }
    directory
        .save_bytes_atomically_expected_with_hook(
            path,
            content.as_bytes(),
            CONFIG_ATOMIC_TEMPORARY_PREFIX,
            true,
            expected,
            || Ok(()),
        )
        .map_err(config_state_error)
}

fn expectation(file: Option<&File>) -> FileExpectation<'_> {
    file.map_or(FileExpectation::Missing, FileExpectation::Present)
}

/// Creates a directory of the user home readable only by the user. An
/// existing directory keeps its permissions.
fn ensure_private_directory(path: &Path) -> Result<(), ConfigError> {
    let existed = fs::symlink_metadata(path).is_ok();
    crate::fs_security::ensure_directory_without_symlinks(path)?;
    #[cfg(unix)]
    if !existed {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = existed;
    Ok(())
}

/// The user configuration without its permission rules, as a layer.
fn user_layer(mut document: toml::Value) -> toml::Value {
    if let Some(table) = document.as_table_mut() {
        table.remove(PERMISSIONS_TABLE);
        table.remove("revision");
    }
    document
}

/// The user's layers for one project.
#[derive(Debug, Default)]
pub(crate) struct UserLayers {
    user: Option<toml::Value>,
    settings: ProjectSettings,
}

impl UserLayers {
    pub(crate) fn load(project_root: &Path) -> Result<Self, ConfigError> {
        let Some(home) = NibPaths::user_home() else {
            return Ok(Self::default());
        };
        let user_path = home.join("config.toml");
        let user = read_optional_document(&home, &user_path)?.map(user_layer);
        if let Some(user) = &user {
            user.clone()
                .try_into::<NibConfig>()
                .map_err(|error| in_file(&user_path, error.into()))?;
        }
        Ok(Self {
            user,
            settings: load_project_settings(project_root)?,
        })
    }

    /// The effective configuration over a project's preferences document.
    pub(crate) fn effective(
        &self,
        preferences: &toml::Value,
        revision: u64,
    ) -> Result<NibConfig, ConfigError> {
        let mut merged = self
            .user
            .clone()
            .unwrap_or_else(|| toml::Value::Table(toml::Table::new()));
        let mut preferences = preferences.clone();
        if let Some(table) = preferences.as_table_mut() {
            table.remove("revision");
        }
        deep_merge(&mut merged, preferences);
        let mut config: NibConfig = merged.try_into()?;
        self.settings.apply(&mut config);
        config.revision = revision;
        config.validate()?;
        Ok(config)
    }
}

/// The user's settings for a project; empty when there are none yet.
pub fn load_project_settings(project_root: &Path) -> Result<ProjectSettings, ConfigError> {
    let Some(directory) = NibPaths::user_project_dir(project_root) else {
        return Ok(ProjectSettings::default());
    };
    let path = directory.join(PROJECT_SETTINGS_FILE);
    match read_optional_document(&directory, &path)? {
        Some(document) => document
            .try_into()
            .map_err(|error: toml::de::Error| in_file(&path, error.into())),
        None => Ok(ProjectSettings::default()),
    }
}

/// The user's settings file for a project, when the project and the user
/// home resolve.
pub fn project_settings_path(project_root: &Path) -> Option<PathBuf> {
    NibPaths::user_project_dir(project_root).map(|directory| directory.join(PROJECT_SETTINGS_FILE))
}

/// The user configuration file, when the user home resolves.
pub fn user_config_path() -> Option<PathBuf> {
    NibPaths::user_config_file()
}

/// Edits the user configuration under its lock. Its permission rules and
/// every key it already had are kept.
pub fn update_user_config<T>(
    operation: impl FnOnce(&mut NibConfig) -> Result<T, String>,
) -> Result<T, ConfigError> {
    let home = NibPaths::user_home().ok_or_else(no_user_home)?;
    update_user_config_in(&home, operation)
}

/// The user configuration in `home` on its own, without permission rules;
/// defaults when it does not exist.
pub fn load_user_config_in(home: &Path) -> Result<NibConfig, ConfigError> {
    let path = home.join("config.toml");
    match read_optional_document(home, &path)? {
        Some(document) => {
            let config: NibConfig = user_layer(document)
                .try_into()
                .map_err(|error: toml::de::Error| in_file(&path, error.into()))?;
            config.validate().map_err(|error| in_file(&path, error.into()))?;
            Ok(config)
        }
        None => Ok(NibConfig::default()),
    }
}

/// [`update_user_config`] for the user home `home`.
pub fn update_user_config_in<T>(
    home: &Path,
    operation: impl FnOnce(&mut NibConfig) -> Result<T, String>,
) -> Result<T, ConfigError> {
    let home = home.to_path_buf();
    ensure_private_directory(&home)?;
    with_config_lock_at(
        user_file_paths(&home, "config.toml"),
        None,
        |_| Ok(()),
        |paths, directory| {
            let (original, file) = read_locked_document(directory, &paths.toml)?;
            let permissions = original.get(PERMISSIONS_TABLE).cloned();
            let mut config: NibConfig = user_layer(original.clone())
                .try_into()
                .map_err(|error: toml::de::Error| in_file(&paths.toml, error.into()))?;
            config.revision = original
                .get("revision")
                .and_then(toml::Value::as_integer)
                .and_then(|revision| u64::try_from(revision).ok())
                .unwrap_or(0);
            let output = operation(&mut config).map_err(ConfigError::Operation)?;
            config.revision = config.revision.checked_add(1).ok_or_else(|| {
                ConfigError::Operation("configuration revision overflowed".to_string())
            })?;
            config
                .validate()
                .map_err(|error| in_file(&paths.toml, error.into()))?;
            let mut document = minimal_document(&config, Some(&original))?;
            if let (Some(permissions), Some(table)) = (permissions, document.as_table_mut()) {
                table.insert(PERMISSIONS_TABLE.to_string(), permissions);
            }
            write_document(
                directory,
                &paths.toml,
                &document,
                expectation(file.as_ref()),
            )?;
            Ok(output)
        },
    )
}

/// Edits the user's settings for a project under their lock.
pub fn update_project_settings<T>(
    project_root: &Path,
    operation: impl FnOnce(&mut ProjectSettings) -> Result<T, String>,
) -> Result<T, ConfigError> {
    let home = NibPaths::user_home().ok_or_else(no_user_home)?;
    let directory = NibPaths::user_project_dir(project_root).ok_or_else(|| {
        ConfigError::Operation(format!(
            "the project folder cannot be resolved for its settings: {}",
            project_root.display()
        ))
    })?;
    let identity_root = NibPaths::project_identity_root(project_root).ok_or_else(|| {
        ConfigError::Operation("the project folder cannot be resolved".to_string())
    })?;
    ensure_private_directory(&home)?;
    if let Some(projects) = directory.parent() {
        ensure_private_directory(projects)?;
    }
    ensure_private_directory(&directory)?;
    with_config_lock_at(
        user_file_paths(&directory, PROJECT_SETTINGS_FILE),
        None,
        |_| Ok(()),
        |paths, locked| {
            record_project_identity(locked, &paths.nib_dir, &identity_root)?;
            let (original, file) = read_locked_document(locked, &paths.toml)?;
            let mut settings: ProjectSettings = original
                .try_into()
                .map_err(|error: toml::de::Error| in_file(&paths.toml, error.into()))?;
            let output = operation(&mut settings).map_err(ConfigError::Operation)?;
            settings
                .validate()
                .map_err(|error| in_file(&paths.toml, error))?;
            let document = toml::Value::try_from(&settings)?;
            write_document(locked, &paths.toml, &document, expectation(file.as_ref()))?;
            Ok(output)
        },
    )
}

/// Records which folder a project directory of the user home belongs to,
/// so a moved or copied project can be recognized later.
fn record_project_identity(
    directory: &StableDirectory,
    project_directory: &Path,
    root: &Path,
) -> Result<(), ConfigError> {
    let path = project_directory.join(PROJECT_IDENTITY_FILE);
    if regular_file_exists(directory, &path)? {
        return Ok(());
    }
    let mut identity = toml::Table::new();
    identity.insert("version".to_string(), 1.into());
    identity.insert("root".to_string(), root.display().to_string().into());
    let content = toml::to_string_pretty(&identity)?;
    directory
        .save_bytes_atomically_expected_with_hook(
            &path,
            content.as_bytes(),
            IDENTITY_TEMPORARY_PREFIX,
            true,
            FileExpectation::Missing,
            || Ok(()),
        )
        .map_err(config_state_error)
}

/// Whether Git tracks `file` in the repository at `project_root`: a file
/// the repository ships is never migrated into the user's settings.
fn tracked_by_git(project_root: &Path, file: &Path) -> bool {
    let Ok(relative) = file.strip_prefix(project_root) else {
        return false;
    };
    std::process::Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(relative)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// What loading a project's configuration file found and did.
pub(crate) struct ProjectPreferences {
    pub(crate) config: NibConfig,
    pub(crate) document: toml::Value,
    pub(crate) file: File,
}

/// Parses a project configuration file: its preferences, and the user-only
/// values it still holds.
pub(crate) fn parse_project_file(
    path: &Path,
    content: &str,
) -> Result<(NibConfig, toml::Value, UserOnlyValues), ConfigError> {
    let mut document: toml::Value = toml::from_str(content)?;
    let user_only = split_user_only(&mut document)?;
    let config: NibConfig = document.clone().try_into()?;
    config
        .validate_preferences()
        .map_err(|error| in_file(path, error.into()))?;
    Ok((config, document, user_only))
}

/// Moves the user-only values of a local project configuration into the
/// user's settings for the project, then rewrites the file as preferences.
///
/// The steps are ordered so an interruption is resumed by the next load:
/// the original is kept first (an identical earlier copy is reused), the
/// values are added to the user's settings (never replacing one), and the
/// project file is replaced only if it is still the one that was read.
pub(crate) fn migrate_project_file(
    paths: &ConfigPaths,
    directory: &StableDirectory,
    content: &str,
    file: File,
    parsed: (NibConfig, toml::Value, UserOnlyValues),
) -> Result<ProjectPreferences, ConfigError> {
    let (config, document, user_only) = parsed;
    if user_only.keys.is_empty() {
        return Ok(ProjectPreferences {
            config,
            document,
            file,
        });
    }
    let project_root = paths.nib_dir.parent().ok_or_else(|| {
        ConfigError::Operation("project configuration has no project folder".to_string())
    })?;
    if tracked_by_git(project_root, &paths.toml) {
        if user_only.has_values() {
            return Err(user_only_error(
                &paths.toml,
                &user_only.keys,
                "the repository ships this file",
            ));
        }
        // Defaults carry nothing to keep, and a shipped file is not
        // rewritten: they are ignored.
        return Ok(ProjectPreferences {
            config,
            document,
            file,
        });
    }
    if user_only.has_values() {
        let current = load_project_settings(project_root)?;
        check_migration(&paths.toml, &current, &user_only)?;
    }
    let backup = keep_legacy_original(directory, &paths.nib_dir, content)?;
    if user_only.has_values() {
        let incoming = user_only.settings.clone();
        let record = backup.clone();
        let toml_path = paths.toml.clone();
        let keys = user_only.keys.clone();
        update_project_settings(project_root, move |settings| {
            check_migration(
                &toml_path,
                settings,
                &UserOnlyValues {
                    settings: incoming.clone(),
                    keys,
                },
            )
            .map_err(|error| error.to_string())?;
            if settings.migration.project_config.is_none() {
                settings.add_missing(&incoming);
                settings.migration = ProjectMigrationRecord {
                    project_config: Some(chrono::Utc::now().to_rfc3339()),
                    backup: Some(record),
                };
            }
            Ok(())
        })?;
    }
    // The rewrite means the same configuration, so it keeps the revision.
    let rewritten = minimal_document(&config, None)?;
    write_document(
        directory,
        &paths.toml,
        &rewritten,
        FileExpectation::Present(&file),
    )?;
    let file = directory
        .open_read(&paths.toml)
        .map_err(config_state_error)?;
    Ok(ProjectPreferences {
        config,
        document: rewritten,
        file,
    })
}

/// Refuses a migration that would replace a user setting, or values that
/// appeared after this project was already migrated.
fn check_migration(
    path: &Path,
    current: &ProjectSettings,
    incoming: &UserOnlyValues,
) -> Result<(), ConfigError> {
    if current.migration.project_config.is_some() {
        if current.contains(&incoming.settings) {
            return Ok(());
        }
        return Err(user_only_error(
            path,
            &incoming.keys,
            "this project's settings were already moved to nib's user home",
        ));
    }
    let conflicts = current.conflicts_with(&incoming.settings);
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(user_only_error(
            path,
            &conflicts,
            "they differ from this project's settings in nib's user home; nothing was changed",
        ))
    }
}

/// Keeps the original of a migrated project configuration under
/// `<project>/.nib/legacy-<time>/config.toml`, reusing an identical copy.
fn keep_legacy_original(
    directory: &StableDirectory,
    nib_dir: &Path,
    content: &str,
) -> Result<String, ConfigError> {
    let mut existing = None;
    directory
        .for_each_entry_bounded(
            MAX_LEGACY_SCAN_ENTRIES,
            MAX_CONFIG_DIRECTORY_NAME_BYTES,
            |name| {
                let name = name.to_string_lossy();
                if existing.is_none() && name.starts_with(LEGACY_DIRECTORY_PREFIX) {
                    let copy = nib_dir.join(name.as_ref()).join("config.toml");
                    if fs::read(&copy).is_ok_and(|bytes| bytes == content.as_bytes()) {
                        existing = Some(name.into_owned());
                    }
                }
                Ok(())
            },
        )
        .map_err(config_state_error)?;
    if let Some(name) = existing {
        return Ok(nib_dir.join(name).display().to_string());
    }
    let name = format!(
        "{LEGACY_DIRECTORY_PREFIX}{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%.6fZ")
    );
    let legacy = nib_dir.join(&name);
    ensure_private_directory(&legacy)?;
    let legacy_directory = StableDirectory::open(&legacy).map_err(config_state_error)?;
    legacy_directory
        .save_bytes_atomically_expected_with_hook(
            &legacy.join("config.toml"),
            content.as_bytes(),
            LEGACY_TEMPORARY_PREFIX,
            true,
            FileExpectation::Missing,
            || Ok(()),
        )
        .map_err(config_state_error)?;
    Ok(legacy.display().to_string())
}

#[cfg(test)]
#[path = "layers_tests.rs"]
mod tests;
