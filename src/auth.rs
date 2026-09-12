use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use nib::config::{load_config_with_source, update_nib_config, ConfigSource};
use nib::llm::registry::{provider_descriptor, PROVIDERS};

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
        let providers: Vec<_> = PROVIDERS.iter().collect();
        for (i, provider) in providers.iter().enumerate() {
            println!("  {}. {} - {}", i + 1, provider.id, provider.display_name);
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
                providers[num - 1].id.to_string()
            } else {
                println!("Invalid number.");
                continue;
            }
        } else {
            input.to_lowercase()
        };

        let provider_known = provider_descriptor(&provider).is_some();
        if !provider_known && provider != "mock" {
            println!("Unknown provider: {}. Try again.", provider);
            continue;
        }

        print!("Enter API key for {} (input will be hidden): ", provider);
        io::stdout().flush().unwrap();
        let api_key = read_password().trim().to_string();

        let descriptor = provider_descriptor(&provider).expect("provider validated above");
        let default_model = descriptor.default_model();
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
    println!("You can now run: nib");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nib::config::{config_paths, load_nib_config_full, LlmApiMode};
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
        assert_eq!(
            config.llm.providers["openai"].api,
            Some(LlmApiMode::Responses)
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
