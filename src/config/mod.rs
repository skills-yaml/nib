//! Project-local configuration stored in `.nib/config.toml`.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use thiserror::Error;

mod split_00;
mod split_01;

pub(crate) use split_00::*;
pub(crate) use split_01::*;

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
    is_openai_compatible_provider, ConfigPaths, LlmApiMode, LlmConfig, ProviderEntry,
    ReasoningEffort,
};
pub use split_00::{
    load_config_with_source, load_nib_config_full, load_nib_config_full_with_source, save_config,
};
pub use split_00::{
    AgentConfig, ApprovalsConfig, ProfileConfig, ProfilesConfig, SkillConfig, SkillsConfig,
    TerminalConfig, WorkloadConfig,
};
pub use split_00::{
    BoundaryConfig, CompressionConfig, DaemonsConfig, ExecutionConfig, LLMConfigFile, MemoryConfig,
    NibConfig,
};

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
