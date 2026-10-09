//! T043 split.

use super::*;

pub(crate) type ConfigMutex = Mutex<()>;

pub(crate) static CONFIG_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<ConfigMutex>>>> =
    OnceLock::new();
pub(crate) const CONFIG_OPERATION_ERROR_SENTINEL: &str = "__nib_config_operation_error__";
pub(crate) const CONFIG_ATOMIC_TEMPORARY_PREFIX: &str = ".config.toml.tmp-";
pub(crate) const MAX_CONFIG_DIRECTORY_ENTRIES: usize = 10_000;
pub(crate) const MAX_CONFIG_DIRECTORY_NAME_BYTES: usize = MAX_CONFIG_DIRECTORY_ENTRIES * 256;

#[cfg(test)]
pub(crate) type ConfigReadHook = Box<dyn FnOnce(&Path) -> Result<(), String> + Send>;

#[cfg(test)]
pub(crate) struct PendingConfigReadHook {
    pub(crate) path: PathBuf,
    pub(crate) hook: Option<ConfigReadHook>,
}

#[cfg(test)]
pub(crate) static CONFIG_READ_HOOK: OnceLock<Mutex<Option<PendingConfigReadHook>>> =
    OnceLock::new();

#[cfg(test)]
pub(crate) struct ConfigReadHookGuard {
    pub(crate) path: PathBuf,
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
pub(crate) fn run_config_read_hook(path: &Path) -> Result<(), ConfigError> {
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
pub(crate) const MAX_CONFIG_FILE_BYTES: u64 = 1024 * 1024;
pub(crate) const MAX_CONTEXT_LENGTH: usize = 4_000_000;
pub(crate) const MAX_AGENT_TURNS: u32 = 10_000;
pub(crate) const MAX_TERMINAL_TIMEOUT_SECS: u64 = 3_600;
pub(crate) const MAX_PROVIDERS: usize = 64;
pub(crate) const MAX_PROVIDER_MODELS: usize = 128;
pub(crate) const MAX_PROFILES: usize = 64;
pub(crate) const MAX_IDENTIFIER_BYTES: usize = 128;
pub(crate) const MAX_MODEL_BYTES: usize = 512;
pub(crate) const MAX_URL_BYTES: usize = 4 * 1024;
pub(crate) const MAX_SECRET_BYTES: usize = 64 * 1024;
pub(crate) const MAX_PROVIDER_KEYS: usize = 64;
pub(crate) const MAX_PATH_BYTES: usize = 4 * 1024;
pub(crate) const MAX_BOUNDARY_PATHS: usize = 256;
pub(crate) const MAX_BOUNDARY_PROFILES: usize = 64;
pub(crate) const MAX_SKILL_PATHS: usize = 256;
pub(crate) const MAX_ACTIVE_SKILLS: usize = 256;
pub(crate) const MAX_MCP_COMMAND_BYTES: usize = 4 * 1024;
pub(crate) const MAX_MCP_ARGUMENTS: usize = 256;
pub(crate) const MAX_MCP_ARGUMENT_BYTES: usize = 16 * 1024;
pub(crate) const MAX_MCP_ARGUMENT_BYTES_TOTAL: usize = 256 * 1024;
pub(crate) const MAX_MCP_ENV_VARS: usize = 256;
pub(crate) const MAX_ENV_NAME_BYTES: usize = 128;
pub(crate) const MAX_ENV_VALUE_BYTES: usize = 64 * 1024;
pub(crate) const MAX_MCP_ENV_BYTES_TOTAL: usize = 512 * 1024;

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
    #[serde(default)]
    pub workspace: WorkspaceConfig,
}

pub(crate) fn revision_is_zero(revision: &u64) -> bool {
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

pub(crate) fn default_execution_provider() -> String {
    "hybrid".to_string()
}

pub(crate) fn default_profile() -> String {
    "restricted".to_string()
}

pub(crate) fn default_true() -> bool {
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

pub(crate) fn default_network() -> String {
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

pub(crate) fn default_threshold() -> f64 {
    0.50
}
pub(crate) fn default_target_ratio() -> f64 {
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

pub(crate) fn default_memory_provider() -> String {
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

pub(crate) fn default_retention_days() -> i64 {
    30
}

pub(crate) fn default_daemon_interval_seconds() -> u64 {
    24 * 60 * 60
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    #[serde(default = "default_true")]
    pub tool_use_enforcement: bool,
    /// Permit a bounded, non-executable response before normal planning for a new
    /// interactive request. Projects may explicitly disable this route.
    #[serde(default = "default_true")]
    pub answer_only: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: default_max_turns(),
            tool_use_enforcement: true,
            answer_only: true,
        }
    }
}

pub(crate) fn default_max_turns() -> u32 {
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

pub(crate) fn default_terminal_backend() -> String {
    "local".to_string()
}

pub(crate) fn default_terminal_timeout() -> u64 {
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

pub(crate) fn default_approval_mode() -> String {
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

pub(crate) fn default_workload_store() -> String {
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

pub(crate) fn default_skill_paths() -> Vec<PathBuf> {
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

pub(crate) fn default_profile_id() -> String {
    "default".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Interactive grant to work in this project directory.
    #[serde(default)]
    pub allowed: bool,
}

pub fn workspace_access_is_granted(project_root: &Path) -> Result<bool, ConfigError> {
    Ok(load_nib_config_full(project_root)?.workspace.allowed)
}

pub fn grant_workspace_access(project_root: &Path) -> Result<(), ConfigError> {
    update_nib_config(project_root, |config| {
        config.workspace.allowed = true;
        Ok(())
    })
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

pub(crate) fn default_mcp_request_timeout_secs() -> u64 {
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

pub(crate) fn default_context_length() -> usize {
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
    #[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
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
            "manual" | "smart" | "policy" | "off" | "ask" | "accept-edits" | "plan" | "auto"
        ) {
            issues.push(
                "approvals.mode must be ask, accept-edits, plan, auto, policy (or manual, smart, off)"
                    .to_string(),
            );
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

pub(crate) fn validate_boundary_config(
    field: &str,
    boundaries: &BoundaryConfig,
    issues: &mut Vec<String>,
) {
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

pub(crate) fn is_scoped_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && !path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

pub(crate) fn is_safe_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_BYTES
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

pub(crate) fn configured_endpoint_conflicts(base_url: &str, mode: LlmApiMode) -> bool {
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

pub(crate) fn configured_endpoint_issue(base_url: &str, mode: LlmApiMode) -> Option<&'static str> {
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

pub(crate) fn configured_terminal_api_mode(path: &str) -> Option<LlmApiMode> {
    let path = path.trim_end_matches('/');
    [LlmApiMode::ChatCompletions, LlmApiMode::Responses]
        .into_iter()
        .find(|mode| path.ends_with(mode.endpoint_suffix()))
}

pub(crate) fn configured_terminal_api_modes(mut path: &str) -> Vec<LlmApiMode> {
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

pub(crate) fn configured_percent_decode_repeated(value: &str) -> Option<String> {
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

pub(crate) fn configured_percent_decode_once(value: &str) -> String {
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

pub(crate) fn configured_hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn path_bytes(path: &Path) -> usize {
    path.as_os_str().as_encoded_bytes().len()
}

pub(crate) fn path_contains_nul(path: &Path) -> bool {
    path.as_os_str().as_encoded_bytes().contains(&0)
}

pub(crate) fn is_valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    name.len() <= MAX_ENV_NAME_BYTES
        && (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

pub(crate) fn is_sensitive_environment_name(name: &str) -> bool {
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

pub(crate) fn load_nib_config_full_until(
    project_root: &Path,
    deadline: Instant,
) -> Result<NibConfig, ConfigError> {
    with_config_lock_until(project_root, deadline, |paths, directory| {
        load_nib_config_with_source_unlocked(paths, directory).map(|loaded| loaded.config)
    })
}

/// Reads a configuration snapshot without creating, migrating, backing up, or
/// recovering configuration state. Cancellation reconciliation uses this path
/// because its single absolute deadline must cover every setup step and a
/// running subagent already has a durable `.nib` namespace.
pub(crate) fn load_nib_config_full_read_only_until(
    project_root: &Path,
    deadline: Instant,
) -> Result<NibConfig, ConfigError> {
    ensure_config_lock_deadline(Some(deadline))?;
    let paths = config_paths(project_root);
    let directory =
        crate::daemons::state::StableDirectory::open(&paths.nib_dir).map_err(config_state_error)?;
    ensure_config_lock_deadline(Some(deadline))?;
    let config = if let Some(file) = directory
        .open_atomic_file_read_only_until(&paths.toml, CONFIG_ATOMIC_TEMPORARY_PREFIX, deadline)
        .map_err(config_state_error)?
    {
        load_nib_config_opened_file(&directory, &paths.toml, file)?
    } else if regular_file_exists(&directory, &paths.json)? {
        let (content, _) = read_regular_file(&directory, &paths.json)?;
        let llm: LlmConfig = serde_json::from_str(&content)?;
        NibConfig {
            revision: 1,
            llm,
            ..NibConfig::default()
        }
    } else {
        NibConfig::default()
    };
    ensure_config_lock_deadline(Some(deadline))?;
    config.validate()?;
    Ok(config)
}

/// Resolves configuration for a mutation-free namespace preflight. Unlike the
/// cancellation-only reader above, a genuinely absent `.nib` directory means
/// validated defaults; it is never created by this operation.
pub(crate) fn load_nib_config_full_preflight_read_only_until(
    project_root: &Path,
    deadline: Instant,
) -> Result<NibConfig, ConfigError> {
    ensure_config_lock_deadline(Some(deadline))?;
    let paths = config_paths(project_root);
    match std::fs::symlink_metadata(&paths.nib_dir) {
        Ok(_) => load_nib_config_full_read_only_until(project_root, deadline),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let config = NibConfig::default();
            config.validate()?;
            ensure_config_lock_deadline(Some(deadline))?;
            match std::fs::symlink_metadata(&paths.nib_dir) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(config),
                Ok(_) => Err(ConfigError::Operation(
                    "configuration namespace appeared during read-only preflight".to_string(),
                )),
                Err(error) => Err(ConfigError::Io(error)),
            }
        }
        Err(error) => Err(ConfigError::Io(error)),
    }
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

/// Bootstraps configuration in a newly created, caller-exclusive worktree.
///
/// This deliberately does not create the repository-wide persistent config
/// lock namespace: the worktree is not yet published to any other nib
/// operation, and the caller compensates its exact directory on failure.
#[cfg(test)]
pub(crate) fn save_nib_config_full_new_unpublished_root(
    project_root: &Path,
    cfg: &mut NibConfig,
) -> Result<(), ConfigError> {
    cfg.validate()?;
    if cfg.revision != 0 {
        return Err(ConfigError::Operation(
            "unpublished worktree configuration must start at revision zero".to_string(),
        ));
    }
    let paths = config_paths(project_root);
    let canonical = crate::fs_security::ensure_directory_without_symlinks(&paths.nib_dir)?;
    let directory =
        crate::daemons::state::StableDirectory::open(&canonical).map_err(config_state_error)?;
    if regular_file_exists(&directory, &paths.toml)?
        || regular_file_exists(&directory, &paths.json)?
    {
        return Err(ConfigError::Operation(
            "unpublished worktree configuration destination is not empty".to_string(),
        ));
    }
    let mut next = cfg.clone();
    next.revision = 1;
    save_nib_config_atomic(
        &directory,
        &paths.toml,
        &next,
        crate::daemons::state::FileExpectation::Missing,
    )?;
    cfg.revision = 1;
    Ok(())
}

/// Bootstraps configuration in a caller-exclusive worktree while an external
/// workload authority remains attached. The same absolute deadline and guard
/// cover directory creation and the atomic config publication.
pub(crate) fn save_nib_config_full_new_unpublished_root_with_guard(
    project_root: &Path,
    cfg: &mut NibConfig,
    deadline: Instant,
    mut external_guard: impl FnMut() -> Result<(), String>,
) -> Result<(), ConfigError> {
    let mut guard = || {
        ensure_config_lock_deadline(Some(deadline)).map_err(|error| error.to_string())?;
        external_guard()?;
        ensure_config_lock_deadline(Some(deadline)).map_err(|error| error.to_string())
    };
    guard().map_err(ConfigError::Operation)?;
    cfg.validate()?;
    if cfg.revision != 0 {
        return Err(ConfigError::Operation(
            "unpublished worktree configuration must start at revision zero".to_string(),
        ));
    }
    let paths = config_paths(project_root);
    let root =
        crate::daemons::state::StableDirectory::open(project_root).map_err(config_state_error)?;
    let directory = root
        .open_or_create_descendant_directory_with_guard(&paths.nib_dir, &mut guard, |_| Ok(()))
        .map_err(config_state_error)?;
    guard().map_err(ConfigError::Operation)?;
    if regular_file_exists(&directory, &paths.toml)?
        || regular_file_exists(&directory, &paths.json)?
    {
        return Err(ConfigError::Operation(
            "unpublished worktree configuration destination is not empty".to_string(),
        ));
    }
    let mut next = cfg.clone();
    next.revision = 1;
    next.validate()?;
    let content = toml::to_string_pretty(&next)?;
    if content.len() as u64 > MAX_CONFIG_FILE_BYTES {
        return Err(ConfigError::FileTooLarge {
            path: paths.toml.display().to_string(),
            size: content.len() as u64,
            max: MAX_CONFIG_FILE_BYTES,
        });
    }
    directory
        .save_bytes_atomically_expected_with_receipt_and_guard(
            &paths.toml,
            content.as_bytes(),
            CONFIG_ATOMIC_TEMPORARY_PREFIX,
            crate::daemons::state::FileExpectation::Missing,
            &mut guard,
        )
        .map_err(|error| config_state_error(error.message))?;
    guard().map_err(ConfigError::Operation)?;
    cfg.revision = 1;
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

pub(crate) fn with_config_lock<T>(
    project_root: &Path,
    operation: impl FnOnce(
        &ConfigPaths,
        &crate::daemons::state::StableDirectory,
    ) -> Result<T, ConfigError>,
) -> Result<T, ConfigError> {
    with_config_lock_with_hook(project_root, None, |_| Ok(()), operation)
}

pub(crate) fn with_config_lock_until<T>(
    project_root: &Path,
    deadline: Instant,
    operation: impl FnOnce(
        &ConfigPaths,
        &crate::daemons::state::StableDirectory,
    ) -> Result<T, ConfigError>,
) -> Result<T, ConfigError> {
    with_config_lock_with_hook(project_root, Some(deadline), |_| Ok(()), operation)
}
