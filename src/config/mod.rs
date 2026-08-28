//! Project-local configuration stored in `.nib/config.toml`.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use thiserror::Error;

type ConfigMutex = Mutex<()>;

static CONFIG_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<ConfigMutex>>>> = OnceLock::new();
const CONFIG_OPERATION_ERROR_SENTINEL: &str = "__nib_config_operation_error__";
const MAX_CONFIG_DIRECTORY_ENTRIES: usize = 10_000;
const MAX_CONFIG_DIRECTORY_NAME_BYTES: usize = MAX_CONFIG_DIRECTORY_ENTRIES * 256;

#[cfg(test)]
type ConfigReadHook = Box<dyn FnOnce(&Path) -> Result<(), String> + Send>;

#[cfg(test)]
struct PendingConfigReadHook {
    path: PathBuf,
    hook: Option<ConfigReadHook>,
}

#[cfg(test)]
static CONFIG_READ_HOOK: OnceLock<Mutex<Option<PendingConfigReadHook>>> = OnceLock::new();

#[cfg(test)]
pub(crate) struct ConfigReadHookGuard {
    path: PathBuf,
}

#[cfg(test)]
impl Drop for ConfigReadHookGuard {
    fn drop(&mut self) {
        let registry = CONFIG_READ_HOOK.get_or_init(|| Mutex::new(None));
        if let Ok(mut pending) = registry.lock() {
            if pending
                .as_ref()
                .is_some_and(|pending| pending.path == self.path)
            {
                *pending = None;
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn install_config_read_hook(
    path: PathBuf,
    hook: impl FnOnce(&Path) -> Result<(), String> + Send + 'static,
) -> ConfigReadHookGuard {
    let registry = CONFIG_READ_HOOK.get_or_init(|| Mutex::new(None));
    let mut pending = registry.lock().expect("config read hook lock");
    assert!(pending.is_none(), "config read hook already installed");
    *pending = Some(PendingConfigReadHook {
        path: path.clone(),
        hook: Some(Box::new(hook)),
    });
    ConfigReadHookGuard { path }
}

#[cfg(test)]
fn run_config_read_hook(path: &Path) -> Result<(), ConfigError> {
    let registry = CONFIG_READ_HOOK.get_or_init(|| Mutex::new(None));
    let hook = {
        let mut pending = registry.lock().expect("config read hook lock");
        if pending.as_ref().is_some_and(|pending| pending.path == path) {
            pending.take().and_then(|mut pending| pending.hook.take())
        } else {
            None
        }
    };
    hook.map_or(Ok(()), |hook| hook(path).map_err(config_state_error))
}

/// Legacy alias used by CLI modules during the Rust migration.
pub type LLMConfigFile = LlmConfig;

pub(crate) const MAX_MCP_CONFIGURED_SERVERS: usize = 32;
pub(crate) const MAX_MCP_SERVER_NAME_BYTES: usize = 64;
pub(crate) const MAX_MCP_REQUEST_TIMEOUT_SECS: u64 = 3_600;
const MAX_CONFIG_FILE_BYTES: u64 = 1024 * 1024;
const MAX_CONTEXT_LENGTH: usize = 4_000_000;
const MAX_AGENT_TURNS: u32 = 10_000;
const MAX_TERMINAL_TIMEOUT_SECS: u64 = 3_600;
const MAX_PROVIDERS: usize = 64;
const MAX_PROVIDER_MODELS: usize = 128;
const MAX_PROFILES: usize = 64;
const MAX_IDENTIFIER_BYTES: usize = 128;
const MAX_MODEL_BYTES: usize = 512;
const MAX_URL_BYTES: usize = 4 * 1024;
const MAX_SECRET_BYTES: usize = 64 * 1024;
const MAX_PROVIDER_KEYS: usize = 64;
const MAX_PATH_BYTES: usize = 4 * 1024;
const MAX_BOUNDARY_PATHS: usize = 256;
const MAX_BOUNDARY_PROFILES: usize = 64;
const MAX_SKILL_PATHS: usize = 256;
const MAX_ACTIVE_SKILLS: usize = 256;
const MAX_MCP_COMMAND_BYTES: usize = 4 * 1024;
const MAX_MCP_ARGUMENTS: usize = 256;
const MAX_MCP_ARGUMENT_BYTES: usize = 16 * 1024;
const MAX_MCP_ARGUMENT_BYTES_TOTAL: usize = 256 * 1024;
const MAX_MCP_ENV_VARS: usize = 256;
const MAX_ENV_NAME_BYTES: usize = 128;
const MAX_ENV_VALUE_BYTES: usize = 64 * 1024;
const MAX_MCP_ENV_BYTES_TOTAL: usize = 512 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NibConfig {
    #[serde(default, skip_serializing_if = "revision_is_zero")]
    pub revision: u64,
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
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub terminal: TerminalConfig,
    #[serde(default)]
    pub approvals: ApprovalsConfig,
    #[serde(default)]
    pub workload: WorkloadConfig,
    #[serde(default)]
    pub skills: SkillsConfig,
    #[serde(default)]
    pub profiles: ProfilesConfig,
}

fn revision_is_zero(revision: &u64) -> bool {
    *revision == 0
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConfig {
    #[serde(default = "default_execution_provider")]
    pub provider: String,
    #[serde(default = "default_profile")]
    pub default_profile: String,
    #[serde(default = "default_true")]
    pub plan_mode: bool,
    #[serde(default)]
    pub boundaries: BoundaryConfig,
    #[serde(default)]
    pub boundary_profiles: HashMap<String, BoundaryConfig>,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            provider: default_execution_provider(),
            default_profile: default_profile(),
            plan_mode: true,
            boundaries: BoundaryConfig::default(),
            boundary_profiles: HashMap::new(),
        }
    }
}

fn default_execution_provider() -> String {
    "hybrid".to_string()
}

fn default_profile() -> String {
    "restricted".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BoundaryConfig {
    #[serde(default)]
    pub allow_write: Vec<String>,
    #[serde(default = "default_network")]
    pub network: String,
}

impl Default for BoundaryConfig {
    fn default() -> Self {
        Self {
            allow_write: Vec::new(),
            network: default_network(),
        }
    }
}

fn default_network() -> String {
    "restricted".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct DaemonsConfig {
    #[serde(default = "default_true")]
    pub cron_enabled: bool,
    #[serde(default = "default_true")]
    pub curator_enabled: bool,
    #[serde(default = "default_retention_days")]
    pub retention_days: i64,
    #[serde(default = "default_daemon_interval_seconds")]
    pub interval_seconds: u64,
    #[serde(default)]
    pub allow_destructive_cleanup: bool,
}

impl Default for DaemonsConfig {
    fn default() -> Self {
        Self {
            cron_enabled: true,
            curator_enabled: true,
            retention_days: 30,
            interval_seconds: default_daemon_interval_seconds(),
            allow_destructive_cleanup: false,
        }
    }
}

fn default_retention_days() -> i64 {
    30
}

fn default_daemon_interval_seconds() -> u64 {
    24 * 60 * 60
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    #[serde(default = "default_true")]
    pub tool_use_enforcement: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: default_max_turns(),
            tool_use_enforcement: true,
        }
    }
}

fn default_max_turns() -> u32 {
    90
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TerminalConfig {
    #[serde(default = "default_terminal_backend")]
    pub backend: String,
    #[serde(default = "default_terminal_timeout")]
    pub timeout: u64,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            backend: default_terminal_backend(),
            timeout: default_terminal_timeout(),
        }
    }
}

fn default_terminal_backend() -> String {
    "local".to_string()
}

fn default_terminal_timeout() -> u64 {
    180
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApprovalsConfig {
    #[serde(default = "default_approval_mode")]
    pub mode: String,
}

impl Default for ApprovalsConfig {
    fn default() -> Self {
        Self {
            mode: default_approval_mode(),
        }
    }
}

fn default_approval_mode() -> String {
    "manual".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkloadConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_workload_store")]
    pub store: String,
    #[serde(default = "default_true")]
    pub require_reconciliation: bool,
}

impl Default for WorkloadConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            store: default_workload_store(),
            require_reconciliation: true,
        }
    }
}

fn default_workload_store() -> String {
    "sessions".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SkillsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_skill_paths")]
    pub paths: Vec<PathBuf>,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            paths: default_skill_paths(),
        }
    }
}

fn default_skill_paths() -> Vec<PathBuf> {
    vec![PathBuf::from(".nib/skills")]
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    pub id: String,
    pub root: PathBuf,
    #[serde(default)]
    pub env_file: Option<PathBuf>,
    #[serde(default)]
    pub active_skills: Vec<String>,
    #[serde(default)]
    pub skill_paths: Vec<PathBuf>,
    #[serde(default)]
    pub state_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProfilesConfig {
    #[serde(default = "default_profile_id")]
    pub default: String,
    #[serde(default)]
    pub active: Vec<ProfileConfig>,
}

impl Default for ProfilesConfig {
    fn default() -> Self {
        Self {
            default: default_profile_id(),
            active: Vec::new(),
        }
    }
}

fn default_profile_id() -> String {
    "default".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default = "default_true")]
    pub client_enabled: bool,
    #[serde(default = "default_true")]
    pub server_enabled: bool,
    #[serde(default)]
    pub servers: HashMap<String, McpServerEntry>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            client_enabled: true,
            server_enabled: true,
            servers: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpServerEntry {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default = "default_mcp_request_timeout_secs")]
    pub request_timeout_secs: u64,
}

impl Default for McpServerEntry {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            request_timeout_secs: default_mcp_request_timeout_secs(),
        }
    }
}

fn default_mcp_request_timeout_secs() -> u64 {
    30
}

pub(crate) fn is_valid_mcp_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_MCP_SERVER_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LlmApiMode {
    #[default]
    ChatCompletions,
    Responses,
}

impl LlmApiMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
        }
    }

    pub fn endpoint_suffix(self) -> &'static str {
        match self {
            Self::ChatCompletions => "/chat/completions",
            Self::Responses => "/responses",
        }
    }
}

impl fmt::Display for LlmApiMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub fn is_openai_compatible_provider(name: &str) -> bool {
    crate::llm::registry::provider_descriptor(name)
        .is_some_and(|provider| provider.is_openai_compatible())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    pub api_key: Option<String>,
    #[serde(default)]
    pub api_keys: Vec<String>,
    pub base_url: Option<String>,
    #[serde(default)]
    pub api: Option<LlmApiMode>,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl ProviderEntry {
    pub fn resolved_api_mode(&self) -> LlmApiMode {
        self.api.unwrap_or_default()
    }
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

impl ConfigSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Toml => "config.toml",
            Self::MigratedFromJson => "migrated config.json",
            Self::Default => "defaults (no config file)",
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse TOML: {0}")]
    Toml(String),
    #[error("failed to serialize TOML: {0}")]
    TomlSerialize(#[from] toml::ser::Error),
    #[error("failed to parse legacy JSON: {0}")]
    Json(String),
    #[error(transparent)]
    Validation(#[from] ConfigValidationError),
    #[error("configuration operation failed: {0}")]
    Operation(String),
    #[error("configuration path is not a regular file: {0}")]
    InvalidFileType(String),
    #[error("configuration lock was poisoned: {0}")]
    LockPoisoned(String),
    #[error("configuration file {path} is {size} bytes; maximum is {max} bytes")]
    FileTooLarge { path: String, size: u64, max: u64 },
}

impl From<toml::de::Error> for ConfigError {
    fn from(error: toml::de::Error) -> Self {
        let location = error
            .span()
            .map(|span| format!(" near byte {}", span.start))
            .unwrap_or_default();
        Self::Toml(format!(
            "invalid syntax or value{location}; source excerpt omitted"
        ))
    }
}

impl From<serde_json::Error> for ConfigError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(format!(
            "invalid syntax or value at line {} column {}; source excerpt omitted",
            error.line(),
            error.column()
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigValidationError {
    pub issues: Vec<String>,
}

impl fmt::Display for ConfigValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid configuration: {}", self.issues.join("; "))
    }
}

impl std::error::Error for ConfigValidationError {}

impl NibConfig {
    pub fn validate(&self) -> Result<(), ConfigValidationError> {
        let mut issues = Vec::new();

        if self.llm.context_length == 0 {
            issues.push("llm.context_length must be greater than zero".to_string());
        } else if self.llm.context_length > MAX_CONTEXT_LENGTH {
            issues.push(format!(
                "llm.context_length must be at most {MAX_CONTEXT_LENGTH}"
            ));
        }
        if self.llm.providers.len() > MAX_PROVIDERS {
            issues.push(format!(
                "llm.providers must contain at most {MAX_PROVIDERS} entries"
            ));
        }
        if let Some(active_provider) = &self.llm.active_provider {
            if active_provider.trim().is_empty() {
                issues.push("llm.active_provider must not be empty when configured".to_string());
            } else if !is_safe_identifier(active_provider) {
                issues.push(format!(
                    "llm.active_provider must be at most {MAX_IDENTIFIER_BYTES} bytes and contain only supported identifier characters"
                ));
            } else if !self.llm.providers.contains_key(active_provider) {
                issues.push(format!(
                    "llm.active_provider references unknown provider: {active_provider}"
                ));
            }
        }
        for (name, provider) in &self.llm.providers {
            if name.trim().is_empty() {
                issues.push("llm.providers must not contain an empty provider name".to_string());
            } else if !is_safe_identifier(name) {
                issues.push(format!(
                    "llm provider name '{name}' must be at most {MAX_IDENTIFIER_BYTES} bytes and contain only supported identifier characters"
                ));
            }
            if provider.model.trim().is_empty() {
                issues.push(format!("llm.providers.{name}.model must not be empty"));
            } else if provider.model.len() > MAX_MODEL_BYTES || provider.model.contains('\0') {
                issues.push(format!(
                    "llm.providers.{name}.model must be at most {MAX_MODEL_BYTES} bytes and contain no NUL"
                ));
            }
            if let Some(models) = &provider.models {
                if models.len() > MAX_PROVIDER_MODELS {
                    issues.push(format!(
                        "llm.providers.{name}.models must contain at most {MAX_PROVIDER_MODELS} entries"
                    ));
                }
                if models.iter().any(|model| model.trim().is_empty()) {
                    issues.push(format!(
                        "llm.providers.{name}.models must not contain empty model identifiers"
                    ));
                }
                if models
                    .iter()
                    .any(|model| model.len() > MAX_MODEL_BYTES || model.contains('\0'))
                {
                    issues.push(format!(
                        "llm.providers.{name}.models entries must be at most {MAX_MODEL_BYTES} bytes and contain no NUL"
                    ));
                }
                let unique_models = models.iter().collect::<HashSet<_>>();
                if unique_models.len() != models.len() {
                    issues.push(format!(
                        "llm.providers.{name}.models must not contain duplicate model identifiers"
                    ));
                }
            }
            if let Some(url) = &provider.base_url {
                if url.trim().is_empty() {
                    issues.push(format!(
                        "llm.providers.{name}.base_url must not be empty when configured"
                    ));
                } else if url.len() > MAX_URL_BYTES || url.contains('\0') {
                    issues.push(format!(
                        "llm.providers.{name}.base_url must be at most {MAX_URL_BYTES} bytes and contain no NUL"
                    ));
                } else if is_openai_compatible_provider(name) {
                    if let Some(issue) =
                        configured_endpoint_issue(url, provider.resolved_api_mode())
                    {
                        if issue.starts_with("conflicts") {
                            issues.push(format!(
                                "llm.providers.{name}.base_url conflicts with api = '{}' or contains a doubled API suffix",
                                provider.resolved_api_mode()
                            ));
                        } else {
                            issues.push(format!("llm.providers.{name}.base_url {issue}"));
                        }
                    }
                }
            }
            if !is_openai_compatible_provider(name)
                && (provider.api.is_some() || provider.reasoning_effort.is_some())
            {
                issues.push(format!(
                    "llm.providers.{name}.api and reasoning_effort are supported only by OpenAI-compatible providers"
                ));
            }
            if name == "mock"
                && (provider.api_key.is_some()
                    || !provider.api_keys.is_empty()
                    || provider.base_url.is_some())
            {
                issues.push(
                    "llm.providers.mock api_key, api_keys, and base_url are not supported"
                        .to_string(),
                );
            }
            if let Some(key) = &provider.api_key {
                if key.trim().is_empty() {
                    issues.push(format!(
                        "llm.providers.{name}.api_key must not be empty when configured"
                    ));
                } else if key.len() > MAX_SECRET_BYTES || key.contains('\0') {
                    issues.push(format!(
                        "llm.providers.{name}.api_key must be at most {MAX_SECRET_BYTES} bytes and contain no NUL"
                    ));
                }
            }
            if provider.api_keys.len() > MAX_PROVIDER_KEYS {
                issues.push(format!(
                    "llm.providers.{name}.api_keys must contain at most {MAX_PROVIDER_KEYS} entries"
                ));
            }
            if provider.api_keys.iter().any(|key| key.trim().is_empty()) {
                issues.push(format!(
                    "llm.providers.{name}.api_keys must not contain empty keys"
                ));
            }
            if provider
                .api_keys
                .iter()
                .any(|key| key.len() > MAX_SECRET_BYTES || key.contains('\0'))
            {
                issues.push(format!(
                    "llm.providers.{name}.api_keys entries must be at most {MAX_SECRET_BYTES} bytes and contain no NUL"
                ));
            }
        }
        if self.agent.max_turns == 0 {
            issues.push("agent.max_turns must be greater than zero".to_string());
        } else if self.agent.max_turns > MAX_AGENT_TURNS {
            issues.push(format!("agent.max_turns must be at most {MAX_AGENT_TURNS}"));
        }
        if !matches!(
            self.execution.provider.as_str(),
            "internal" | "hybrid" | "bwrap"
        ) {
            issues.push("execution.provider must be internal, hybrid, or bwrap".to_string());
        }
        if !matches!(
            self.execution.default_profile.as_str(),
            "restricted" | "internal"
        ) {
            issues.push("execution.default_profile must be restricted or internal".to_string());
        }
        validate_boundary_config(
            "execution.boundaries",
            &self.execution.boundaries,
            &mut issues,
        );
        if self.execution.boundary_profiles.len() > MAX_BOUNDARY_PROFILES {
            issues.push(format!(
                "execution.boundary_profiles must contain at most {MAX_BOUNDARY_PROFILES} entries"
            ));
        }
        for (name, profile) in &self.execution.boundary_profiles {
            let field = format!("execution.boundary_profiles.{name}");
            if !is_safe_identifier(name) || matches!(name.as_str(), "internal" | "restricted") {
                issues.push(format!(
                    "execution boundary profile '{name}' must be a non-reserved identifier of at most {MAX_IDENTIFIER_BYTES} bytes"
                ));
            }
            validate_boundary_config(&field, profile, &mut issues);
            if let Err(reason) =
                boundary_profile_tightening_error(&self.execution.boundaries, profile)
            {
                issues.push(format!(
                    "execution boundary profile '{name}' must preserve or tighten execution.boundaries: {reason}"
                ));
            }
        }

        if self.terminal.backend != "local" {
            issues.push("terminal.backend must be local in this release".to_string());
        }
        if self.terminal.timeout == 0 {
            issues.push("terminal.timeout must be greater than zero".to_string());
        } else if self.terminal.timeout > MAX_TERMINAL_TIMEOUT_SECS {
            issues.push(format!(
                "terminal.timeout must be at most {MAX_TERMINAL_TIMEOUT_SECS}"
            ));
        }

        if self.compression.enabled {
            if !(0.0..=1.0).contains(&self.compression.threshold)
                || self.compression.threshold == 0.0
            {
                issues.push("compression.threshold must be in (0, 1]".to_string());
            }
            if !(0.0..1.0).contains(&self.compression.target_ratio)
                || self.compression.target_ratio == 0.0
            {
                issues.push("compression.target_ratio must be in (0, 1)".to_string());
            }
            if self.compression.target_ratio >= self.compression.threshold {
                issues.push(
                    "compression.target_ratio must be lower than compression.threshold".to_string(),
                );
            }
        }

        if self.memory.enabled && !matches!(self.memory.provider.as_str(), "built-in" | "json") {
            issues.push("memory.provider must be built-in or json".to_string());
        }
        if !matches!(
            self.approvals.mode.as_str(),
            "manual" | "smart" | "policy" | "off"
        ) {
            issues.push("approvals.mode must be manual, smart, policy, or off".to_string());
        }
        if !self.workload.enabled {
            issues.push("workload.enabled must remain true for auditable execution".to_string());
        }
        if !self.workload.require_reconciliation {
            issues.push(
                "workload.require_reconciliation must remain true for auditable execution"
                    .to_string(),
            );
        }
        if !matches!(self.workload.store.as_str(), "sessions" | "json") {
            issues.push("workload.store must be sessions or json".to_string());
        }
        if self.daemons.retention_days < 0 {
            issues.push("daemons.retention_days must not be negative".to_string());
        } else if self
            .daemons
            .retention_days
            .checked_mul(86_400_000)
            .is_none()
        {
            issues.push("daemons.retention_days is too large".to_string());
        }
        if self.daemons.cron_enabled && self.daemons.interval_seconds == 0 {
            issues.push("daemons.interval_seconds must be greater than zero".to_string());
        }
        if self.skills.paths.len() > MAX_SKILL_PATHS {
            issues.push(format!(
                "skills.paths must contain at most {MAX_SKILL_PATHS} paths"
            ));
        }
        if self.skills.paths.iter().any(|path| {
            (self.skills.enabled && path.as_os_str().is_empty())
                || path_bytes(path) > MAX_PATH_BYTES
                || path_contains_nul(path)
        }) {
            issues.push(format!(
                "skills.paths entries must be non-empty, at most {MAX_PATH_BYTES} bytes, and contain no NUL"
            ));
        }

        if self.profiles.default.trim().is_empty() {
            issues.push("profiles.default must not be empty".to_string());
        } else if !is_safe_identifier(&self.profiles.default) {
            issues.push(format!(
                "profiles.default must be at most {MAX_IDENTIFIER_BYTES} bytes and contain only supported identifier characters"
            ));
        }
        if self.profiles.active.len() > MAX_PROFILES {
            issues.push(format!(
                "profiles.active must contain at most {MAX_PROFILES} entries"
            ));
        }
        let mut profile_ids = std::collections::HashSet::new();
        for profile in &self.profiles.active {
            if profile.id.trim().is_empty() {
                issues.push("profiles.active[].id must not be empty".to_string());
            } else if !is_safe_identifier(&profile.id) {
                issues.push(format!(
                    "profile {} id must be at most {MAX_IDENTIFIER_BYTES} bytes and contain only supported identifier characters",
                    profile.id
                ));
            } else if !profile_ids.insert(profile.id.as_str()) {
                issues.push(format!("duplicate profile id: {}", profile.id));
            }
            if profile.root.as_os_str().is_empty() {
                issues.push(format!("profile {} root must not be empty", profile.id));
            } else if path_bytes(&profile.root) > MAX_PATH_BYTES || path_contains_nul(&profile.root)
            {
                issues.push(format!(
                    "profile {} root must be at most {MAX_PATH_BYTES} bytes and contain no NUL",
                    profile.id
                ));
            }
            if profile.active_skills.len() > MAX_ACTIVE_SKILLS {
                issues.push(format!(
                    "profile {} active_skills must contain at most {MAX_ACTIVE_SKILLS} entries",
                    profile.id
                ));
            }
            if profile
                .active_skills
                .iter()
                .any(|skill| !is_safe_identifier(skill))
            {
                issues.push(format!(
                    "profile {} active_skills entries must be at most {MAX_IDENTIFIER_BYTES} bytes and contain only supported identifier characters",
                    profile.id
                ));
            }
            if profile.skill_paths.len() > MAX_SKILL_PATHS {
                issues.push(format!(
                    "profile {} skill_paths must contain at most {MAX_SKILL_PATHS} entries",
                    profile.id
                ));
            }
            for (field, path) in [
                ("env_file", profile.env_file.as_ref()),
                ("state_dir", profile.state_dir.as_ref()),
            ] {
                if path.is_some_and(|path| {
                    !is_scoped_relative_path(path)
                        || path_bytes(path) > MAX_PATH_BYTES
                        || path_contains_nul(path)
                }) {
                    issues.push(format!(
                        "profile {} {field} must be scoped relative, at most {MAX_PATH_BYTES} bytes, and contain no NUL",
                        profile.id
                    ));
                }
            }
            if profile.skill_paths.iter().any(|path| {
                !is_scoped_relative_path(path)
                    || path_bytes(path) > MAX_PATH_BYTES
                    || path_contains_nul(path)
            }) {
                issues.push(format!(
                    "profile {} skill_paths must be scoped relative, at most {MAX_PATH_BYTES} bytes, and contain no NUL",
                    profile.id
                ));
            }
        }
        if !self.profiles.active.is_empty() && !profile_ids.contains(self.profiles.default.as_str())
        {
            issues.push(format!(
                "profiles.default references unknown profile: {}",
                self.profiles.default
            ));
        }

        if self.mcp.servers.len() > MAX_MCP_CONFIGURED_SERVERS {
            issues.push(format!(
                "mcp.servers must contain at most {MAX_MCP_CONFIGURED_SERVERS} entries"
            ));
        }
        for (name, server) in &self.mcp.servers {
            if name.trim().is_empty() {
                issues.push("mcp.servers must not contain an empty server name".to_string());
            } else if !is_valid_mcp_server_name(name) {
                issues.push(format!(
                    "mcp server name '{name}' must be at most {MAX_MCP_SERVER_NAME_BYTES} bytes and contain only ASCII letters, digits, '.', '-', or '_'"
                ));
            }
            if server.command.trim().is_empty() {
                issues.push(format!("mcp.servers.{name}.command must not be empty"));
            } else if server.command.len() > MAX_MCP_COMMAND_BYTES || server.command.contains('\0')
            {
                issues.push(format!(
                    "mcp.servers.{name}.command must be at most {MAX_MCP_COMMAND_BYTES} bytes and contain no NUL"
                ));
            }
            if server.request_timeout_secs == 0 {
                issues.push(format!(
                    "mcp.servers.{name}.request_timeout_secs must be greater than zero"
                ));
            } else if server.request_timeout_secs > MAX_MCP_REQUEST_TIMEOUT_SECS {
                issues.push(format!(
                    "mcp.servers.{name}.request_timeout_secs must be at most {MAX_MCP_REQUEST_TIMEOUT_SECS}"
                ));
            }
            if server
                .cwd
                .as_ref()
                .is_some_and(|path| path.as_os_str().is_empty())
            {
                issues.push(format!("mcp.servers.{name}.cwd must not be empty"));
            }
            if server
                .cwd
                .as_ref()
                .is_some_and(|path| path_bytes(path) > MAX_PATH_BYTES || path_contains_nul(path))
            {
                issues.push(format!(
                    "mcp.servers.{name}.cwd must be at most {MAX_PATH_BYTES} bytes and contain no NUL"
                ));
            }
            if server.args.len() > MAX_MCP_ARGUMENTS {
                issues.push(format!(
                    "mcp.servers.{name}.args must contain at most {MAX_MCP_ARGUMENTS} entries"
                ));
            }
            if server
                .args
                .iter()
                .any(|argument| argument.len() > MAX_MCP_ARGUMENT_BYTES || argument.contains('\0'))
            {
                issues.push(format!(
                    "mcp.servers.{name}.args entries must be at most {MAX_MCP_ARGUMENT_BYTES} bytes and contain no NUL"
                ));
            }
            let argument_bytes = server.args.iter().fold(0usize, |total, argument| {
                total.saturating_add(argument.len())
            });
            if argument_bytes > MAX_MCP_ARGUMENT_BYTES_TOTAL {
                issues.push(format!(
                    "mcp.servers.{name}.args exceed the {MAX_MCP_ARGUMENT_BYTES_TOTAL}-byte aggregate limit"
                ));
            }
            if server.env.len() > MAX_MCP_ENV_VARS {
                issues.push(format!(
                    "mcp.servers.{name}.env must contain at most {MAX_MCP_ENV_VARS} entries"
                ));
            }
            if server.env.keys().any(|key| !is_valid_environment_name(key)) {
                issues.push(format!(
                    "mcp.servers.{name}.env contains an invalid environment variable name"
                ));
            }
            if server
                .env
                .values()
                .any(|value| value.len() > MAX_ENV_VALUE_BYTES || value.contains('\0'))
            {
                issues.push(format!(
                    "mcp.servers.{name}.env values must be at most {MAX_ENV_VALUE_BYTES} bytes and contain no NUL"
                ));
            }
            let environment_bytes = server.env.iter().fold(0usize, |total, (key, value)| {
                total.saturating_add(key.len()).saturating_add(value.len())
            });
            if environment_bytes > MAX_MCP_ENV_BYTES_TOTAL {
                issues.push(format!(
                    "mcp.servers.{name}.env exceeds the {MAX_MCP_ENV_BYTES_TOTAL}-byte aggregate limit"
                ));
            }
        }

        if issues.is_empty() {
            Ok(())
        } else {
            Err(ConfigValidationError { issues })
        }
    }

    pub fn sensitive_values(&self) -> Vec<String> {
        let mut values = HashSet::new();
        for provider in self.llm.providers.values() {
            if let Some(value) = &provider.api_key {
                if !value.trim().is_empty() {
                    values.insert(value.clone());
                }
            }
            values.extend(
                provider
                    .api_keys
                    .iter()
                    .filter(|value| !value.trim().is_empty())
                    .cloned(),
            );
        }
        for server in self.mcp.servers.values() {
            for (key, value) in &server.env {
                if is_sensitive_environment_name(key) && !value.trim().is_empty() {
                    values.insert(value.clone());
                }
            }
        }

        let mut values = values.into_iter().collect::<Vec<_>>();
        values.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
        values
    }

    /// Rejects a public session identifier that would disclose configured sensitive data.
    ///
    /// Session identifiers are rendered by CLI and interactive lifecycle surfaces and are
    /// persisted as filenames. Keep this check fail-closed and return a constant diagnostic so
    /// the rejected identifier cannot be reflected by the validation path itself.
    pub fn validate_public_session_id(&self, session_id: &str) -> Result<(), String> {
        let redacted = crate::tools::executor::redact_text_with_encoded_sensitive_values(
            session_id,
            self.public_session_sensitive_values(),
        );
        if redacted != session_id {
            Err("session identifier conflicts with configured sensitive data".to_string())
        } else {
            Ok(())
        }
    }

    pub fn public_session_sensitive_values(&self) -> Vec<String> {
        let mut values = self.sensitive_values();
        values.extend(crate::llm::factory::provider_environment_credentials());
        values.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
        values.dedup();
        values
    }
}

fn validate_boundary_config(field: &str, boundaries: &BoundaryConfig, issues: &mut Vec<String>) {
    if !matches!(
        boundaries.network.as_str(),
        "restricted" | "enabled" | "disabled"
    ) {
        issues.push(format!(
            "{field}.network must be restricted, enabled, or disabled"
        ));
    }
    if boundaries.allow_write.len() > MAX_BOUNDARY_PATHS {
        issues.push(format!(
            "{field}.allow_write must contain at most {MAX_BOUNDARY_PATHS} paths"
        ));
    }
    if boundaries
        .allow_write
        .iter()
        .any(|path| path.trim().is_empty() || path.len() > MAX_PATH_BYTES || path.contains('\0'))
    {
        issues.push(format!(
            "{field}.allow_write paths must be non-empty, at most {MAX_PATH_BYTES} bytes, and contain no NUL"
        ));
    }
}

pub(crate) fn boundary_profile_tightening_error(
    configured: &BoundaryConfig,
    selected: &BoundaryConfig,
) -> Result<(), String> {
    fn network_rank(network: &str) -> Option<u8> {
        match network {
            "enabled" => Some(0),
            "restricted" => Some(1),
            "disabled" => Some(2),
            _ => None,
        }
    }

    let configured_rank = network_rank(&configured.network)
        .ok_or_else(|| "configured network policy is invalid".to_string())?;
    let selected_rank = network_rank(&selected.network)
        .ok_or_else(|| "selected network policy is invalid".to_string())?;
    if selected_rank < configured_rank {
        return Err(format!(
            "network policy '{}' would weaken configured policy '{}'",
            selected.network, configured.network
        ));
    }

    let configured_writes: HashSet<&str> =
        configured.allow_write.iter().map(String::as_str).collect();
    if let Some(path) = selected
        .allow_write
        .iter()
        .find(|path| !configured_writes.contains(path.as_str()))
    {
        return Err(format!(
            "writable path '{path}' is not present in execution.boundaries.allow_write"
        ));
    }

    Ok(())
}

fn is_scoped_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && !path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

fn is_safe_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_BYTES
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

fn configured_endpoint_conflicts(base_url: &str, mode: LlmApiMode) -> bool {
    let path = base_url
        .split(['?', '#'])
        .next()
        .unwrap_or(base_url)
        .trim_end_matches('/');
    let terminal_mode = configured_terminal_api_mode(path);
    terminal_mode.is_some_and(|terminal| terminal != mode)
        || terminal_mode.is_some_and(|terminal| {
            let prefix = path
                .strip_suffix(terminal.endpoint_suffix())
                .expect("terminal mode suffix");
            configured_terminal_api_mode(prefix).is_some()
        })
}

fn configured_endpoint_issue(base_url: &str, mode: LlmApiMode) -> Option<&'static str> {
    let Ok(parsed) = reqwest::Url::parse(base_url.trim()) else {
        return Some("must be an absolute HTTP(S) URL");
    };
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Some("must be an absolute HTTP(S) URL");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Some("must not contain embedded credentials");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Some("must not contain a query string or fragment");
    }
    let path = parsed.path().trim_end_matches('/');
    let Some(decoded_path) = configured_percent_decode_repeated(path) else {
        return Some("percent-encoding nesting exceeds the supported limit");
    };
    if decoded_path != path
        && configured_terminal_api_modes(&decoded_path) != configured_terminal_api_modes(path)
    {
        return Some("API endpoint suffix must not be percent-encoded");
    }
    configured_endpoint_conflicts(parsed.path(), mode)
        .then_some("conflicts with the selected api mode or contains a doubled API suffix")
}

fn configured_terminal_api_mode(path: &str) -> Option<LlmApiMode> {
    let path = path.trim_end_matches('/');
    [LlmApiMode::ChatCompletions, LlmApiMode::Responses]
        .into_iter()
        .find(|mode| path.ends_with(mode.endpoint_suffix()))
}

fn configured_terminal_api_modes(mut path: &str) -> Vec<LlmApiMode> {
    let mut modes = Vec::new();
    while let Some(mode) = configured_terminal_api_mode(path) {
        modes.push(mode);
        path = path
            .trim_end_matches('/')
            .strip_suffix(mode.endpoint_suffix())
            .expect("terminal mode suffix");
    }
    modes
}

fn configured_percent_decode_repeated(value: &str) -> Option<String> {
    const MAX_PERCENT_DECODE_PASSES: usize = 8;

    let mut decoded = value.to_string();
    let mut passes = 0;
    loop {
        let next = configured_percent_decode_once(&decoded);
        if next == decoded {
            return Some(decoded);
        }
        if passes == MAX_PERCENT_DECODE_PASSES {
            return None;
        }
        decoded = next;
        passes += 1;
    }
}

fn configured_percent_decode_once(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (
                configured_hex_value(bytes[index + 1]),
                configured_hex_value(bytes[index + 2]),
            ) {
                decoded.push((high << 4) | low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn configured_hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn path_bytes(path: &Path) -> usize {
    path.as_os_str().as_encoded_bytes().len()
}

fn path_contains_nul(path: &Path) -> bool {
    path.as_os_str().as_encoded_bytes().contains(&0)
}

fn is_valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    name.len() <= MAX_ENV_NAME_BYTES
        && (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn is_sensitive_environment_name(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    [
        "API_KEY",
        "APIKEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "CREDENTIAL",
        "PRIVATE_KEY",
    ]
    .iter()
    .any(|marker| name.contains(marker))
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
        let provider_id = provider.unwrap_or(active.as_str());
        let configured = self.providers.get(provider_id);
        let descriptor = crate::llm::registry::provider_descriptor(provider_id);
        let mut models = configured
            .and_then(|entry| entry.models.clone())
            .or_else(|| descriptor.map(|provider| provider.models().to_vec()))
            .unwrap_or_default();
        let selected_model = configured
            .map(|entry| entry.model.as_str())
            .or_else(|| descriptor.map(|provider| provider.default_model()));
        if let Some(selected_model) = selected_model {
            if !models.iter().any(|model| model == selected_model) {
                models.insert(0, selected_model.to_string());
            }
        }
        models
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
                    models: None,
                    api_key: None,
                    api_keys: Vec::new(),
                    base_url: None,
                    api: None,
                    reasoning_effort: None,
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
        let is_new = !self.providers.contains_key(&provider);
        let default_api = is_new
            .then(|| {
                crate::llm::registry::provider_descriptor(&provider)
                    .and_then(|provider| provider.auth_api_default)
            })
            .flatten();
        let entry = self
            .providers
            .entry(provider.clone())
            .or_insert_with(|| ProviderEntry {
                model: model.clone(),
                models: None,
                api_key: None,
                api_keys: Vec::new(),
                base_url: None,
                api: default_api,
                reasoning_effort: None,
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

/// Loads the LLM configuration, using validated defaults only when no config exists.
///
/// The compatibility name describes the missing-file behavior; corrupt or unsafe
/// configuration state is returned to the caller rather than replaced by defaults.
pub fn load_config_or_default(project_root: &Path) -> Result<LlmConfig, ConfigError> {
    load_nib_config_full(project_root).map(|config| config.llm)
}

/// Loads the complete configuration, using validated defaults only when no config exists.
///
/// Corrupt, malformed, detached, or otherwise unsafe configuration state is returned
/// to the caller rather than replaced by defaults.
pub fn load_nib_config_or_default(project_root: &Path) -> Result<NibConfig, ConfigError> {
    load_nib_config_full(project_root)
}

pub fn load_nib_config_full(project_root: &Path) -> Result<NibConfig, ConfigError> {
    load_nib_config_full_with_source(project_root).map(|(config, _)| config)
}

pub fn load_nib_config_full_with_source(
    project_root: &Path,
) -> Result<(NibConfig, ConfigSource), ConfigError> {
    with_config_lock(project_root, |paths, directory| {
        load_nib_config_with_source_unlocked(paths, directory)
            .map(|loaded| (loaded.config, loaded.source))
    })
}

pub fn load_config_with_source(
    project_root: &Path,
) -> Result<(LlmConfig, ConfigSource), ConfigError> {
    with_config_lock(project_root, |paths, directory| {
        load_nib_config_with_source_unlocked(paths, directory)
            .map(|loaded| (loaded.config.llm, loaded.source))
    })
}

pub fn save_config(project_root: &Path, llm: &LlmConfig) -> Result<(), ConfigError> {
    let llm = llm.clone();
    update_nib_config(project_root, move |config| {
        config.llm = llm;
        Ok(())
    })
}

pub fn save_nib_config_full(project_root: &Path, cfg: &mut NibConfig) -> Result<(), ConfigError> {
    cfg.validate()?;
    let committed_revision = cfg
        .revision
        .checked_add(1)
        .ok_or_else(|| ConfigError::Operation("configuration revision overflowed".to_string()))?;
    with_config_lock(project_root, |paths, directory| {
        // A direct save must not turn an unreadable on-disk configuration into
        // an apparently successful default or replacement.
        let loaded = load_nib_config_with_source_unlocked(paths, directory)?;
        if cfg.revision != loaded.config.revision {
            return Err(ConfigError::Operation(format!(
                "stale configuration revision: snapshot={}, current={}",
                cfg.revision, loaded.config.revision
            )));
        }
        let mut next = cfg.clone();
        next.revision = committed_revision;
        save_nib_config_atomic(directory, &paths.toml, &next, loaded.expectation())
    })?;
    cfg.revision = committed_revision;
    Ok(())
}

/// Result of a locked configuration edit that may intentionally avoid a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigMutation<T> {
    /// Return the operation result without validating, writing, or advancing revision.
    Unchanged(T),
    /// Validate and atomically commit the edited configuration.
    Changed(T),
}

pub fn update_nib_config<T>(
    project_root: &Path,
    operation: impl FnOnce(&mut NibConfig) -> Result<T, String>,
) -> Result<T, ConfigError> {
    update_nib_config_conditionally(project_root, |config| {
        operation(config).map(ConfigMutation::Changed)
    })
}

/// Edit the latest configuration under its lock and commit only when requested.
///
/// Returning [`ConfigMutation::Unchanged`] discards any in-memory edits made by the
/// operation and leaves both the file and its revision untouched.
pub fn update_nib_config_conditionally<T>(
    project_root: &Path,
    operation: impl FnOnce(&mut NibConfig) -> Result<ConfigMutation<T>, String>,
) -> Result<T, ConfigError> {
    with_config_lock(project_root, |paths, directory| {
        let mut loaded = load_nib_config_with_source_unlocked(paths, directory)?;
        let revision = loaded.config.revision;
        let output = match operation(&mut loaded.config).map_err(ConfigError::Operation)? {
            ConfigMutation::Unchanged(output) => return Ok(output),
            ConfigMutation::Changed(output) => output,
        };
        loaded.config.revision = revision.checked_add(1).ok_or_else(|| {
            ConfigError::Operation("configuration revision overflowed".to_string())
        })?;
        loaded.config.validate()?;
        save_nib_config_atomic(directory, &paths.toml, &loaded.config, loaded.expectation())?;
        Ok(output)
    })
}

pub fn edit_nib_config<T>(
    project_root: &Path,
    edit: impl FnOnce(&Path) -> Result<T, String>,
) -> Result<T, ConfigError> {
    with_config_lock(project_root, |paths, directory| {
        let loaded = load_nib_config_with_source_unlocked(paths, directory)?;
        let original = loaded.config;
        let committed_revision = original.revision.checked_add(1).ok_or_else(|| {
            ConfigError::Operation("configuration revision overflowed".to_string())
        })?;
        if !regular_file_exists(directory, &paths.toml)? {
            save_nib_config_atomic(
                directory,
                &paths.toml,
                &original,
                crate::daemons::state::FileExpectation::Missing,
            )?;
        }

        directory.verify_visible().map_err(config_state_error)?;
        let edited = match edit(&paths.toml) {
            Ok(output) => output,
            Err(error) => {
                restore_nib_config_atomic(directory, &paths.toml, &original).map_err(
                    |restore| ConfigError::Operation(format!("{error}; restore failed: {restore}")),
                )?;
                return Err(ConfigError::Operation(error));
            }
        };
        directory.verify_visible().map_err(config_state_error)?;

        match load_nib_config_file(directory, &paths.toml) {
            Ok((mut config, edited_file)) => {
                config.revision = committed_revision;
                save_nib_config_atomic(
                    directory,
                    &paths.toml,
                    &config,
                    crate::daemons::state::FileExpectation::Present(&edited_file),
                )?;
                Ok(edited)
            }
            Err(error) => {
                restore_nib_config_atomic(directory, &paths.toml, &original).map_err(
                    |restore| {
                        ConfigError::Operation(format!(
                            "edited config is invalid ({error}); restore failed: {restore}"
                        ))
                    },
                )?;
                Err(ConfigError::Operation(format!(
                    "edited config is invalid and the previous config was restored: {error}"
                )))
            }
        }
    })
}

fn with_config_lock<T>(
    project_root: &Path,
    operation: impl FnOnce(
        &ConfigPaths,
        &crate::daemons::state::StableDirectory,
    ) -> Result<T, ConfigError>,
) -> Result<T, ConfigError> {
    with_config_lock_with_hook(project_root, |_| Ok(()), operation)
}

fn with_config_lock_with_hook<T>(
    project_root: &Path,
    before_lock: impl FnOnce(&ConfigPaths) -> Result<(), ConfigError>,
    operation: impl FnOnce(
        &ConfigPaths,
        &crate::daemons::state::StableDirectory,
    ) -> Result<T, ConfigError>,
) -> Result<T, ConfigError> {
    let paths = config_paths(project_root);
    let directory_existed = paths.nib_dir.is_dir();
    crate::fs_security::ensure_directory_without_symlinks(&paths.nib_dir)?;
    if !directory_existed {
        if let Some(parent) = paths.nib_dir.parent() {
            sync_directory(parent)?;
        }
    }
    let expected_directory =
        crate::daemons::state::StableDirectory::open(&paths.nib_dir).map_err(config_state_error)?;
    before_lock(&paths)?;

    let normalized = normalized_config_path(&paths.toml)?;
    let process_lock = config_process_lock(&normalized)?;
    let _guard = process_lock
        .lock()
        .map_err(|_| ConfigError::LockPoisoned(normalized.display().to_string()))?;
    let lock_path = paths.nib_dir.join("config.toml.lock");
    let mut operation_error = None;
    let result =
        crate::daemons::state::with_file_lock_in(&lock_path, &paths.nib_dir, |directory| {
            if !directory.same_identity(&expected_directory) {
                return Err(format!(
                    "configuration directory identity changed before lock acquisition: {}",
                    paths.nib_dir.display()
                ));
            }
            expected_directory.recover_stale_temporary_files(
                ".config.toml.tmp-",
                MAX_CONFIG_DIRECTORY_ENTRIES,
                MAX_CONFIG_DIRECTORY_NAME_BYTES,
            )?;
            match operation(&paths, &expected_directory) {
                Ok(value) => Ok(value),
                Err(error) => {
                    let message = error.to_string();
                    operation_error = Some(error);
                    Err(format!("{CONFIG_OPERATION_ERROR_SENTINEL}{message}"))
                }
            }
        });
    match result {
        Ok(value) => Ok(value),
        Err(error) => match operation_error {
            Some(operation_error) if error.starts_with(CONFIG_OPERATION_ERROR_SENTINEL) => {
                Err(operation_error)
            }
            Some(operation_error) => Err(ConfigError::Operation(format!(
                "{operation_error}; configuration state verification failed: {error}"
            ))),
            None => Err(config_state_error(error)),
        },
    }
}

fn config_process_lock(path: &Path) -> Result<Arc<ConfigMutex>, ConfigError> {
    let registry = CONFIG_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = registry
        .lock()
        .map_err(|_| ConfigError::LockPoisoned(path.display().to_string()))?;
    registry.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = registry.get(path).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    registry.insert(path.to_path_buf(), Arc::downgrade(&lock));
    Ok(lock)
}

fn normalized_config_path(path: &Path) -> Result<PathBuf, ConfigError> {
    let parent = path.parent().ok_or_else(|| {
        ConfigError::Operation(format!(
            "configuration path has no parent: {}",
            path.display()
        ))
    })?;
    let parent = parent.canonicalize()?;
    let file_name = path.file_name().ok_or_else(|| {
        ConfigError::Operation(format!(
            "configuration path has no file name: {}",
            path.display()
        ))
    })?;
    Ok(parent.join(file_name))
}

struct LoadedNibConfig {
    config: NibConfig,
    source: ConfigSource,
    toml_file: Option<File>,
}

impl LoadedNibConfig {
    fn expectation(&self) -> crate::daemons::state::FileExpectation<'_> {
        self.toml_file.as_ref().map_or(
            crate::daemons::state::FileExpectation::Missing,
            crate::daemons::state::FileExpectation::Present,
        )
    }
}

fn load_nib_config_with_source_unlocked(
    paths: &ConfigPaths,
    directory: &crate::daemons::state::StableDirectory,
) -> Result<LoadedNibConfig, ConfigError> {
    if regular_file_exists(directory, &paths.toml)? {
        let (config, file) = load_nib_config_file(directory, &paths.toml)?;
        backup_legacy_json(paths, directory, None)?;
        return Ok(LoadedNibConfig {
            config,
            source: ConfigSource::Toml,
            toml_file: Some(file),
        });
    }

    if regular_file_exists(directory, &paths.json)? {
        return migrate_json_to_toml_unlocked(paths, directory);
    }

    let config = NibConfig::default();
    config.validate()?;
    Ok(LoadedNibConfig {
        config,
        source: ConfigSource::Default,
        toml_file: None,
    })
}

fn load_nib_config_file(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
) -> Result<(NibConfig, File), ConfigError> {
    let (content, file) = read_regular_file(directory, path)?;
    let config: NibConfig = toml::from_str(&content)?;
    config.validate()?;
    Ok((config, file))
}

fn read_regular_file(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
) -> Result<(String, File), ConfigError> {
    let file = directory.open_read(path).map_err(config_state_error)?;
    let opened_metadata = file.metadata()?;
    validate_config_file_metadata(path, &opened_metadata)?;
    #[cfg(test)]
    run_config_read_hook(path)?;
    let mut bytes = Vec::with_capacity(opened_metadata.len() as usize);
    (&file)
        .take(MAX_CONFIG_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CONFIG_FILE_BYTES {
        return Err(ConfigError::FileTooLarge {
            path: path.display().to_string(),
            size: bytes.len() as u64,
            max: MAX_CONFIG_FILE_BYTES,
        });
    }
    directory
        .verify_file_identity(path, &file)
        .map_err(config_state_error)?;
    directory.verify_visible().map_err(config_state_error)?;
    let contents = String::from_utf8(bytes).map_err(|error| {
        ConfigError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    })?;
    Ok((contents, file))
}

fn validate_config_file_metadata(path: &Path, metadata: &fs::Metadata) -> Result<(), ConfigError> {
    if crate::fs_security::metadata_is_link_or_reparse(metadata) || !metadata.is_file() {
        return Err(ConfigError::InvalidFileType(path.display().to_string()));
    }
    if metadata.len() > MAX_CONFIG_FILE_BYTES {
        return Err(ConfigError::FileTooLarge {
            path: path.display().to_string(),
            size: metadata.len(),
            max: MAX_CONFIG_FILE_BYTES,
        });
    }
    Ok(())
}

fn regular_file_exists(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
) -> Result<bool, ConfigError> {
    directory.path_exists(path).map_err(config_state_error)
}

fn save_nib_config_atomic(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    config: &NibConfig,
    expected: crate::daemons::state::FileExpectation<'_>,
) -> Result<(), ConfigError> {
    save_nib_config_atomic_with_hook(directory, path, config, expected, || Ok(()))
}

fn save_nib_config_atomic_with_hook(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    config: &NibConfig,
    expected: crate::daemons::state::FileExpectation<'_>,
    before_commit: impl FnOnce() -> Result<(), String>,
) -> Result<(), ConfigError> {
    config.validate()?;
    let content = toml::to_string_pretty(config)?;
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
            ".config.toml.tmp-",
            true,
            expected,
            before_commit,
        )
        .map_err(config_state_error)
}

fn restore_nib_config_atomic(
    directory: &crate::daemons::state::StableDirectory,
    path: &Path,
    config: &NibConfig,
) -> Result<(), ConfigError> {
    let current = if regular_file_exists(directory, path)? {
        Some(directory.open_read(path).map_err(config_state_error)?)
    } else {
        None
    };
    let expected = current.as_ref().map_or(
        crate::daemons::state::FileExpectation::Missing,
        crate::daemons::state::FileExpectation::Present,
    );
    save_nib_config_atomic(directory, path, config, expected)
}

fn migrate_json_to_toml_unlocked(
    paths: &ConfigPaths,
    directory: &crate::daemons::state::StableDirectory,
) -> Result<LoadedNibConfig, ConfigError> {
    let (content, json_file) = read_regular_file(directory, &paths.json)?;
    let llm: LlmConfig = serde_json::from_str(&content)?;
    let config = NibConfig {
        revision: 1,
        llm: llm.clone(),
        ..NibConfig::default()
    };
    config.validate()?;
    save_nib_config_atomic(
        directory,
        &paths.toml,
        &config,
        crate::daemons::state::FileExpectation::Missing,
    )?;
    backup_legacy_json(paths, directory, Some(&json_file))?;
    let (config, file) = load_nib_config_file(directory, &paths.toml)?;
    Ok(LoadedNibConfig {
        config,
        source: ConfigSource::MigratedFromJson,
        toml_file: Some(file),
    })
}

fn backup_legacy_json(
    paths: &ConfigPaths,
    directory: &crate::daemons::state::StableDirectory,
    expected_source: Option<&File>,
) -> Result<(), ConfigError> {
    if !regular_file_exists(directory, &paths.json)? {
        return Ok(());
    }
    let owned_source;
    let source = match expected_source {
        Some(source) => source,
        None => {
            owned_source = directory
                .open_read(&paths.json)
                .map_err(config_state_error)?;
            &owned_source
        }
    };
    directory
        .verify_file_identity(&paths.json, source)
        .map_err(config_state_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let writable_source = directory
            .open_read_write(&paths.json)
            .map_err(config_state_error)?;
        directory
            .verify_file_identity(&paths.json, source)
            .map_err(config_state_error)?;
        writable_source.set_permissions(fs::Permissions::from_mode(0o600))?;
        writable_source.sync_all()?;
        directory
            .verify_file_identity(&paths.json, source)
            .map_err(config_state_error)?;
    }
    if regular_file_exists(directory, &paths.json_backup)? {
        let backup = directory
            .open_read(&paths.json_backup)
            .map_err(config_state_error)?;
        directory
            .remove_file_if_matches(&paths.json_backup, &backup, ".config-backup-delete-")
            .map_err(config_state_error)?;
    }
    directory
        .rename_file_if_matches(&paths.json, &paths.json_backup, source)
        .map_err(config_state_error)?;
    Ok(())
}

fn config_state_error(error: String) -> ConfigError {
    ConfigError::Operation(error)
}

fn sync_directory(_path: &Path) -> Result<(), ConfigError> {
    #[cfg(unix)]
    File::open(_path)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[cfg(unix)]
    const CONFIG_COMMIT_CHILD_ROOT: &str = "NIB_CONFIG_COMMIT_CHILD_ROOT";
    #[cfg(unix)]
    const CONFIG_COMMIT_CHILD_MODE: &str = "NIB_CONFIG_COMMIT_CHILD_MODE";
    #[cfg(unix)]
    const CONFIG_COMMIT_CHILD_READY: &str = "NIB_CONFIG_COMMIT_CHILD_READY";
    #[cfg(unix)]
    const CONFIG_COMMIT_CHILD_RELEASE: &str = "NIB_CONFIG_COMMIT_CHILD_RELEASE";

    #[test]
    fn toml_roundtrip_preserves_llm_config() {
        let llm = LlmConfig {
            active_provider: Some("openai".to_string()),
            providers: HashMap::from([(
                "openai".to_string(),
                ProviderEntry {
                    model: "gpt-4o".to_string(),
                    models: Some(vec!["gpt-4o".to_string(), "gateway/new-model".to_string()]),
                    api_key: Some("sk-test".to_string()),
                    api_keys: vec!["sk-backup".to_string()],
                    base_url: None,
                    api: Some(LlmApiMode::Responses),
                    reasoning_effort: Some(ReasoningEffort::Medium),
                },
            )]),
            context_length: 128_000,
        };
        let nib = NibConfig {
            llm: llm.clone(),
            ..NibConfig::default()
        };
        let serialized = toml::to_string_pretty(&nib).expect("serialize");
        let parsed: NibConfig = toml::from_str(&serialized).expect("parse");
        assert_eq!(parsed.llm, llm);
    }

    #[test]
    fn legacy_provider_defaults_to_chat_completions_without_rewriting() {
        let provider: ProviderEntry = toml::from_str(
            r#"
model = "gpt-5.6-luna"
api_key = "fixture"
"#,
        )
        .expect("legacy provider");

        assert_eq!(provider.api, None);
        assert_eq!(provider.models, None);
        assert_eq!(provider.reasoning_effort, None);
        assert_eq!(provider.resolved_api_mode(), LlmApiMode::ChatCompletions);
        let serialized = toml::to_string(&provider).expect("serialize legacy provider");
        assert!(!serialized.contains("api ="));
        assert!(!serialized.contains("reasoning_effort"));
        assert!(!serialized.contains("models"));
    }

    #[test]
    fn available_models_use_bundled_defaults_and_keep_selected_custom_models() {
        let bundled = LlmConfig::default().get_available_models(Some("openai"));
        assert_eq!(bundled, ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"]);

        let configured = LlmConfig {
            active_provider: Some("openai".to_string()),
            providers: HashMap::from([(
                "openai".to_string(),
                ProviderEntry {
                    model: "gateway/future-model".to_string(),
                    ..ProviderEntry::default()
                },
            )]),
            ..LlmConfig::default()
        };
        assert_eq!(
            configured.get_available_models(None),
            [
                "gateway/future-model",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
            ]
        );
    }

    #[test]
    fn configured_model_list_replaces_bundled_suggestions_in_order() {
        let configured = LlmConfig {
            active_provider: Some("openai".to_string()),
            providers: HashMap::from([(
                "openai".to_string(),
                ProviderEntry {
                    model: "gateway/selected".to_string(),
                    models: Some(vec![
                        "gateway/first".to_string(),
                        "gateway/second".to_string(),
                    ]),
                    ..ProviderEntry::default()
                },
            )]),
            ..LlmConfig::default()
        };
        assert_eq!(
            configured.get_available_models(None),
            ["gateway/selected", "gateway/first", "gateway/second"]
        );

        let mut empty_override = configured;
        empty_override.providers.get_mut("openai").unwrap().model = "gateway/second".to_string();
        assert_eq!(
            empty_override.get_available_models(None),
            ["gateway/first", "gateway/second"]
        );
        empty_override.providers.get_mut("openai").unwrap().models = Some(Vec::new());
        assert_eq!(
            empty_override.get_available_models(None),
            ["gateway/second"]
        );
    }

    #[test]
    fn configured_model_list_is_bounded_and_rejects_invalid_entries() {
        let mut config = NibConfig::default();
        config.llm.providers.insert(
            "openai".to_string(),
            ProviderEntry {
                model: "gpt-5.6-sol".to_string(),
                models: Some(
                    (0..MAX_PROVIDER_MODELS)
                        .map(|index| format!("model-{index}"))
                        .chain(std::iter::once("model-0".to_string()))
                        .chain(std::iter::once(String::new()))
                        .chain(std::iter::once("x".repeat(MAX_MODEL_BYTES + 1)))
                        .chain(std::iter::once("model\0unsafe".to_string()))
                        .collect(),
                ),
                ..ProviderEntry::default()
            },
        );

        let error = config
            .validate()
            .expect_err("invalid configured model list")
            .to_string();
        for expected in [
            "at most 128 entries",
            "empty model",
            "at most 512 bytes and contain no NUL",
            "duplicate model",
        ] {
            assert!(error.contains(expected), "missing {expected}: {error}");
        }
    }

    #[test]
    fn provider_api_validation_is_typed_scoped_and_suffix_safe() {
        let mut config = NibConfig::default();
        config.llm.active_provider = Some("openai".to_string());
        config.llm.providers.insert(
            "openai".to_string(),
            ProviderEntry {
                model: "gpt-5.6-luna".to_string(),
                api_key: Some("fixture".to_string()),
                base_url: Some("https://api.openai.com/v1/chat/completions".to_string()),
                api: Some(LlmApiMode::Responses),
                reasoning_effort: Some(ReasoningEffort::Medium),
                ..ProviderEntry::default()
            },
        );
        let error = config
            .validate()
            .expect_err("conflicting endpoint suffix")
            .to_string();
        assert!(
            error.contains("conflicts with api = 'responses'"),
            "{error}"
        );

        let provider = config.llm.providers.get_mut("openai").unwrap();
        provider.base_url = Some("https://api.openai.com/v1".to_string());
        config.validate().expect("matching root URL");
        config.llm.providers.get_mut("openai").unwrap().base_url =
            Some("https://gateway.test/proxy/responses/v1".to_string());
        config
            .validate()
            .expect("reserved nonterminal segment is a valid root URL");
        config.llm.providers.get_mut("openai").unwrap().base_url =
            Some("https://gateway.test/tenant/acme%20corp/v1/responses".to_string());
        config
            .validate()
            .expect("encoded tenant path is a valid full endpoint");

        for (url, expected) in [
            (
                "https://user:config-secret@example.test/v1",
                "embedded credentials",
            ),
            (
                "https://example.test/v1?token=config-secret",
                "query string or fragment",
            ),
            ("file:///tmp/openai", "absolute HTTP(S) URL"),
            (
                "https://example.test/v1/%72esponses",
                "must not be percent-encoded",
            ),
            (
                "https://example.test/v1/responses/%2572esponses",
                "must not be percent-encoded",
            ),
        ] {
            config.llm.providers.get_mut("openai").unwrap().base_url = Some(url.to_string());
            let error = config
                .validate()
                .expect_err("unsafe provider URL")
                .to_string();
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("config-secret"), "{error}");
        }
        config.llm.providers.get_mut("openai").unwrap().base_url =
            Some("https://api.openai.com/v1".to_string());

        let deeply_encoded_suffix = (0..9).fold("%72esponses".to_string(), |value, _| {
            value.replace('%', "%25")
        });
        config.llm.providers.get_mut("openai").unwrap().base_url =
            Some(format!("https://example.test/v1/{deeply_encoded_suffix}"));
        let error = config
            .validate()
            .expect_err("excessive percent-encoding nesting must fail closed")
            .to_string();
        assert!(
            error.contains("nesting exceeds the supported limit"),
            "{error}"
        );
        config.llm.providers.get_mut("openai").unwrap().base_url =
            Some("https://api.openai.com/v1".to_string());

        config.llm.active_provider = Some("anthropic".to_string());
        config.llm.providers.insert(
            "anthropic".to_string(),
            ProviderEntry {
                model: "claude".to_string(),
                api_key: Some("fixture".to_string()),
                api: Some(LlmApiMode::Responses),
                ..ProviderEntry::default()
            },
        );
        let error = config
            .validate()
            .expect_err("unused provider mode")
            .to_string();
        assert!(
            error.contains("supported only by OpenAI-compatible"),
            "{error}"
        );
    }

    #[test]
    fn new_openai_auth_defaults_to_responses_without_migrating_existing_entries() {
        let mut llm = LlmConfig::default();
        llm.add_or_update_provider(
            "openai".to_string(),
            "gpt-5.6-luna".to_string(),
            Some("fixture".to_string()),
        );
        assert_eq!(llm.providers["openai"].api, Some(LlmApiMode::Responses));

        let mut legacy = LlmConfig {
            providers: HashMap::from([(
                "openai".to_string(),
                ProviderEntry {
                    model: "gpt-4o".to_string(),
                    models: Some(vec!["gateway/model-a".to_string()]),
                    ..ProviderEntry::default()
                },
            )]),
            ..LlmConfig::default()
        };
        legacy.add_or_update_provider(
            "openai".to_string(),
            "gpt-5.6-luna".to_string(),
            Some("fixture".to_string()),
        );
        assert_eq!(legacy.providers["openai"].api, None);
        assert_eq!(
            legacy.providers["openai"].models.as_deref(),
            Some(["gateway/model-a".to_string()].as_slice())
        );
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
        assert_eq!(
            load_nib_config_full(dir.path())
                .expect("migrated full config")
                .revision,
            1
        );

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
                    api_keys: Vec::new(),
                    base_url: None,
                    ..ProviderEntry::default()
                },
            )]),
            context_length: 128_000,
        };
        save_config(dir.path(), &llm).expect("save");
        let loaded = load_config_or_default(dir.path()).expect("load saved config");
        assert_eq!(loaded, llm);
    }

    #[test]
    fn compatibility_loaders_default_only_when_configuration_is_missing() {
        let root = tempdir().expect("temporary config root");

        let llm = load_config_or_default(root.path()).expect("missing LLM config defaults");
        let full =
            load_nib_config_or_default(root.path()).expect("missing complete config defaults");
        let (_, source) =
            load_config_with_source(root.path()).expect("missing config source defaults");

        assert_eq!(llm, LlmConfig::default());
        assert_eq!(full, NibConfig::default());
        assert_eq!(source, ConfigSource::Default);
        assert!(!config_paths(root.path()).toml.exists());
    }

    #[test]
    fn compatibility_loaders_propagate_malformed_configuration() {
        let root = tempdir().expect("temporary config root");
        let paths = config_paths(root.path());
        fs::create_dir_all(&paths.nib_dir).expect("create config directory");
        fs::write(
            &paths.toml,
            b"[llm.providers.openai]\napi_key = \"doctor-parse-secret",
        )
        .expect("write malformed config");

        for error in [
            load_config_or_default(root.path()).unwrap_err(),
            load_nib_config_or_default(root.path()).unwrap_err(),
        ] {
            assert!(matches!(error, ConfigError::Toml(_)));
            let diagnostic = error.to_string();
            assert!(!diagnostic.contains("doctor-parse-secret"), "{diagnostic}");
            assert!(
                diagnostic.contains("source excerpt omitted"),
                "{diagnostic}"
            );
            assert!(!diagnostic.contains('\n'), "{diagnostic}");
        }
    }

    #[test]
    fn compatibility_loaders_propagate_unsafe_configuration_state() {
        let root = tempdir().expect("temporary config root");
        let paths = config_paths(root.path());
        let mut config = NibConfig::default();
        save_nib_config_full(root.path(), &mut config).expect("save valid config");

        let llm_hook = install_config_read_hook(paths.toml.clone(), |_| {
            Err("simulated configuration detachment".to_string())
        });
        let llm_error = load_config_or_default(root.path())
            .expect_err("detached LLM configuration must fail closed");
        drop(llm_hook);

        let full_hook = install_config_read_hook(paths.toml.clone(), |_| {
            Err("simulated configuration detachment".to_string())
        });
        let full_error = load_nib_config_or_default(root.path())
            .expect_err("detached complete configuration must fail closed");
        drop(full_hook);

        assert!(
            matches!(llm_error, ConfigError::Operation(ref message) if message.contains("detachment"))
        );
        assert!(
            matches!(full_error, ConfigError::Operation(ref message) if message.contains("detachment"))
        );
    }

    #[test]
    fn complete_defaults_are_valid_and_non_empty() {
        let cfg = NibConfig::default();

        cfg.validate().expect("default config must be valid");
        assert_eq!(cfg.agent.max_turns, 90);
        assert!(cfg.agent.tool_use_enforcement);
        assert_eq!(cfg.terminal.backend, "local");
        assert_eq!(cfg.terminal.timeout, 180);
        assert_eq!(cfg.approvals.mode, "manual");
        assert_eq!(cfg.workload.store, "sessions");
        assert_eq!(cfg.execution.provider, "hybrid");
        assert_eq!(cfg.execution.default_profile, "restricted");
        assert_eq!(cfg.execution.boundaries.network, "restricted");
        assert_eq!(cfg.profiles.default, "default");
        assert!(cfg.mcp.client_enabled);
        assert!(cfg.mcp.server_enabled);
    }

    #[test]
    fn named_boundary_profiles_roundtrip_and_only_tighten_the_base_boundary() {
        let mut cfg = NibConfig::default();
        cfg.execution.boundaries = BoundaryConfig {
            allow_write: vec!["build".to_string(), "cache".to_string()],
            network: "enabled".to_string(),
        };
        cfg.execution.boundary_profiles.insert(
            "offline-build".to_string(),
            BoundaryConfig {
                allow_write: vec!["build".to_string()],
                network: "disabled".to_string(),
            },
        );

        cfg.validate().expect("tightening profile is valid");
        let encoded = toml::to_string_pretty(&cfg).expect("serialize config");
        assert!(encoded.contains("[execution.boundary_profiles.offline-build]"));
        let decoded: NibConfig = toml::from_str(&encoded).expect("parse config");
        assert_eq!(decoded, cfg);
        decoded.validate().expect("roundtripped profile is valid");
    }

    #[test]
    fn named_boundary_profile_validation_rejects_weaker_or_reserved_profiles() {
        let mut cfg = NibConfig::default();
        cfg.execution.boundaries.allow_write = vec!["build".to_string()];
        cfg.execution.boundary_profiles.insert(
            "open-network".to_string(),
            BoundaryConfig {
                allow_write: Vec::new(),
                network: "enabled".to_string(),
            },
        );
        cfg.execution.boundary_profiles.insert(
            "extra-write".to_string(),
            BoundaryConfig {
                allow_write: vec!["outside".to_string()],
                network: "restricted".to_string(),
            },
        );
        cfg.execution
            .boundary_profiles
            .insert("internal".to_string(), BoundaryConfig::default());

        let message = cfg
            .validate()
            .expect_err("weaker profiles must be rejected")
            .to_string();
        assert!(message.contains("open-network") && message.contains("network policy"));
        assert!(message.contains("extra-write") && message.contains("writable path"));
        assert!(message.contains("internal") && message.contains("non-reserved identifier"));
    }

    #[cfg(unix)]
    #[test]
    fn saved_config_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().expect("tempdir");
        let mut config = NibConfig::default();
        save_nib_config_full(directory.path(), &mut config).expect("save config");
        let mode = fs::metadata(config_paths(directory.path()).toml)
            .expect("config metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn validation_reports_all_invalid_runtime_sections() {
        let mut cfg = NibConfig::default();
        cfg.agent.max_turns = 0;
        cfg.terminal.backend = "unknown".to_string();
        cfg.execution.provider = "unknown".to_string();
        cfg.execution.default_profile = "unknown".to_string();
        cfg.compression.threshold = 0.1;
        cfg.compression.target_ratio = 0.2;
        cfg.memory.provider = "sqlite".to_string();
        cfg.approvals.mode = "always".to_string();
        cfg.workload.enabled = false;
        cfg.workload.require_reconciliation = false;
        cfg.daemons.interval_seconds = 0;

        let error = cfg.validate().expect_err("config must be rejected");
        let message = error.to_string();
        assert!(message.contains("agent.max_turns"));
        assert!(message.contains("terminal.backend"));
        assert!(message.contains("execution.provider"));
        assert!(message.contains("execution.default_profile"));
        assert!(message.contains("compression.target_ratio"));
        assert!(message.contains("memory.provider"));
        assert!(message.contains("approvals.mode"));
        assert!(message.contains("workload.enabled"));
        assert!(message.contains("workload.require_reconciliation"));
        assert!(message.contains("daemons.interval_seconds"));
    }

    #[test]
    fn full_schema_roundtrips_through_toml() {
        let cfg = NibConfig {
            mcp: McpConfig {
                servers: HashMap::from([(
                    "local".to_string(),
                    McpServerEntry {
                        command: "mcp-server".to_string(),
                        args: vec!["--stdio".to_string()],
                        env: HashMap::from([("MODE".to_string(), "test".to_string())]),
                        cwd: Some(PathBuf::from("tools/mcp")),
                        request_timeout_secs: 45,
                    },
                )]),
                ..McpConfig::default()
            },
            profiles: ProfilesConfig {
                default: "workspace".to_string(),
                active: vec![ProfileConfig {
                    id: "workspace".to_string(),
                    root: PathBuf::from("."),
                    env_file: Some(PathBuf::from(".env.nib")),
                    active_skills: vec!["rust".to_string()],
                    skill_paths: vec![PathBuf::from("skills")],
                    state_dir: Some(PathBuf::from(".nib/profiles/workspace")),
                }],
            },
            ..NibConfig::default()
        };
        let encoded = toml::to_string_pretty(&cfg).expect("serialize full schema");
        let decoded: NibConfig = toml::from_str(&encoded).expect("parse full schema");

        assert_eq!(decoded, cfg);
        decoded.validate().expect("roundtripped config is valid");
    }

    #[test]
    fn mcp_deserialization_uses_operational_defaults() {
        let cfg: NibConfig = toml::from_str(
            r#"
[mcp.servers.local]
command = "mcp-server"
"#,
        )
        .expect("parse minimal MCP config");

        assert!(cfg.mcp.client_enabled);
        assert!(cfg.mcp.server_enabled);
        assert_eq!(cfg.mcp.servers["local"].request_timeout_secs, 30);
        cfg.validate().expect("minimal MCP config is valid");
    }

    #[test]
    fn mcp_validation_rejects_zero_timeout_and_empty_cwd() {
        let mut cfg = NibConfig::default();
        cfg.mcp.servers.insert(
            "broken".to_string(),
            McpServerEntry {
                command: "mcp-server".to_string(),
                cwd: Some(PathBuf::new()),
                request_timeout_secs: 0,
                ..McpServerEntry::default()
            },
        );

        let message = cfg
            .validate()
            .expect_err("invalid MCP process settings must be rejected")
            .to_string();
        assert!(message.contains("mcp.servers.broken.request_timeout_secs"));
        assert!(message.contains("mcp.servers.broken.cwd"));
    }

    #[test]
    fn mcp_validation_rejects_unsafe_names_excessive_timeouts_and_server_counts() {
        let mut cfg = NibConfig::default();
        cfg.mcp.servers.insert(
            "bad::name".to_string(),
            McpServerEntry {
                command: "mcp-server".to_string(),
                request_timeout_secs: MAX_MCP_REQUEST_TIMEOUT_SECS + 1,
                ..McpServerEntry::default()
            },
        );
        for index in 0..MAX_MCP_CONFIGURED_SERVERS {
            cfg.mcp.servers.insert(
                format!("server-{index}"),
                McpServerEntry {
                    command: "mcp-server".to_string(),
                    ..McpServerEntry::default()
                },
            );
        }

        let message = cfg
            .validate()
            .expect_err("unsafe MCP process settings must be rejected")
            .to_string();

        assert!(message.contains("at most 32 entries"));
        assert!(message.contains("bad::name"));
        assert!(message.contains("must be at most 3600"));
    }

    #[test]
    fn unknown_top_level_and_nested_config_keys_are_rejected() {
        let root = tempdir().expect("temporary config root");
        let paths = config_paths(root.path());
        fs::create_dir_all(&paths.nib_dir).expect("create config directory");
        fs::write(&paths.toml, "termnal = {}").expect("write typo config");
        let top_level =
            load_nib_config_full(root.path()).expect_err("top-level config typo must fail closed");
        let top_level = top_level.to_string();
        assert!(top_level.contains("invalid syntax or value"), "{top_level}");
        assert!(top_level.contains("source excerpt omitted"), "{top_level}");
        assert!(!top_level.contains("termnal"), "{top_level}");

        let nested = toml::from_str::<NibConfig>(
            r#"
[mcp.servers.local]
command = "mcp-server"
request_timeot_secs = 10
"#,
        )
        .expect_err("nested config typo must fail closed");
        assert!(nested
            .to_string()
            .contains("unknown field `request_timeot_secs`"));
    }

    #[test]
    fn config_resource_count_numeric_and_string_limits_are_enforced() {
        let mut config = NibConfig::default();
        config.llm.context_length = MAX_CONTEXT_LENGTH + 1;
        config.agent.max_turns = MAX_AGENT_TURNS + 1;
        config.terminal.timeout = MAX_TERMINAL_TIMEOUT_SECS + 1;
        config.llm.providers = (0..=MAX_PROVIDERS)
            .map(|index| {
                (
                    format!("provider-{index}"),
                    ProviderEntry {
                        model: "model".to_string(),
                        ..ProviderEntry::default()
                    },
                )
            })
            .collect();
        config.profiles.default = "profile-0".to_string();
        config.profiles.active = (0..=MAX_PROFILES)
            .map(|index| ProfileConfig {
                id: format!("profile-{index}"),
                root: PathBuf::from("."),
                ..ProfileConfig::default()
            })
            .collect();
        config.skills.paths = (0..=MAX_SKILL_PATHS)
            .map(|index| PathBuf::from(format!("skills/{index}")))
            .collect();
        config.execution.boundaries.allow_write = (0..=MAX_BOUNDARY_PATHS)
            .map(|index| format!("path/{index}"))
            .collect();

        let message = config
            .validate()
            .expect_err("oversized resource settings must fail")
            .to_string();

        for expected in [
            "llm.context_length",
            "agent.max_turns",
            "terminal.timeout",
            "llm.providers must contain at most",
            "profiles.active must contain at most",
            "skills.paths must contain at most",
            "execution.boundaries.allow_write must contain at most",
        ] {
            assert!(message.contains(expected), "missing {expected}: {message}");
        }
    }

    #[test]
    fn mcp_process_payload_and_environment_limits_are_enforced() {
        let mut config = NibConfig::default();
        config.mcp.servers.insert(
            "bounded".to_string(),
            McpServerEntry {
                command: "x".repeat(MAX_MCP_COMMAND_BYTES + 1),
                args: (0..=MAX_MCP_ARGUMENTS).map(|_| "arg".to_string()).collect(),
                env: HashMap::from([
                    ("INVALID-NAME".to_string(), "value".to_string()),
                    (
                        "VALID_NAME".to_string(),
                        "x".repeat(MAX_ENV_VALUE_BYTES + 1),
                    ),
                ]),
                cwd: Some(PathBuf::from("x".repeat(MAX_PATH_BYTES + 1))),
                ..McpServerEntry::default()
            },
        );

        let message = config
            .validate()
            .expect_err("oversized MCP settings must fail")
            .to_string();

        for expected in [".command", ".args", ".env", ".cwd"] {
            assert!(message.contains(expected), "missing {expected}: {message}");
        }
        assert!(message.contains("invalid environment variable name"));
    }

    #[test]
    fn provider_profile_and_skill_string_limits_are_enforced() {
        let mut config = NibConfig::default();
        let provider_name = "p".repeat(MAX_IDENTIFIER_BYTES + 1);
        config.llm.providers.insert(
            provider_name,
            ProviderEntry {
                model: "m".repeat(MAX_MODEL_BYTES + 1),
                api_key: Some("k".repeat(MAX_SECRET_BYTES + 1)),
                api_keys: (0..=MAX_PROVIDER_KEYS)
                    .map(|_| "backup".to_string())
                    .collect(),
                base_url: Some("u".repeat(MAX_URL_BYTES + 1)),
                ..ProviderEntry::default()
            },
        );
        config.profiles.default = "profile".to_string();
        config.profiles.active = vec![ProfileConfig {
            id: "profile".to_string(),
            root: PathBuf::from("r".repeat(MAX_PATH_BYTES + 1)),
            active_skills: vec!["s".repeat(MAX_IDENTIFIER_BYTES + 1)],
            skill_paths: vec![PathBuf::from("s".repeat(MAX_PATH_BYTES + 1))],
            ..ProfileConfig::default()
        }];

        let message = config
            .validate()
            .expect_err("oversized strings and paths must fail")
            .to_string();

        for expected in [
            "llm provider name",
            ".model",
            ".api_key",
            ".api_keys",
            ".base_url",
            "root must be at most",
            "active_skills entries",
            "skill_paths must be scoped relative",
        ] {
            assert!(message.contains(expected), "missing {expected}: {message}");
        }
    }

    #[test]
    fn mock_rejects_unused_transport_and_credential_fields_but_keeps_model_catalog_fields() {
        for entry in [
            ProviderEntry {
                model: "mock-model".to_string(),
                api_key: Some("private-primary".to_string()),
                ..ProviderEntry::default()
            },
            ProviderEntry {
                model: "mock-model".to_string(),
                api_keys: vec!["private-backup".to_string()],
                ..ProviderEntry::default()
            },
            ProviderEntry {
                model: "mock-model".to_string(),
                base_url: Some("http://127.0.0.1:9".to_string()),
                ..ProviderEntry::default()
            },
        ] {
            let mut config = NibConfig::default();
            config.llm.providers.insert("mock".to_string(), entry);
            config.llm.active_provider = Some("mock".to_string());
            let error = config
                .validate()
                .expect_err("unused Mock field")
                .to_string();
            assert!(error.contains("api_key, api_keys, and base_url are not supported"));
            assert!(!error.contains("private-"));
        }

        let mut valid = NibConfig::default();
        valid.llm.providers.insert(
            "mock".to_string(),
            ProviderEntry {
                model: "mock-model".to_string(),
                models: Some(vec!["mock-model".to_string(), "mock-alt".to_string()]),
                ..ProviderEntry::default()
            },
        );
        valid.llm.active_provider = Some("mock".to_string());
        valid
            .validate()
            .expect("Mock model catalog fields are consumed");
    }

    #[test]
    fn sensitive_values_are_deduplicated_and_sorted_longest_first() {
        let mut config = NibConfig::default();
        config.llm.providers.insert(
            "fixture".to_string(),
            ProviderEntry {
                model: "model".to_string(),
                api_key: Some("short-token".to_string()),
                api_keys: vec![
                    "longer-secret-value".to_string(),
                    "short-token".to_string(),
                    String::new(),
                ],
                base_url: None,
                ..ProviderEntry::default()
            },
        );
        config.mcp.servers.insert(
            "fixture".to_string(),
            McpServerEntry {
                command: "fixture".to_string(),
                env: HashMap::from([
                    (
                        "SERVICE_TOKEN".to_string(),
                        "longer-secret-value".to_string(),
                    ),
                    ("db_password".to_string(), "medium-secret".to_string()),
                    ("PUBLIC_VALUE".to_string(), "not-sensitive".to_string()),
                    ("EMPTY_SECRET".to_string(), " ".to_string()),
                ]),
                ..McpServerEntry::default()
            },
        );

        assert_eq!(
            config.sensitive_values(),
            ["longer-secret-value", "medium-secret", "short-token"]
        );
    }

    #[test]
    fn public_session_ids_cannot_embed_raw_or_encoded_credentials() {
        let mut config = NibConfig::default();
        config.llm.providers.insert(
            "inactive-openai".to_string(),
            ProviderEntry {
                model: "fixture".to_string(),
                api_key: Some("foo".to_string()),
                api_keys: vec!["active/credential".to_string()],
                ..ProviderEntry::default()
            },
        );

        config
            .validate_public_session_id("ordinary-session")
            .expect("ordinary identifier");
        for identifier in ["foo", "prefix-foo-suffix", "Zm9v", r#"active\/credential"#] {
            let error = config
                .validate_public_session_id(identifier)
                .expect_err("credential-derived session identifier");
            assert_eq!(
                error,
                "session identifier conflicts with configured sensitive data"
            );
            assert!(!error.contains("foo"));
            assert!(!error.contains("Zm9v"));
        }

        let previous = std::env::var_os("GOOGLE_API_KEY");
        std::env::set_var("GOOGLE_API_KEY", "private-env-session");
        let environment_error = config
            .validate_public_session_id("private-env-session")
            .expect_err("environment credential session identifier");
        match previous {
            Some(value) => std::env::set_var("GOOGLE_API_KEY", value),
            None => std::env::remove_var("GOOGLE_API_KEY"),
        }
        assert_eq!(
            environment_error,
            "session identifier conflicts with configured sensitive data"
        );
        assert!(!environment_error.contains("private-env-session"));
    }

    #[test]
    fn corrupt_config_cannot_be_replaced_by_save_or_update() {
        let root = tempdir().expect("temporary config root");
        let paths = config_paths(root.path());
        fs::create_dir_all(&paths.nib_dir).expect("config directory");
        let corrupt = b"not = [valid";
        fs::write(&paths.toml, corrupt).expect("corrupt config");

        let mut replacement = NibConfig::default();
        assert!(save_nib_config_full(root.path(), &mut replacement).is_err());
        assert!(update_nib_config(root.path(), |config| {
            config.agent.max_turns = 12;
            Ok(())
        })
        .is_err());
        assert_eq!(fs::read(&paths.toml).unwrap(), corrupt);
    }

    #[test]
    fn consecutive_config_saves_refresh_the_snapshot_revision() {
        let root = tempdir().expect("temporary config root");
        let mut config = NibConfig::default();
        config.agent.max_turns = 41;

        save_nib_config_full(root.path(), &mut config).expect("first save");
        assert_eq!(config.revision, 1);
        config.agent.max_turns = 42;
        save_nib_config_full(root.path(), &mut config).expect("second save");
        assert_eq!(config.revision, 2);

        let persisted = load_nib_config_full(root.path()).expect("persisted config");
        assert_eq!(persisted.revision, 2);
        assert_eq!(persisted.agent.max_turns, 42);
    }

    #[test]
    fn stale_config_snapshot_cannot_overwrite_a_newer_revision() {
        let root = tempdir().expect("temporary config root");
        let mut initial = NibConfig::default();
        save_nib_config_full(root.path(), &mut initial).expect("initial config");
        let mut first = load_nib_config_full(root.path()).expect("first snapshot");
        let mut stale = first.clone();
        first.agent.max_turns = 41;
        stale.agent.max_turns = 99;

        save_nib_config_full(root.path(), &mut first).expect("first snapshot commit");
        let error = save_nib_config_full(root.path(), &mut stale)
            .expect_err("stale config snapshot must be rejected");

        assert!(
            error.to_string().contains("stale configuration revision"),
            "{error}"
        );
        assert_eq!(stale.revision, 1);
        let persisted = load_nib_config_full(root.path()).expect("authoritative config");
        assert_eq!(persisted.revision, 2);
        assert_eq!(persisted.agent.max_turns, 41);
    }

    #[test]
    fn legacy_toml_without_revision_defaults_to_zero_and_updates_to_one() {
        let root = tempdir().expect("temporary config root");
        let paths = config_paths(root.path());
        fs::create_dir_all(&paths.nib_dir).expect("config directory");
        fs::write(&paths.toml, "[agent]\nmax_turns = 41\n").expect("legacy TOML");
        assert_eq!(
            load_nib_config_full(root.path())
                .expect("legacy config")
                .revision,
            0
        );

        update_nib_config(root.path(), |config| {
            config.agent.max_turns = 42;
            Ok(())
        })
        .expect("update legacy config");

        let persisted = load_nib_config_full(root.path()).expect("updated config");
        assert_eq!(persisted.revision, 1);
        assert_eq!(persisted.agent.max_turns, 42);
    }

    #[test]
    fn config_revision_overflow_preserves_disk_and_snapshot() {
        let root = tempdir().expect("temporary config root");
        let paths = config_paths(root.path());
        let mut snapshot = NibConfig::default();
        save_nib_config_full(root.path(), &mut snapshot).expect("initial config");
        let before = fs::read(&paths.toml).expect("config before failed save");
        snapshot.revision = u64::MAX;
        snapshot.agent.max_turns = 41;

        let error = save_nib_config_full(root.path(), &mut snapshot)
            .expect_err("revision overflow must fail closed");

        assert!(error.to_string().contains("revision overflowed"), "{error}");
        assert_eq!(snapshot.revision, u64::MAX);
        assert_eq!(snapshot.agent.max_turns, 41);
        assert_eq!(
            fs::read(paths.toml).expect("config after failed save"),
            before
        );
    }

    #[test]
    fn editor_transaction_restores_invalid_edits_atomically() {
        let root = tempdir().expect("temporary config root");
        let mut original = NibConfig::default();
        original.agent.max_turns = 42;
        save_nib_config_full(root.path(), &mut original).expect("initial config");

        let error = edit_nib_config(root.path(), |path| {
            fs::write(path, "not = [valid").map_err(|error| error.to_string())?;
            Ok(())
        })
        .expect_err("invalid edit must be restored");

        assert!(error.to_string().contains("previous config was restored"));
        assert_eq!(
            load_nib_config_full(root.path()).unwrap().agent.max_turns,
            42
        );
    }

    #[test]
    fn oversized_sparse_config_is_rejected_before_reading() {
        let root = tempdir().expect("temporary config root");
        let paths = config_paths(root.path());
        fs::create_dir_all(&paths.nib_dir).expect("config directory");
        File::create(&paths.toml)
            .and_then(|file| file.set_len(MAX_CONFIG_FILE_BYTES + 1))
            .expect("create sparse config");

        let error = load_nib_config_full(root.path())
            .expect_err("oversized config must fail before allocation");

        assert!(matches!(error, ConfigError::FileTooLarge { .. }));
        let mut replacement = NibConfig::default();
        assert!(save_nib_config_full(root.path(), &mut replacement).is_err());
        assert_eq!(
            fs::metadata(paths.toml).unwrap().len(),
            MAX_CONFIG_FILE_BYTES + 1
        );
    }

    #[cfg(unix)]
    #[test]
    fn config_read_rejects_regular_file_replacement_after_open() {
        let root = tempdir().expect("temporary config root");
        let paths = config_paths(root.path());
        let mut canonical = NibConfig::default();
        canonical.llm.providers.insert(
            "credential-source".to_string(),
            ProviderEntry {
                model: "model".to_string(),
                api_key: Some("canonical-config-secret".to_string()),
                ..ProviderEntry::default()
            },
        );
        save_nib_config_full(root.path(), &mut canonical).expect("canonical config");
        let canonical_bytes = fs::read(&paths.toml).expect("canonical bytes");
        let displaced = paths.nib_dir.join("config.toml.displaced");
        fs::rename(&paths.toml, &displaced).expect("displace canonical config");
        fs::write(
            &paths.toml,
            toml::to_string_pretty(&NibConfig::default()).expect("forged config"),
        )
        .expect("publish forged config");

        let restore_path = paths.toml.clone();
        let restore_displaced = displaced.clone();
        let _hook = install_config_read_hook(paths.toml.clone(), move |_| {
            fs::remove_file(&restore_path).map_err(|error| error.to_string())?;
            fs::rename(&restore_displaced, &restore_path).map_err(|error| error.to_string())
        });
        let error = load_nib_config_full(root.path())
            .expect_err("opened forged config must fail identity validation");

        assert!(error.to_string().contains("identity changed"), "{error}");
        assert_eq!(fs::read(&paths.toml).unwrap(), canonical_bytes);
        assert_eq!(
            load_nib_config_full(root.path())
                .unwrap()
                .sensitive_values(),
            ["canonical-config-secret"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn config_read_rejects_regular_file_replacement_from_child_process() {
        const CHILD_ROOT: &str = "NIB_CONFIG_READ_REPLACEMENT_CHILD_ROOT";
        const CHILD_READY: &str = "NIB_CONFIG_READ_REPLACEMENT_CHILD_READY";
        const CHILD_RELEASE: &str = "NIB_CONFIG_READ_REPLACEMENT_CHILD_RELEASE";

        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let root = PathBuf::from(root);
            let paths = config_paths(&root);
            let ready = PathBuf::from(
                std::env::var_os(CHILD_READY).expect("child readiness path must be configured"),
            );
            let release = PathBuf::from(
                std::env::var_os(CHILD_RELEASE).expect("child release path must be configured"),
            );
            let _hook = install_config_read_hook(paths.toml.clone(), move |_| {
                fs::write(&ready, b"ready").map_err(|error| error.to_string())?;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !release.exists() {
                    if std::time::Instant::now() >= deadline {
                        return Err("timed out waiting for config replacement".to_string());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Ok(())
            });

            let error = load_nib_config_full(&root)
                .expect_err("child must reject a post-open config replacement");
            assert!(error.to_string().contains("identity changed"), "{error}");
            return;
        }

        let root = tempdir().expect("temporary config root");
        let paths = config_paths(root.path());
        let mut canonical = NibConfig::default();
        canonical.agent.max_turns = 37;
        save_nib_config_full(root.path(), &mut canonical).expect("canonical config");
        let displaced = paths.nib_dir.join("config.toml.child-displaced");
        let ready = root.path().join("config-read-child.ready");
        let release = root.path().join("config-read-child.release");
        let child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "config::tests::config_read_rejects_regular_file_replacement_from_child_process",
                "--nocapture",
            ])
            .env(CHILD_ROOT, root.path())
            .env(CHILD_READY, &ready)
            .env(CHILD_RELEASE, &release)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn config reader child");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ready.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "config reader child did not reach the post-open barrier"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        fs::rename(&paths.toml, &displaced).expect("displace opened config");
        let mut replacement = NibConfig::default();
        replacement.agent.max_turns = 99;
        fs::write(
            &paths.toml,
            toml::to_string_pretty(&replacement).expect("replacement config"),
        )
        .expect("publish replacement config");
        fs::write(&release, b"release").expect("release config reader child");
        let output = child
            .wait_with_output()
            .expect("wait for config reader child");
        assert!(
            output.status.success(),
            "config reader child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        fs::remove_file(&paths.toml).expect("remove replacement config");
        fs::rename(&displaced, &paths.toml).expect("restore canonical config");
        assert_eq!(
            load_nib_config_full(root.path()).unwrap().agent.max_turns,
            37
        );
    }

    #[cfg(unix)]
    #[test]
    fn config_save_fails_closed_when_nib_directory_is_replaced() {
        let root = tempdir().expect("temporary config root");
        let paths = config_paths(root.path());
        let mut original = NibConfig::default();
        original.agent.max_turns = 41;
        save_nib_config_full(root.path(), &mut original).expect("original config");
        let original_bytes = fs::read(&paths.toml).expect("original bytes");
        let displaced = root.path().join(".nib.displaced");
        let nib_dir = paths.nib_dir.clone();
        let displaced_for_update = displaced.clone();

        let error = update_nib_config(root.path(), move |config| {
            fs::rename(&nib_dir, &displaced_for_update).map_err(|error| error.to_string())?;
            fs::create_dir(&nib_dir).map_err(|error| error.to_string())?;
            config.agent.max_turns = 42;
            Ok(())
        })
        .expect_err("detached config directory must reject publication");

        assert!(
            error.to_string().contains("identity changed")
                || error.to_string().contains("state directory changed"),
            "{error}"
        );
        assert!(!paths.toml.exists(), "replacement directory was modified");
        assert_eq!(
            fs::read(displaced.join("config.toml")).unwrap(),
            original_bytes
        );
        fs::remove_dir(&paths.nib_dir).expect("remove replacement directory");
        fs::rename(&displaced, &paths.nib_dir).expect("restore config directory");
        assert_eq!(
            load_nib_config_full(root.path()).unwrap().agent.max_turns,
            41
        );
    }

    #[test]
    fn config_commit_rejects_target_replacement_and_preserves_newer_file() {
        let root = tempdir().expect("temporary config root");
        let mut original = NibConfig::default();
        original.agent.max_turns = 41;
        save_nib_config_full(root.path(), &mut original).expect("original config");
        let displaced = root.path().join("config.displaced.toml");
        let mut replacement = original.clone();
        replacement.agent.max_turns = 99;
        let replacement_bytes = toml::to_string_pretty(&replacement).expect("replacement TOML");

        let error = with_config_lock(root.path(), |paths, directory| {
            let mut loaded = load_nib_config_with_source_unlocked(paths, directory)?;
            loaded.config.agent.max_turns = 42;
            save_nib_config_atomic_with_hook(
                directory,
                &paths.toml,
                &loaded.config,
                loaded.expectation(),
                || {
                    fs::rename(&paths.toml, &displaced).map_err(|error| error.to_string())?;
                    fs::write(&paths.toml, replacement_bytes.as_bytes())
                        .map_err(|error| error.to_string())
                },
            )
        })
        .expect_err("replaced config target must fail conditional commit");

        assert!(error.to_string().contains("identity changed"), "{error}");
        assert_eq!(
            load_nib_config_full(root.path())
                .expect("replacement config")
                .agent
                .max_turns,
            99
        );
        let displaced_config: NibConfig =
            toml::from_str(&fs::read_to_string(displaced).expect("displaced original"))
                .expect("decode displaced original");
        assert_eq!(displaced_config.agent.max_turns, 41);
    }

    #[cfg(unix)]
    #[test]
    fn real_child_config_commit_barrier_and_fsync_crash_recovery() {
        if let Some(root) = std::env::var_os(CONFIG_COMMIT_CHILD_ROOT) {
            run_config_commit_child(Path::new(&root));
            return;
        }

        let replacement_root = tempdir().expect("replacement config root");
        let replacement_paths = config_paths(replacement_root.path());
        let mut original = NibConfig::default();
        original.agent.max_turns = 41;
        save_nib_config_full(replacement_root.path(), &mut original)
            .expect("seed replacement config");
        let displaced = replacement_root.path().join("config.child-displaced.toml");
        let ready = replacement_root.path().join("config-replacement.ready");
        let release = replacement_root.path().join("config-replacement.release");
        let mut child =
            spawn_config_commit_child(replacement_root.path(), "replace", &ready, Some(&release));
        wait_for_config_commit_child(&mut child, &ready);

        let mut replacement = original.clone();
        replacement.revision = replacement
            .revision
            .checked_add(1)
            .expect("replacement revision");
        replacement.agent.max_turns = 99;
        let replacement_bytes = toml::to_string_pretty(&replacement).expect("replacement TOML");
        fs::rename(&replacement_paths.toml, &displaced).expect("displace expected config");
        fs::write(&replacement_paths.toml, replacement_bytes.as_bytes())
            .expect("install replacement config");
        fs::write(&release, b"release").expect("release config child");
        let status = child.wait().expect("wait for replacement child");
        assert!(status.success(), "replacement child failed: {status}");
        assert_eq!(
            fs::read(&replacement_paths.toml).expect("replacement config bytes"),
            replacement_bytes.as_bytes()
        );
        assert_eq!(
            load_nib_config_full(replacement_root.path())
                .expect("load replacement config")
                .agent
                .max_turns,
            99
        );

        let crash_root = tempdir().expect("crash config root");
        let crash_paths = config_paths(crash_root.path());
        let mut crash_config = NibConfig::default();
        crash_config.agent.max_turns = 53;
        save_nib_config_full(crash_root.path(), &mut crash_config).expect("seed crash config");
        let crash_before = fs::read(&crash_paths.toml).expect("config before crash");
        let crash_ready = crash_root.path().join("config-crash.ready");
        let mut crash_child =
            spawn_config_commit_child(crash_root.path(), "kill", &crash_ready, None);
        wait_for_config_commit_child(&mut crash_child, &crash_ready);
        let temporary = config_temporary_paths(&crash_paths.nib_dir);
        assert_eq!(temporary.len(), 1, "expected one fsynced config temp");
        crash_child.kill().expect("kill config writer");
        crash_child.wait().expect("reap config writer");
        assert!(
            temporary[0].exists(),
            "killed writer temp disappeared early"
        );

        let recovered = load_nib_config_full(crash_root.path()).expect("recover config");
        assert_eq!(recovered.agent.max_turns, 53);
        assert_eq!(
            fs::read(&crash_paths.toml).expect("config after recovery"),
            crash_before
        );
        assert!(
            config_temporary_paths(&crash_paths.nib_dir).is_empty(),
            "config recovery left the killed writer temp"
        );
    }

    #[cfg(unix)]
    #[test]
    fn config_lock_rejects_nib_replacement_after_capability_open() {
        let root = tempdir().expect("temporary config root");
        let mut config = NibConfig::default();
        save_nib_config_full(root.path(), &mut config).expect("initial config");
        let displaced = root.path().join(".nib.prelock-displaced");
        let displaced_for_hook = displaced.clone();

        let error = with_config_lock_with_hook(
            root.path(),
            move |paths| {
                fs::rename(&paths.nib_dir, &displaced_for_hook)?;
                fs::create_dir(&paths.nib_dir)?;
                Ok(())
            },
            |_, _| -> Result<(), ConfigError> {
                panic!("config operation entered with a replacement directory")
            },
        )
        .expect_err("replacement directory must not become authoritative");

        assert!(
            error
                .to_string()
                .contains("identity changed before lock acquisition"),
            "{error}"
        );
        let replacement = config_paths(root.path());
        let replacement_lock = replacement.nib_dir.join("config.toml.lock");
        let anchor = crate::daemons::state::daemon_lock_anchor_path(&replacement_lock)
            .expect("replacement lock anchor path");
        assert!(
            anchor.exists(),
            "failed operation must retain its persistent lock anchor"
        );
        crate::daemons::state::with_file_lock_in(&replacement_lock, &replacement.nib_dir, |_| {
            Ok(())
        })
        .expect("reconcile replacement lock domain");
        assert!(
            !anchor.exists(),
            "successful reconciliation must clean the replacement lock anchor"
        );
        fs::remove_dir_all(&replacement.nib_dir).expect("remove replacement directory");
        fs::rename(&displaced, root.path().join(".nib")).expect("restore config directory");
        load_nib_config_full(root.path()).expect("load restored config");
    }

    #[cfg(unix)]
    #[test]
    fn editor_rollback_does_not_write_to_replacement_nib_directory() {
        let root = tempdir().expect("temporary config root");
        let paths = config_paths(root.path());
        let mut original = NibConfig::default();
        original.agent.max_turns = 73;
        save_nib_config_full(root.path(), &mut original).expect("original config");
        let original_bytes = fs::read(&paths.toml).expect("original bytes");
        let displaced = root.path().join(".nib.editor-displaced");
        let nib_dir = paths.nib_dir.clone();
        let displaced_for_edit = displaced.clone();
        let replacement_bytes = b"replacement-owned-by-editor";

        let error = edit_nib_config(root.path(), move |path| {
            fs::rename(&nib_dir, &displaced_for_edit).map_err(|error| error.to_string())?;
            fs::create_dir(&nib_dir).map_err(|error| error.to_string())?;
            fs::write(path, replacement_bytes).map_err(|error| error.to_string())?;
            Err::<(), _>("editor failed after directory replacement".to_string())
        })
        .expect_err("rollback must fail closed after directory replacement");

        assert!(error.to_string().contains("state directory"), "{error}");
        assert_eq!(fs::read(&paths.toml).unwrap(), replacement_bytes);
        assert_eq!(
            fs::read(displaced.join("config.toml")).unwrap(),
            original_bytes
        );
        fs::remove_dir_all(&paths.nib_dir).expect("remove replacement directory");
        fs::rename(&displaced, &paths.nib_dir).expect("restore config directory");
        assert_eq!(
            load_nib_config_full(root.path()).unwrap().agent.max_turns,
            73
        );
    }

    #[cfg(unix)]
    #[test]
    fn config_rejects_symlinked_nib_directory_without_writing_outside() {
        use std::os::unix::fs::symlink;

        let root = tempdir().expect("project");
        let outside = tempdir().expect("outside");
        symlink(outside.path(), root.path().join(".nib")).expect("symlink .nib");

        let mut config = NibConfig::default();
        let error = save_nib_config_full(root.path(), &mut config)
            .expect_err("symlinked state root must fail closed");

        assert!(error.to_string().contains("symlink"));
        assert!(!outside.path().join("config.toml").exists());
        assert!(!outside.path().join("config.toml.lock").exists());
    }

    #[cfg(windows)]
    #[test]
    fn config_rejects_non_symlink_reparse_config_path() {
        let root = tempdir().expect("temporary config root");
        let paths = config_paths(root.path());
        fs::create_dir_all(&paths.nib_dir).expect("config directory");
        let target = root.path().join("config-reparse-target");
        fs::create_dir(&target).expect("reparse target");
        let output = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&paths.toml)
            .arg(&target)
            .output()
            .expect("create config junction");
        assert!(
            output.status.success(),
            "mklink failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let error = load_nib_config_full(root.path())
            .expect_err("non-symlink reparse config path must fail closed");

        assert!(error.to_string().contains("reparse point"), "{error}");
    }

    #[cfg(unix)]
    fn run_config_commit_child(root: &Path) {
        let mode = std::env::var(CONFIG_COMMIT_CHILD_MODE)
            .expect("config commit child mode must be configured");
        let ready = PathBuf::from(
            std::env::var_os(CONFIG_COMMIT_CHILD_READY)
                .expect("config commit child ready path must be configured"),
        );
        let release = std::env::var_os(CONFIG_COMMIT_CHILD_RELEASE).map(PathBuf::from);
        let result = with_config_lock(root, |paths, directory| {
            let mut loaded = load_nib_config_with_source_unlocked(paths, directory)?;
            loaded.config.agent.max_turns = 42;
            loaded.config.revision = loaded.config.revision.checked_add(1).ok_or_else(|| {
                ConfigError::Operation("configuration revision overflowed".to_string())
            })?;
            save_nib_config_atomic_with_hook(
                directory,
                &paths.toml,
                &loaded.config,
                loaded.expectation(),
                || {
                    fs::write(&ready, b"ready").map_err(|error| error.to_string())?;
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                    loop {
                        if release.as_ref().is_some_and(|path| path.exists()) {
                            return Ok(());
                        }
                        if std::time::Instant::now() >= deadline {
                            return Err("config commit child timed out".to_string());
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                },
            )
        });

        match mode.as_str() {
            "replace" => {
                let error = result.expect_err("commit-barrier replacement must fail closed");
                assert!(error.to_string().contains("identity changed"), "{error}");
            }
            "kill" => panic!("config crash child unexpectedly left its commit barrier"),
            value => panic!("unsupported config commit child mode: {value}"),
        }
    }

    #[cfg(unix)]
    fn spawn_config_commit_child(
        root: &Path,
        mode: &str,
        ready: &Path,
        release: Option<&Path>,
    ) -> std::process::Child {
        let _ = fs::remove_file(ready);
        if let Some(release) = release {
            let _ = fs::remove_file(release);
        }
        let mut command = std::process::Command::new(
            std::env::current_exe().expect("current config test binary"),
        );
        command
            .args([
                "--exact",
                "config::tests::real_child_config_commit_barrier_and_fsync_crash_recovery",
                "--nocapture",
            ])
            .env(CONFIG_COMMIT_CHILD_ROOT, root)
            .env(CONFIG_COMMIT_CHILD_MODE, mode)
            .env(CONFIG_COMMIT_CHILD_READY, ready);
        if let Some(release) = release {
            command.env(CONFIG_COMMIT_CHILD_RELEASE, release);
        }
        command.spawn().expect("spawn config commit child")
    }

    #[cfg(unix)]
    fn wait_for_config_commit_child(child: &mut std::process::Child, ready: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ready.exists() {
            if let Some(status) = child.try_wait().expect("inspect config commit child") {
                panic!("config commit child exited before readiness: {status}");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "config commit child did not become ready"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    fn config_temporary_paths(directory: &Path) -> Vec<PathBuf> {
        fs::read_dir(directory)
            .expect("list config directory")
            .map(|entry| entry.expect("config directory entry").path())
            .filter(|path| {
                path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with(".config.toml.tmp-") && name.ends_with(".tmp")
                })
            })
            .collect()
    }
}
