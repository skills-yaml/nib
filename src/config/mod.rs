//! nib configuration: the user configuration in nib's user home, a
//! project's preferences in `<project>/.nib/config.toml`, and the user's
//! settings for each project (see `layers`).

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use thiserror::Error;

pub mod keymap;
mod layers;
mod split_00;
mod split_01;

pub(crate) use layers::*;
pub(crate) use split_00::*;
pub(crate) use split_01::*;

pub use layers::{
    load_project_settings, load_user_config_in, project_settings_path, update_project_settings,
    update_user_config, update_user_config_in, user_config_path, ProjectApprovalSettings,
    ProjectExecutionSettings, ProjectLlmSettings, ProjectMigrationRecord, ProjectSettings,
    ProjectWorkspaceSettings, ProviderSettings,
};

pub use split_00::{
    config_paths, load_config_or_default, load_nib_config_or_default, ConfigError, ConfigSource,
    ConfigValidationError,
};
pub use split_00::{
    edit_nib_config, save_nib_config_full, update_nib_config, update_nib_config_conditionally,
    ConfigMutation,
};
pub use split_00::{
    grant_workspace_access, workspace_access_is_granted, McpConfig, McpServerEntry, WorkspaceConfig,
};
pub use split_00::{
    is_openai_compatible_provider, supports_extended_prompt_cache, ConfigPaths, LlmApiMode,
    LlmConfig, PromptCachePolicy, ProviderEntry, ReasoningEffort,
};
pub use split_00::{
    load_config_with_source, load_nib_config_full, load_nib_config_full_with_source, save_config,
};
pub use split_00::{
    AgentConfig, ApprovalsConfig, EscalationConfig, ProfileConfig, ProfilesConfig, SkillConfig,
    SkillsConfig, TerminalConfig, WorkloadConfig,
};
pub use split_00::{
    BoundaryConfig, CompressionConfig, DaemonsConfig, ExecutionConfig, MemoryConfig, NibConfig,
};

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
