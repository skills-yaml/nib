//! Project-local configuration stored in `.nib/config.toml`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const SUPPORTED_PROVIDERS: &[(&str, &str)] = &[
    ("openai", "OpenAI (gpt-4o etc)"),
    ("anthropic", "Anthropic Claude"),
    ("google", "Google Gemini"),
    ("grok", "xAI Grok"),
    ("openrouter", "OpenRouter"),
    ("mock", "Mock"),
];

pub const AVAILABLE_MODELS: &[(&str, &[&str])] = &[
    (
        "openai",
        &["gpt-4o", "gpt-4o-mini", "gpt-4-turbo", "o1-preview"],
    ),
    (
        "anthropic",
        &[
            "claude-3-5-sonnet-20241022",
            "claude-3-opus-20240229",
            "claude-3-haiku-20240307",
        ],
    ),
    (
        "google",
        &["gemini-1.5-pro", "gemini-1.5-flash", "gemini-2.0-flash-exp"],
    ),
    ("grok", &["grok-2-1212", "grok-beta", "grok-3"]),
    (
        "openrouter",
        &[
            "openrouter/anthropic/claude-3.5-sonnet",
            "openrouter/meta-llama/llama-3.1-70b-instruct",
            "openrouter/google/gemini-1.5-pro",
            "openrouter/mistralai/mistral-large",
        ],
    ),
    ("mock", &["mock-model"]),
];

/// Legacy alias used by CLI modules during the Rust migration.
pub type LLMConfigFile = LlmConfig;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct NibConfig {
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub execution: ExecutionConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub compression: CompressionConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub daemons: DaemonsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ExecutionConfig {
    #[serde(default = "default_execution_provider")]
    pub provider: String,
    #[serde(default = "default_profile")]
    pub default_profile: String,
    #[serde(default = "default_true")]
    pub plan_mode: bool,
    #[serde(default)]
    pub boundaries: BoundaryConfig,
}

fn default_execution_provider() -> String {
    "internal".to_string()
}

fn default_profile() -> String {
    "restricted".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct BoundaryConfig {
    #[serde(default)]
    pub allow_write: Vec<String>,
    #[serde(default = "default_network")]
    pub network: String,
}

fn default_network() -> String {
    "restricted".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompressionConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_threshold")]
    pub threshold: f64,
    #[serde(default = "default_target_ratio")]
    pub target_ratio: f64,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: 0.50,
            target_ratio: 0.20,
        }
    }
}

fn default_threshold() -> f64 {
    0.50
}
fn default_target_ratio() -> f64 {
    0.20
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_memory_provider")]
    pub provider: String,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: "built-in".to_string(),
        }
    }
}

fn default_memory_provider() -> String {
    "built-in".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonsConfig {
    #[serde(default = "default_true")]
    pub cron_enabled: bool,
    #[serde(default = "default_true")]
    pub curator_enabled: bool,
    #[serde(default = "default_retention_days")]
    pub retention_days: i64,
}

impl Default for DaemonsConfig {
    fn default() -> Self {
        Self {
            cron_enabled: true,
            curator_enabled: true,
            retention_days: 30,
        }
    }
}

fn default_retention_days() -> i64 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: HashMap<String, McpServerEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct McpServerEntry {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LlmConfig {
    pub active_provider: Option<String>,
    #[serde(default)]
    pub providers: HashMap<String, ProviderEntry>,
    #[serde(default = "default_context_length")]
    pub context_length: usize,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            active_provider: None,
            providers: HashMap::new(),
            context_length: 128_000,
        }
    }
}

fn default_context_length() -> usize {
    128_000
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ProviderEntry {
    pub model: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    pub nib_dir: PathBuf,
    pub toml: PathBuf,
    pub json: PathBuf,
    pub json_backup: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    Toml,
    MigratedFromJson,
    Default,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("failed to serialize TOML: {0}")]
    TomlSerialize(#[from] toml::ser::Error),
    #[error("failed to parse legacy JSON: {0}")]
    Json(#[from] serde_json::Error),
}

impl LlmConfig {
    pub fn get_active_provider(&self) -> String {
        self.active_provider
            .clone()
            .unwrap_or_else(|| "mock".to_string())
    }

    pub fn get_provider(&self, name: Option<&str>) -> Option<&ProviderEntry> {
        let active = self.get_active_provider();
        let name = name.unwrap_or(active.as_str());
        self.providers.get(name)
    }

    pub fn get_available_models(&self, provider: Option<&str>) -> Vec<String> {
        let active = self.get_active_provider();
        let p = provider.unwrap_or(active.as_str());
        AVAILABLE_MODELS
            .iter()
            .find(|(name, _)| *name == p)
            .map(|(_, models)| models.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default()
    }

    pub fn update_model_for_active(&mut self, new_model: String) {
        let active = self.get_active_provider();
        if let Some(entry) = self.providers.get_mut(&active) {
            entry.model = new_model;
        } else if active == "mock" {
            self.providers.insert(
                "mock".to_string(),
                ProviderEntry {
                    model: new_model,
                    api_key: None,
                    base_url: None,
                },
            );
            if self.active_provider.is_none() {
                self.active_provider = Some("mock".to_string());
            }
        }
    }

    pub fn add_or_update_provider(
        &mut self,
        provider: String,
        model: String,
        api_key: Option<String>,
    ) {
        let entry = self
            .providers
            .entry(provider.clone())
            .or_insert_with(|| ProviderEntry {
                model: model.clone(),
                api_key: None,
                base_url: None,
            });
        entry.model = model;
        if api_key.is_some() {
            entry.api_key = api_key;
        }
        if self.active_provider.is_none() || self.active_provider.as_deref() == Some("mock") {
            self.active_provider = Some(provider);
        }
    }
}

pub fn config_paths(project_root: &Path) -> ConfigPaths {
    let nib_dir = project_root.join(".nib");
    ConfigPaths {
        toml: nib_dir.join("config.toml"),
        json: nib_dir.join("config.json"),
        json_backup: nib_dir.join("config.json.bak"),
        nib_dir,
    }
}

pub fn load_config(project_root: &Path) -> LlmConfig {
    load_nib_config_full(project_root)
        .map(|c| c.llm)
        .unwrap_or_default()
}

pub fn load_nib_config(project_root: &Path) -> NibConfig {
    load_nib_config_full(project_root).unwrap_or_default()
}

pub fn load_nib_config_full(project_root: &Path) -> Result<NibConfig, ConfigError> {
    let paths = config_paths(project_root);
    fs::create_dir_all(&paths.nib_dir)?;

    if paths.toml.exists() {
        return load_nib_config_file(&paths.toml);
    }

    if paths.json.exists() {
        let llm = migrate_json_to_toml(&paths)?;
        return Ok(NibConfig {
            llm,
            execution: ExecutionConfig::default(),
            mcp: McpConfig::default(),
            compression: CompressionConfig::default(),
            memory: MemoryConfig::default(),
            daemons: DaemonsConfig::default(),
        });
    }

    Ok(NibConfig::default())
}

pub fn load_config_with_source(
    project_root: &Path,
) -> Result<(LlmConfig, ConfigSource), ConfigError> {
    let paths = config_paths(project_root);
    fs::create_dir_all(&paths.nib_dir)?;

    if paths.toml.exists() {
        let cfg = load_nib_config_file(&paths.toml)?;
        return Ok((cfg.llm, ConfigSource::Toml));
    }

    if paths.json.exists() {
        let llm = migrate_json_to_toml(&paths)?;
        return Ok((llm, ConfigSource::MigratedFromJson));
    }

    Ok((LlmConfig::default(), ConfigSource::Default))
}

pub fn save_config(project_root: &Path, llm: &LlmConfig) -> Result<(), ConfigError> {
    let mut cfg = load_nib_config(project_root);
    cfg.llm = llm.clone();
    save_nib_config_full(project_root, &cfg)
}

pub fn save_nib_config_full(project_root: &Path, cfg: &NibConfig) -> Result<(), ConfigError> {
    let paths = config_paths(project_root);
    fs::create_dir_all(&paths.nib_dir)?;
    save_nib_config(&paths.toml, cfg)
}

fn load_nib_config_file(path: &Path) -> Result<NibConfig, ConfigError> {
    let content = fs::read_to_string(path)?;
    let cfg: NibConfig = toml::from_str(&content)?;
    Ok(cfg)
}

fn save_nib_config(path: &Path, cfg: &NibConfig) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = toml::to_string_pretty(cfg)?;
    fs::write(path, content)?;
    Ok(())
}

fn migrate_json_to_toml(paths: &ConfigPaths) -> Result<LlmConfig, ConfigError> {
    let content = fs::read_to_string(&paths.json)?;
    let llm: LlmConfig = serde_json::from_str(&content)?;
    save_nib_config(
        &paths.toml,
        &NibConfig {
            llm: llm.clone(),
            execution: ExecutionConfig::default(),
            mcp: McpConfig::default(),
            compression: CompressionConfig::default(),
            memory: MemoryConfig::default(),
            daemons: DaemonsConfig::default(),
        },
    )?;

    if paths.json_backup.exists() {
        fs::remove_file(&paths.json_backup)?;
    }
    fs::rename(&paths.json, &paths.json_backup)?;

    Ok(llm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn toml_roundtrip_preserves_llm_config() {
        let llm = LlmConfig {
            active_provider: Some("openai".to_string()),
            providers: HashMap::from([(
                "openai".to_string(),
                ProviderEntry {
                    model: "gpt-4o".to_string(),
                    api_key: Some("sk-test".to_string()),
                    base_url: None,
                },
            )]),
            context_length: 128_000,
        };
        let nib = NibConfig {
            llm: llm.clone(),
            execution: ExecutionConfig::default(),
            mcp: McpConfig::default(),
            compression: CompressionConfig::default(),
            memory: MemoryConfig::default(),
            daemons: DaemonsConfig::default(),
        };
        let serialized = toml::to_string_pretty(&nib).expect("serialize");
        let parsed: NibConfig = toml::from_str(&serialized).expect("parse");
        assert_eq!(parsed.llm, llm);
    }

    #[test]
    fn config_migration_from_json() {
        let dir = tempdir().expect("tempdir");
        let paths = config_paths(dir.path());
        fs::create_dir_all(&paths.nib_dir).expect("mkdir");

        let legacy = r#"{
  "active_provider": "grok",
  "providers": {
    "grok": {
      "model": "grok-2-1212",
      "api_key": "xai-test",
      "base_url": null
    }
  }
}"#;
        fs::write(&paths.json, legacy).expect("write json");

        let (llm, source) = load_config_with_source(dir.path()).expect("load");
        assert_eq!(source, ConfigSource::MigratedFromJson);
        assert_eq!(llm.get_active_provider(), "grok");
        assert_eq!(
            llm.providers.get("grok").map(|p| p.model.as_str()),
            Some("grok-2-1212")
        );
        assert!(paths.toml.exists());
        assert!(!paths.json.exists());
        assert!(paths.json_backup.exists());

        let (reloaded, source) = load_config_with_source(dir.path()).expect("reload");
        assert_eq!(source, ConfigSource::Toml);
        assert_eq!(reloaded, llm);
    }

    #[test]
    fn save_and_load_toml() {
        let dir = tempdir().expect("tempdir");
        let llm = LlmConfig {
            active_provider: Some("mock".to_string()),
            providers: HashMap::from([(
                "mock".to_string(),
                ProviderEntry {
                    model: "mock-model".to_string(),
                    api_key: None,
                    base_url: None,
                },
            )]),
            context_length: 128_000,
        };
        save_config(dir.path(), &llm).expect("save");
        let loaded = load_config(dir.path());
        assert_eq!(loaded, llm);
    }
}
