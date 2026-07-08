//! Integration tests for config migration (T009).

use nib::config::{
    config_paths, load_config_with_source, save_config, ConfigSource, LlmConfig, ProviderEntry,
};
use std::collections::HashMap;
use std::fs;
use tempfile::tempdir;

#[test]
fn config_migration_integration() {
    let dir = tempdir().expect("tempdir");
    let paths = config_paths(dir.path());
    fs::create_dir_all(&paths.nib_dir).expect("mkdir");

    fs::write(
        &paths.json,
        r#"{
  "active_provider": "anthropic",
  "providers": {
    "anthropic": {
      "model": "claude-3-5-sonnet-20241022",
      "api_key": "secret-key",
      "base_url": null
    }
  }
}"#,
    )
    .expect("write json");

    let (llm, source) = load_config_with_source(dir.path()).expect("load");
    assert_eq!(source, ConfigSource::MigratedFromJson);
    assert_eq!(llm.get_active_provider(), "anthropic");
    assert_eq!(
        llm.providers
            .get("anthropic")
            .and_then(|p| p.api_key.as_deref()),
        Some("secret-key")
    );
    assert!(paths.toml.exists());
    assert!(paths.json_backup.exists());
    assert!(!paths.json.exists());
}

#[test]
fn config_toml_save_load_integration() {
    let dir = tempdir().expect("tempdir");
    let llm = LlmConfig {
        active_provider: Some("openrouter".to_string()),
        providers: HashMap::from([(
            "openrouter".to_string(),
            ProviderEntry {
                model: "openrouter/anthropic/claude-3.5-sonnet".to_string(),
                api_key: Some("or-key".to_string()),
                base_url: Some("https://openrouter.ai/api/v1".to_string()),
            },
        )]),
    };

    save_config(dir.path(), &llm).expect("save");
    let (loaded, source) = load_config_with_source(dir.path()).expect("load");
    assert_eq!(source, ConfigSource::Toml);
    assert_eq!(loaded, llm);
}
