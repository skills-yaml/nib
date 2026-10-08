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
fn expired_deadline_rejects_a_free_config_mutex() {
    let mutex = Mutex::new(());
    let path = Path::new("expired-free-config.toml");

    let error = acquire_config_mutex(
        &mutex,
        path,
        Some(Instant::now() - Duration::from_millis(1)),
    )
    .expect_err("an expired deadline must reject an uncontended config mutex");

    assert!(
        error
            .to_string()
            .contains("configuration lock deadline elapsed"),
        "{error}"
    );
    assert!(
        mutex.try_lock().is_ok(),
        "failed acquisition retained the mutex"
    );
}

#[test]
fn expired_config_operation_does_not_create_state_or_enter_mutation() {
    let root = tempdir().expect("project");
    let mut operation_ran = false;

    let error = with_config_lock_until(
        root.path(),
        Instant::now() - Duration::from_millis(1),
        |_, _| {
            operation_ran = true;
            Ok(())
        },
    )
    .expect_err("an expired config operation must fail before setup");

    assert!(
        error
            .to_string()
            .contains("configuration lock deadline elapsed"),
        "{error}"
    );
    assert!(!operation_ran, "expired config operation entered mutation");
    assert!(
        !root.path().join(".nib").exists(),
        "expired config operation created its state namespace"
    );
}

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
    assert_eq!(
        bundled,
        [
            "gpt-6.1-sol",
            "gpt-6-astra",
            "gpt-6-luna",
            "gpt-6-sol",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
        ]
    );

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
            "gpt-6.1-sol",
            "gpt-6-astra",
            "gpt-6-luna",
            "gpt-6-sol",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
        ]
    );
}

#[test]
fn selected_model_outside_bundled_catalog_remains_visible() {
    for selected in ["gpt-5.5", "gpt-4.1"] {
        let configured = LlmConfig {
            active_provider: Some("openai".to_string()),
            providers: HashMap::from([(
                "openai".to_string(),
                ProviderEntry {
                    model: selected.to_string(),
                    ..ProviderEntry::default()
                },
            )]),
            ..LlmConfig::default()
        };
        let models = configured.get_available_models(None);
        assert_eq!(models[0], selected);
        assert_eq!(models[1], "gpt-6.1-sol");
        assert_eq!(models.iter().filter(|model| *model == selected).count(), 1);
        assert!(!LlmConfig::default()
            .get_available_models(Some("openai"))
            .iter()
            .any(|model| model == selected));
    }
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
    let full = load_nib_config_or_default(root.path()).expect("missing complete config defaults");
    let (_, source) = load_config_with_source(root.path()).expect("missing config source defaults");

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
    assert!(cfg.agent.answer_only);
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
    assert!(
        !cfg.workspace.allowed,
        "workspace access starts ungranted until startup consent"
    );
}

#[test]
fn workspace_access_grant_persists_in_project_config() {
    let root = tempfile::tempdir().expect("project root");
    assert!(
        !workspace_access_is_granted(root.path()).expect("default grant state"),
        "first load must not imply consent"
    );
    grant_workspace_access(root.path()).expect("grant workspace");
    assert!(workspace_access_is_granted(root.path()).expect("granted state"));
    let reloaded = load_nib_config_full(root.path()).expect("reload");
    assert!(reloaded.workspace.allowed);
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
    assert!(persisted.agent.answer_only);
}

#[test]
fn answer_only_defaults_on_and_can_be_disabled_explicitly() {
    let legacy: NibConfig =
        toml::from_str("[agent]\nmax_turns = 41\ntool_use_enforcement = true\n")
            .expect("legacy agent config");
    assert!(legacy.agent.answer_only);

    let mut configured = legacy;
    configured.agent.answer_only = false;
    let encoded = toml::to_string(&configured).expect("serialize answer-only config");
    let decoded: NibConfig = toml::from_str(&encoded).expect("reload answer-only config");
    assert!(!decoded.agent.answer_only);
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
    save_nib_config_full(replacement_root.path(), &mut original).expect("seed replacement config");
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
    let mut crash_child = spawn_config_commit_child(crash_root.path(), "kill", &crash_ready, None);
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
        None,
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
    crate::daemons::state::with_file_lock_in(&replacement_lock, &replacement.nib_dir, |_| Ok(()))
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
    let mut command =
        std::process::Command::new(std::env::current_exe().expect("current config test binary"));
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
