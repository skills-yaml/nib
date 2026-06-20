use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderEntry {
    pub model: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LLMConfigFile {
    pub active_provider: Option<String>,
    pub providers: HashMap<String, ProviderEntry>,
}

impl LLMConfigFile {
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

pub fn load_config(project_root: &Path) -> LLMConfigFile {
    let path = project_root.join(".nib").join("config.json");
    if path.exists() {
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(cfg) = serde_json::from_str(&content) {
                return cfg;
            }
        }
    }
    LLMConfigFile::default()
}

pub fn save_config(
    project_root: &Path,
    cfg: &LLMConfigFile,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = project_root.join(".nib").join("config.json");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(cfg)?;
    fs::write(path, content)?;
    Ok(())
}
