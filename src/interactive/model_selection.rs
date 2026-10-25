//! Provider and model identities shared by plain mode and the TUI.
use super::{ModelChoice, ModelSelection};
use crate::config::NibConfig;

pub(super) fn selection(config: &NibConfig) -> ModelSelection {
    let provider = config.llm.get_active_provider();
    let current = config
        .llm
        .get_provider(None)
        .map(|entry| entry.model.clone())
        .unwrap_or_default();
    let mut providers: Vec<_> = config.llm.providers.keys().cloned().collect();
    if providers.is_empty() {
        providers.push(provider.clone());
    }
    providers.sort();
    let available = providers
        .into_iter()
        .flat_map(|name| {
            config
                .llm
                .get_available_models(Some(&name))
                .into_iter()
                .map(move |model| ModelChoice {
                    provider: name.clone(),
                    model,
                })
        })
        .collect();
    ModelSelection {
        provider,
        current,
        available,
        sensitive_values: config.public_session_sensitive_values(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{load_nib_config_full, save_nib_config_full};
    use crate::interactive::{set_active_model, set_provider_model};

    #[test]
    fn all_providers_are_selectable_and_selection_preserves_provider_settings() {
        let root = tempfile::tempdir().unwrap();
        let mut config = NibConfig::default();
        config
            .llm
            .add_or_update_provider("openai".into(), "shared".into(), None);
        config
            .llm
            .add_or_update_provider("openrouter".into(), "vendor/custom".into(), None);
        config.llm.active_provider = Some("openai".into());
        config.llm.providers.get_mut("openai").unwrap().models = Some(vec!["shared".into()]);
        config.llm.providers.get_mut("openrouter").unwrap().models = Some(vec!["shared".into()]);
        config.llm.providers.get_mut("openrouter").unwrap().api_key = Some("fixture-key".into());
        config.llm.providers.get_mut("openrouter").unwrap().base_url =
            Some("https://gateway.example/v1".into());
        save_nib_config_full(root.path(), &mut config).unwrap();
        let store = crate::session::SessionStore::at_dir(root.path().join("sessions"));
        let session = store.try_create_session().unwrap();
        let crate::interactive::InteractiveEffect::SelectModel(choices) =
            crate::interactive::execute_interactive_command(
                crate::interactive::InteractiveCommand::Model { selection: None },
                root.path(),
                &store,
                &session.id,
            )
            .unwrap()
        else {
            panic!("model choices");
        };
        assert_eq!(choices.current, "shared");
        assert_eq!(
            choices
                .available
                .iter()
                .map(ModelChoice::label)
                .collect::<Vec<_>>(),
            vec![
                "shared (openai)",
                "vendor/custom (openrouter)",
                "shared (openrouter)"
            ]
        );
        let original = config.llm.providers.clone();
        set_provider_model(
            root.path(),
            &choices.available[2].provider,
            &choices.available[2].model,
        )
        .unwrap();
        let loaded = load_nib_config_full(root.path()).unwrap();
        assert_eq!(loaded.llm.get_active_provider(), "openrouter");
        assert_eq!(loaded.llm.providers["openrouter"].model, "shared");
        assert_eq!(loaded.llm.providers["openai"], original["openai"]);
        let mut expected = original["openrouter"].clone();
        expected.model = "shared".into();
        assert_eq!(loaded.llm.providers["openrouter"], expected);
        let diagnostics = crate::llm::factory::provider_diagnostics(&loaded.llm, None).unwrap();
        assert_eq!(diagnostics.provider, "openrouter");
        assert_eq!(diagnostics.model, "shared");
        set_active_model(root.path(), "vendor/future").unwrap();
        assert_eq!(
            load_nib_config_full(root.path()).unwrap().llm.providers["openrouter"].model,
            "vendor/future"
        );
        assert!(set_provider_model(root.path(), "missing", "model").is_err());
        assert!(set_provider_model(root.path(), "openai", "").is_err());
        assert_eq!(
            load_nib_config_full(root.path())
                .unwrap()
                .llm
                .get_active_provider(),
            "openrouter"
        );
    }

    #[test]
    fn bundled_models_and_empty_overrides_are_provider_specific() {
        let mut config = NibConfig::default();
        config
            .llm
            .add_or_update_provider("openai".into(), "custom".into(), None);
        config
            .llm
            .add_or_update_provider("openrouter".into(), "vendor/custom".into(), None);
        config.llm.providers.get_mut("openrouter").unwrap().models = Some(vec![]);
        let choices = selection(&config);
        assert!(choices
            .available
            .iter()
            .any(|choice| choice.provider == "openai" && choice.model == "custom"));
        assert!(choices.available.len() > 2);
        assert_eq!(
            choices
                .available
                .iter()
                .filter(|choice| choice.provider == "openrouter")
                .count(),
            1
        );
    }

    #[test]
    fn opaque_exact_ids_and_implicit_mock_remain_usable() {
        let root = tempfile::tempdir().unwrap();
        let mut config = NibConfig::default();
        let fallback = selection(&config).available.remove(0);
        set_provider_model(root.path(), &fallback.provider, &fallback.model).unwrap();
        assert_eq!(
            load_nib_config_full(root.path())
                .unwrap()
                .llm
                .get_active_provider(),
            "mock"
        );
        config
            .llm
            .add_or_update_provider("openai".into(), "gateway::model".into(), None);
        config.llm.active_provider = None;
        save_nib_config_full(root.path(), &mut config).unwrap();
        let choices = selection(&config);
        assert!(choices
            .available
            .iter()
            .all(|choice| choice.provider == "openai"));
        let custom = choices
            .available
            .iter()
            .find(|choice| choice.model == "gateway::model")
            .unwrap();
        set_provider_model(root.path(), &custom.provider, &custom.model).unwrap();
        set_active_model(root.path(), "openrouter::opaque-model").unwrap();
        let loaded = load_nib_config_full(root.path()).unwrap();
        assert_eq!(loaded.llm.get_active_provider(), "openai");
        assert_eq!(
            loaded.llm.providers["openai"].model,
            "openrouter::opaque-model"
        );
    }
}
