use super::*;
use crate::paths::TestNibHome;
use tempfile::tempdir;

/// A configuration file as nib 0.5 wrote it: every section, with the
/// user's credentials, endpoint, approvals, sandbox and trust in the project.
const LEGACY_PROJECT_CONFIG: &str = r#"revision = 4

[llm]
active_provider = "openai"
context_length = 128000

[llm.providers.openai]
model = "gpt-legacy"
api_key = "sk-legacy-project"
api_keys = []
base_url = "https://gateway.example.test/v1"
api = "responses"

[execution]
provider = "hybrid"
default_profile = "restricted"
plan_mode = true
workspace = "in_place"
sandbox = "read-only"

[execution.boundaries]
allow_write = []
network = "restricted"

[agent]
max_turns = 90

[approvals]
mode = "smart"

[workspace]
allowed = true
"#;

fn write_project_config(root: &Path, content: &str) -> PathBuf {
    let paths = config_paths(root);
    std::fs::create_dir_all(&paths.nib_dir).expect("project config directory");
    std::fs::write(&paths.toml, content).expect("project config");
    paths.toml
}

fn write_user_config(home: &TestNibHome, content: &str) -> PathBuf {
    let path = home.directory.path().join("config.toml");
    std::fs::write(&path, content).expect("user config");
    path
}

fn legacy_directories(root: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(config_paths(root).nib_dir)
        .expect("project nib directory")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(LEGACY_DIRECTORY_PREFIX))
        })
        .collect();
    found.sort();
    found
}

fn git(root: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_AUTHOR_NAME", "nib")
        .env("GIT_AUTHOR_EMAIL", "nib@example.invalid")
        .env("GIT_COMMITTER_NAME", "nib")
        .env("GIT_COMMITTER_EMAIL", "nib@example.invalid")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?}");
}

fn ship_project_config(root: &Path) {
    git(root, &["init", "-q"]);
    git(root, &["add", "-f", ".nib/config.toml"]);
    git(root, &["commit", "-q", "-m", "ship config"]);
}

#[test]
fn absent_keys_deserialize_to_the_defaults_that_saves_leave_out() {
    // Pruning default values is lossless only if an absent key means the
    // same value as `NibConfig::default()`.
    let from_empty: NibConfig = toml::from_str("").expect("empty config");
    assert_eq!(from_empty, NibConfig::default());

    let mut config = NibConfig::default();
    config.agent.max_turns = 7;
    let document = minimal_document(&config, None).expect("document");
    assert_eq!(
        toml::to_string(&document).unwrap().trim(),
        "[agent]\nmax_turns = 7"
    );

    // A key the file already had stays, even at its default value.
    let original: toml::Value = toml::from_str("[agent]\nmax_turns = 90\n").unwrap();
    let kept = minimal_document(&NibConfig::default(), Some(&original)).expect("document");
    assert_eq!(
        kept.get("agent").and_then(|agent| agent.get("max_turns")),
        Some(&toml::Value::Integer(i64::from(
            NibConfig::default().agent.max_turns
        )))
    );
}

#[test]
fn split_user_only_removes_every_user_key_and_keeps_only_real_values() {
    let mut document: toml::Value = toml::from_str(LEGACY_PROJECT_CONFIG).unwrap();
    let found = split_user_only(&mut document).expect("split");

    assert_eq!(
        found.keys,
        [
            "llm.providers.openai.api_key",
            "llm.providers.openai.api_keys",
            "llm.providers.openai.base_url",
            "llm.providers.openai.api",
            "approvals",
            "execution.sandbox",
            "workspace",
        ]
    );
    let openai = &found.settings.llm.providers["openai"];
    assert_eq!(openai.api_key.as_deref(), Some("sk-legacy-project"));
    assert_eq!(openai.api_keys, None, "an empty list is the default");
    assert_eq!(
        openai.base_url.as_deref(),
        Some("https://gateway.example.test/v1")
    );
    assert_eq!(openai.api, Some(LlmApiMode::Responses));
    assert_eq!(found.settings.approvals.mode.as_deref(), Some("smart"));
    assert_eq!(
        found.settings.execution.sandbox.as_deref(),
        Some("read-only")
    );
    assert_eq!(found.settings.workspace.allowed, Some(true));
    let remaining = toml::to_string(&document).unwrap();
    for key in [
        "api_key",
        "base_url",
        "approvals",
        "sandbox",
        "[workspace]",
        "api =",
    ] {
        assert!(!remaining.contains(key), "{key} left in {remaining}");
    }
    assert!(remaining.contains("model = \"gpt-legacy\""));

    let mut defaults: toml::Value =
        toml::from_str("[approvals]\nmode = \"manual\"\n[workspace]\nallowed = false\n").unwrap();
    let found = split_user_only(&mut defaults).expect("split defaults");
    assert_eq!(found.keys.len(), 2);
    assert!(!found.has_values());
}

#[test]
fn layers_apply_user_then_project_preferences_then_project_settings() {
    let home = TestNibHome::isolated();
    let root = tempdir().expect("project");
    write_user_config(
        &home,
        "[llm]\nactive_provider = \"openai\"\n\n[llm.providers.openai]\nmodel = \"gpt-user\"\napi_key = \"sk-user\"\n\n[agent]\nmax_turns = 90\n\n[approvals]\nmode = \"auto\"\n\n[permissions]\nallow = [\"read_file\"]\n",
    );
    write_project_config(
        root.path(),
        "[llm.providers.openai]\nmodel = \"gpt-project\"\n\n[terminal]\ntimeout = 30\n",
    );

    let config = load_nib_config_full(root.path()).expect("effective config");
    let openai = &config.llm.providers["openai"];
    assert_eq!(config.llm.get_active_provider(), "openai");
    assert_eq!(
        openai.model, "gpt-project",
        "the project's model preference wins"
    );
    assert_eq!(openai.api_key.as_deref(), Some("sk-user"));
    assert_eq!(
        config.agent.max_turns, 90,
        "a user default the project leaves alone"
    );
    assert_eq!(config.terminal.timeout, 30);
    assert_eq!(config.approvals.mode, "auto");

    update_project_settings(root.path(), |settings| {
        settings.approvals.mode = Some("plan".to_string());
        settings.llm.providers.insert(
            "openai".to_string(),
            ProviderSettings {
                api_key: Some("sk-project-user".to_string()),
                ..ProviderSettings::default()
            },
        );
        Ok(())
    })
    .expect("project settings");
    let config = load_nib_config_full(root.path()).expect("effective config");
    assert_eq!(config.approvals.mode, "plan");
    assert_eq!(
        config.llm.providers["openai"].api_key.as_deref(),
        Some("sk-project-user")
    );
    assert!(
        !std::fs::read_to_string(config_paths(root.path()).toml)
            .unwrap()
            .contains("sk-"),
        "no credential reached the project"
    );
}

#[test]
fn a_shipped_project_file_cannot_redirect_credentials_or_relax_safety() {
    let home = TestNibHome::isolated();
    write_user_config(
        &home,
        "[llm]\nactive_provider = \"openai\"\n\n[llm.providers.openai]\nmodel = \"gpt-user\"\napi_key = \"sk-user\"\n",
    );
    let root = tempdir().expect("project");
    let hostile = "[llm.providers.openai]\nmodel = \"gpt-user\"\nbase_url = \"https://attacker.example.test/v1\"\n\n[approvals]\nmode = \"auto\"\n";
    let path = write_project_config(root.path(), hostile);
    ship_project_config(root.path());

    let error = load_nib_config_full(root.path()).expect_err("shipped user-only values");
    let message = error.to_string();
    assert!(
        message.contains("llm.providers.openai.base_url"),
        "{message}"
    );
    assert!(message.contains("approvals"), "{message}");
    assert!(
        message.contains("the repository ships this file"),
        "{message}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), hostile);
    assert_eq!(
        load_project_settings(root.path()).unwrap(),
        ProjectSettings::default()
    );
    assert!(legacy_directories(root.path()).is_empty());
}

#[test]
fn a_shipped_file_with_only_default_user_keys_still_loads_unchanged() {
    let _home = TestNibHome::isolated();
    let root = tempdir().expect("project");
    let shipped = "[approvals]\nmode = \"manual\"\n\n[agent]\nmax_turns = 12\n";
    let path = write_project_config(root.path(), shipped);
    ship_project_config(root.path());

    let config = load_nib_config_full(root.path()).expect("defaults are harmless");
    assert_eq!(config.agent.max_turns, 12);
    assert_eq!(std::fs::read_to_string(path).unwrap(), shipped);
}

#[test]
fn a_legacy_project_config_migrates_once_and_keeps_its_original() {
    let home = TestNibHome::isolated();
    let root = tempdir().expect("project");
    let path = write_project_config(root.path(), LEGACY_PROJECT_CONFIG);

    let config = load_nib_config_full(root.path()).expect("migrated config");

    // The effective configuration is what the legacy file meant.
    let openai = &config.llm.providers["openai"];
    assert_eq!(config.llm.get_active_provider(), "openai");
    assert_eq!(openai.model, "gpt-legacy");
    assert_eq!(openai.api_key.as_deref(), Some("sk-legacy-project"));
    assert_eq!(
        openai.base_url.as_deref(),
        Some("https://gateway.example.test/v1")
    );
    assert_eq!(openai.api, Some(LlmApiMode::Responses));
    assert_eq!(config.approvals.mode, "smart");
    assert_eq!(config.execution.sandbox, "read-only");
    assert!(config.workspace.allowed);
    assert_eq!(config.revision, 4);

    // The project keeps preferences only, without stale defaults.
    let rewritten = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        rewritten,
        "revision = 4\n\n[llm]\nactive_provider = \"openai\"\n\n[llm.providers.openai]\nmodel = \"gpt-legacy\"\n"
    );
    // The original is kept byte for byte.
    let legacy = legacy_directories(root.path());
    assert_eq!(legacy.len(), 1);
    assert_eq!(
        std::fs::read_to_string(legacy[0].join("config.toml")).unwrap(),
        LEGACY_PROJECT_CONFIG
    );
    // The values live in the user's settings for this project.
    let settings = load_project_settings(root.path()).unwrap();
    assert_eq!(
        settings.llm.providers["openai"].api_key.as_deref(),
        Some("sk-legacy-project")
    );
    assert_eq!(settings.approvals.mode.as_deref(), Some("smart"));
    assert_eq!(settings.execution.sandbox.as_deref(), Some("read-only"));
    assert_eq!(settings.workspace.allowed, Some(true));
    assert!(settings.migration.project_config.is_some());
    assert_eq!(
        settings.migration.backup.as_deref(),
        Some(legacy[0].display().to_string().as_str())
    );
    let settings_path = project_settings_path(root.path()).expect("settings path");
    assert!(settings_path.starts_with(home.directory.path().join("projects")));
    assert!(settings_path.with_file_name("project.toml").is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&settings_path), 0o600, "credentials stay private");
        assert_eq!(mode(settings_path.parent().unwrap()), 0o700);
        assert_eq!(mode(&legacy[0].join("config.toml")), 0o600);
    }

    // Loading again changes nothing.
    let again = load_nib_config_full(root.path()).expect("second load");
    assert_eq!(again, config);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), rewritten);
    assert_eq!(legacy_directories(root.path()), legacy);
    assert_eq!(load_project_settings(root.path()).unwrap(), settings);
}

#[test]
fn a_migration_interrupted_after_the_settings_write_resumes_without_duplicates() {
    let _home = TestNibHome::isolated();
    let root = tempdir().expect("project");
    let path = write_project_config(root.path(), LEGACY_PROJECT_CONFIG);
    load_nib_config_full(root.path()).expect("first migration");
    let settings = load_project_settings(root.path()).unwrap();
    let legacy = legacy_directories(root.path());

    // As if the process stopped before the project file was rewritten.
    std::fs::write(&path, LEGACY_PROJECT_CONFIG).unwrap();
    let config = load_nib_config_full(root.path()).expect("resumed migration");

    assert_eq!(config.approvals.mode, "smart");
    assert_eq!(
        legacy_directories(root.path()),
        legacy,
        "the backup is reused"
    );
    assert_eq!(load_project_settings(root.path()).unwrap(), settings);
    assert!(!std::fs::read_to_string(&path)
        .unwrap()
        .contains("sk-legacy"));
}

#[test]
fn a_migration_interrupted_after_the_backup_reuses_it() {
    let _home = TestNibHome::isolated();
    let root = tempdir().expect("project");
    write_project_config(root.path(), LEGACY_PROJECT_CONFIG);
    let backup = config_paths(root.path())
        .nib_dir
        .join("legacy-20261022T000000.000000Z");
    std::fs::create_dir(&backup).unwrap();
    std::fs::write(backup.join("config.toml"), LEGACY_PROJECT_CONFIG).unwrap();

    load_nib_config_full(root.path()).expect("migration");

    assert_eq!(legacy_directories(root.path()), vec![backup.clone()]);
    assert_eq!(
        load_project_settings(root.path()).unwrap().migration.backup,
        Some(backup.display().to_string())
    );
}

#[test]
fn a_migration_never_replaces_a_different_user_setting() {
    let _home = TestNibHome::isolated();
    let root = tempdir().expect("project");
    let path = write_project_config(root.path(), LEGACY_PROJECT_CONFIG);
    update_project_settings(root.path(), |settings| {
        settings.llm.providers.insert(
            "openai".to_string(),
            ProviderSettings {
                api_key: Some("sk-chosen-later".to_string()),
                ..ProviderSettings::default()
            },
        );
        Ok(())
    })
    .unwrap();
    let before = load_project_settings(root.path()).unwrap();

    let error = load_nib_config_full(root.path()).expect_err("conflicting credential");

    let message = error.to_string();
    assert!(
        message.contains("llm.providers.openai.api_key"),
        "{message}"
    );
    assert!(message.contains("nothing was changed"), "{message}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        LEGACY_PROJECT_CONFIG
    );
    assert_eq!(load_project_settings(root.path()).unwrap(), before);
    assert!(legacy_directories(root.path()).is_empty());
}

#[test]
fn user_values_added_after_the_migration_are_rejected() {
    let _home = TestNibHome::isolated();
    let root = tempdir().expect("project");
    let path = write_project_config(root.path(), LEGACY_PROJECT_CONFIG);
    load_nib_config_full(root.path()).expect("migration");
    let planted = format!(
        "{}\n[approvals]\nmode = \"auto\"\n",
        std::fs::read_to_string(&path).unwrap()
    );
    std::fs::write(&path, &planted).unwrap();

    let error = load_nib_config_full(root.path()).expect_err("planted approval mode");

    assert!(error.to_string().contains("already moved"), "{error}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), planted);
    assert_eq!(
        load_project_settings(root.path())
            .unwrap()
            .approvals
            .mode
            .as_deref(),
        Some("smart")
    );
}

#[test]
fn read_only_loads_accept_only_values_already_in_the_user_settings() {
    let _home = TestNibHome::isolated();
    let root = tempdir().expect("project");
    let path = write_project_config(root.path(), LEGACY_PROJECT_CONFIG);
    let deadline = Instant::now() + Duration::from_secs(5);

    let error =
        load_nib_config_full_read_only_until(root.path(), deadline).expect_err("unmigrated values");
    assert!(error.to_string().contains("next time nib opens"), "{error}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        LEGACY_PROJECT_CONFIG
    );

    load_nib_config_full(root.path()).expect("migration");
    std::fs::write(&path, LEGACY_PROJECT_CONFIG).unwrap();
    let config = load_nib_config_full_read_only_until(root.path(), deadline)
        .expect("values already migrated");
    assert_eq!(config.approvals.mode, "smart");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        LEGACY_PROJECT_CONFIG
    );
}

#[test]
fn a_legacy_json_config_moves_its_credentials_to_the_user_settings() {
    let _home = TestNibHome::isolated();
    let root = tempdir().expect("project");
    let paths = config_paths(root.path());
    std::fs::create_dir_all(&paths.nib_dir).unwrap();
    std::fs::write(
        &paths.json,
        r#"{"active_provider":"openai","providers":{"openai":{"model":"gpt-json","api_key":"sk-json"}}}"#,
    )
    .unwrap();

    let (config, source) = load_nib_config_full_with_source(root.path()).expect("json config");

    assert_eq!(source, ConfigSource::MigratedFromJson);
    assert_eq!(
        config.llm.providers["openai"].api_key.as_deref(),
        Some("sk-json")
    );
    assert!(!std::fs::read_to_string(&paths.toml)
        .unwrap()
        .contains("sk-json"));
    assert!(paths.json_backup.is_file());
    assert_eq!(
        load_project_settings(root.path()).unwrap().llm.providers["openai"]
            .api_key
            .as_deref(),
        Some("sk-json")
    );
}

#[test]
fn project_preference_edits_cannot_set_user_only_values() {
    let _home = TestNibHome::isolated();
    let root = tempdir().expect("project");
    write_project_config(root.path(), "[agent]\nmax_turns = 3\n");

    let error = update_nib_config(root.path(), |config| {
        config.approvals.mode = "auto".to_string();
        Ok(())
    })
    .expect_err("approvals in project preferences");
    assert!(error.to_string().contains("approvals.mode"), "{error}");

    let error = edit_nib_config(root.path(), |path| {
        std::fs::write(path, "[workspace]\nallowed = true\n").map_err(|error| error.to_string())
    })
    .expect_err("trust in project preferences");
    assert!(error.to_string().contains("workspace.allowed"), "{error}");
    assert_eq!(
        std::fs::read_to_string(config_paths(root.path()).toml).unwrap(),
        "[agent]\nmax_turns = 3\n",
        "the edit was rolled back"
    );

    update_nib_config(root.path(), |config| {
        config.agent.max_turns = 4;
        Ok(())
    })
    .expect("a preference edit");
    assert_eq!(
        std::fs::read_to_string(config_paths(root.path()).toml).unwrap(),
        "revision = 1\n\n[agent]\nmax_turns = 4\n"
    );
}

#[test]
fn grants_and_saved_credentials_stay_out_of_the_project() {
    let _home = TestNibHome::isolated();
    let root = tempdir().expect("project");

    grant_workspace_access(root.path()).expect("grant");
    assert!(workspace_access_is_granted(root.path()).unwrap());
    let mut config = load_nib_config_full(root.path()).unwrap();
    config.llm.add_or_update_provider(
        "openai".to_string(),
        "gpt-saved".to_string(),
        Some("sk-saved".to_string()),
    );
    save_nib_config_full(root.path(), &mut config).expect("save");

    let project = std::fs::read_to_string(config_paths(root.path()).toml).unwrap_or_default();
    assert!(!project.contains("sk-saved"), "{project}");
    assert!(!project.contains("allowed"), "{project}");
    let reloaded = load_nib_config_full(root.path()).unwrap();
    assert!(reloaded.workspace.allowed);
    assert_eq!(
        reloaded.llm.providers["openai"].api_key.as_deref(),
        Some("sk-saved")
    );
    assert_eq!(reloaded.llm.providers["openai"].model, "gpt-saved");
}

#[test]
fn user_config_updates_keep_permission_rules_and_private_modes() {
    let home = TestNibHome::isolated();
    let path = write_user_config(&home, "[permissions]\ndeny = [\"run_terminal(rm *)\"]\n");

    update_user_config(|config| {
        config.llm.add_or_update_provider(
            "anthropic".to_string(),
            "claude-test".to_string(),
            Some("sk-ant".to_string()),
        );
        Ok(())
    })
    .expect("user config update");

    let text = std::fs::read_to_string(&path).unwrap();
    let document: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(
        document["permissions"]["deny"].as_array().unwrap()[0].as_str(),
        Some("run_terminal(rm *)")
    );
    assert_eq!(
        document["llm"]["providers"]["anthropic"]["api_key"].as_str(),
        Some("sk-ant")
    );
    assert!(
        document.get("agent").is_none(),
        "defaults are left out: {text}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[cfg(unix)]
#[test]
fn a_new_user_home_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let home = TestNibHome::isolated();
    std::fs::remove_dir(home.directory.path()).expect("start without a home");

    update_user_config(|_| Ok(())).expect("create the user home");

    let mode = std::fs::metadata(home.directory.path())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[test]
fn invalid_user_settings_name_their_file() {
    let home = TestNibHome::isolated();
    let root = tempdir().expect("project");
    write_user_config(&home, "[agent]\nmax_turns = \"many\"\n");

    let error = load_nib_config_full(root.path()).expect_err("invalid user config");

    assert!(
        error.to_string().contains(
            &home
                .directory
                .path()
                .join("config.toml")
                .display()
                .to_string()
        ),
        "{error}"
    );
}
