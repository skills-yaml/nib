use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use nib::config::{load_config_with_source, update_nib_config, ConfigSource, SUPPORTED_PROVIDERS};

pub fn run_auth_wizard() -> Result<(), String> {
    let stdin = io::stdin();
    run_auth_wizard_with_input(&mut stdin.lock(), || {
        rpassword::read_password().unwrap_or_default()
    })
}

fn run_auth_wizard_with_input(
    reader: &mut impl BufRead,
    mut read_password: impl FnMut() -> String,
) -> Result<(), String> {
    println!("nib Auth Wizard");
    println!("================");
    println!("Select a provider and enter its API key.");
    println!("You can configure multiple providers.");
    println!();

    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match load_config_with_source(&project_root) {
        Ok((_, ConfigSource::MigratedFromJson)) => {
            println!(
                "[nib] Migrated .nib/config.json → .nib/config.toml (backup: config.json.bak)"
            );
        }
        Ok(_) => {}
        Err(error) => return Err(format!("could not load configuration: {error}")),
    }
    let mut updates = Vec::new();

    loop {
        println!("Available providers:");
        let providers: Vec<_> = SUPPORTED_PROVIDERS.iter().collect();
        for (i, (name, desc)) in providers.iter().enumerate() {
            println!("  {}. {} - {}", i + 1, name, desc);
        }

        print!("Enter provider name or number (or 'done' to finish): ");
        io::stdout().flush().unwrap();

        let mut input = String::new();
        reader
            .read_line(&mut input)
            .map_err(|error| format!("failed to read provider selection: {error}"))?;
        let input = input.trim();

        if input.eq_ignore_ascii_case("done") || input.is_empty() {
            break;
        }

        let provider = if let Ok(num) = input.parse::<usize>() {
            if num > 0 && num <= providers.len() {
                providers[num - 1].0.to_string()
            } else {
                println!("Invalid number.");
                continue;
            }
        } else {
            input.to_lowercase()
        };

        let provider_known = SUPPORTED_PROVIDERS.iter().any(|(p, _)| *p == provider);
        if !provider_known && provider != "mock" {
            println!("Unknown provider: {}. Try again.", provider);
            continue;
        }

        print!("Enter API key for {} (input will be hidden): ", provider);
        io::stdout().flush().unwrap();
        let api_key = read_password().trim().to_string();

        let default_models: std::collections::HashMap<&str, &str> = [
            ("openai", "gpt-4o"),
            ("anthropic", "claude-3-5-sonnet-20241022"),
            ("google", "gemini-1.5-pro"),
            ("grok", "grok-2-1212"),
            ("openrouter", "openrouter/anthropic/claude-3.5-sonnet"),
            ("meta", "muse-spark-1.1"),
            ("mock", "mock-model"),
        ]
        .iter()
        .cloned()
        .collect();

        let default_model = default_models.get(provider.as_str()).unwrap_or(&"default");
        print!("Default model [{}]: ", default_model);
        io::stdout().flush().unwrap();
        let mut model_input = String::new();
        reader
            .read_line(&mut model_input)
            .map_err(|error| format!("failed to read default model: {error}"))?;
        let model = if model_input.trim().is_empty() {
            default_model.to_string()
        } else {
            model_input.trim().to_string()
        };

        updates.push((
            provider.clone(),
            model.clone(),
            if api_key.is_empty() {
                None
            } else {
                Some(api_key)
            },
        ));

        println!("Added/updated provider: {}", provider);

        if std::env::var("NIB_AUTH_ONE").is_err() {
            print!("Add another provider? [y/N]: ");
            io::stdout().flush().unwrap();
            let mut again = String::new();
            reader
                .read_line(&mut again)
                .map_err(|error| format!("failed to read provider continuation: {error}"))?;
            if !again.trim().to_lowercase().starts_with('y') {
                break;
            }
        } else {
            break;
        }
    }

    update_nib_config(&project_root, move |config| {
        for (provider, model, api_key) in updates {
            config.llm.add_or_update_provider(provider, model, api_key);
        }
        Ok(())
    })
    .map_err(|error| format!("failed to save configuration: {error}"))?;
    println!("\nProviders configured in .nib/config.toml");
    println!("You can now run: nib chat");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nib::config::{config_paths, load_nib_config_full};
    use serial_test::serial;
    use std::ffi::OsString;
    use std::io::Cursor;
    use tempfile::tempdir;

    struct CurrentDirGuard(PathBuf);

    impl CurrentDirGuard {
        fn enter(path: &std::path::Path) -> Self {
            let original = std::env::current_dir().expect("current directory");
            std::env::set_current_dir(path).expect("enter project");
            Self(original)
        }
    }

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.0).expect("restore current directory");
        }
    }

    fn restore_env(name: &str, value: Option<OsString>) {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }

    #[test]
    #[serial]
    fn auth_wizard_reprompts_and_saves_mock_provider() {
        let project = tempdir().expect("project");
        let _cwd = CurrentDirGuard::enter(project.path());
        let mut input = Cursor::new(b"999\nunknown\nmock\n\nn\n");

        run_auth_wizard_with_input(&mut input, String::new).expect("auth wizard");

        let config = load_nib_config_full(project.path()).expect("saved config");
        assert_eq!(config.llm.get_active_provider(), "mock");
        assert_eq!(config.llm.providers["mock"].model, "mock-model");
        assert_eq!(config.llm.providers["mock"].api_key, None);
    }

    #[test]
    #[serial]
    fn auth_wizard_accepts_numeric_provider_and_one_shot_mode() {
        let project = tempdir().expect("project");
        let _cwd = CurrentDirGuard::enter(project.path());
        let previous_one = std::env::var_os("NIB_AUTH_ONE");
        std::env::set_var("NIB_AUTH_ONE", "1");
        let mut input = Cursor::new(b"1\ncustom-model\n");

        run_auth_wizard_with_input(&mut input, || "secret-key".to_string())
            .expect("one-shot auth wizard");

        let config = load_nib_config_full(project.path()).expect("saved config");
        assert_eq!(config.llm.get_active_provider(), "openai");
        assert_eq!(config.llm.providers["openai"].model, "custom-model");
        assert_eq!(
            config.llm.providers["openai"].api_key.as_deref(),
            Some("secret-key")
        );
        restore_env("NIB_AUTH_ONE", previous_one);
    }

    #[test]
    #[serial]
    fn auth_wizard_preserves_corrupt_configuration() {
        let project = tempdir().expect("project");
        let paths = config_paths(project.path());
        std::fs::create_dir_all(&paths.nib_dir).expect("config directory");
        std::fs::write(&paths.toml, "invalid = [toml").expect("corrupt config");
        let _cwd = CurrentDirGuard::enter(project.path());
        let mut input = Cursor::new(b"done\n");

        let error = run_auth_wizard_with_input(&mut input, String::new)
            .expect_err("corrupt config must fail closed");
        assert!(error.contains("could not load configuration"));
        assert_eq!(
            std::fs::read_to_string(&paths.toml).expect("unchanged config"),
            "invalid = [toml"
        );
    }
}
