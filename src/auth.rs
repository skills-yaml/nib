use std::io::{self, Write};
use std::path::PathBuf;

use crate::config::{load_config, save_config, LLMConfigFile, SUPPORTED_PROVIDERS};

pub fn run_auth_wizard() {
    println!("nib Auth Wizard");
    println!("================");
    println!("Select a provider and enter its API key.");
    println!("You can configure multiple providers.");
    println!();

    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut cfg: LLMConfigFile = load_config(&project_root);

    loop {
        println!("Available providers:");
        let providers: Vec<_> = SUPPORTED_PROVIDERS.iter().collect();
        for (i, (name, desc)) in providers.iter().enumerate() {
            println!("  {}. {} - {}", i + 1, name, desc);
        }

        print!("Enter provider name or number (or 'done' to finish): ");
        io::stdout().flush().unwrap();

        let mut input = String::new();
        io::stdin().read_line(&mut input).unwrap();
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
        let api_key = rpassword::read_password()
            .unwrap_or_default()
            .trim()
            .to_string();

        let default_models: std::collections::HashMap<&str, &str> = [
            ("openai", "gpt-4o"),
            ("anthropic", "claude-3-5-sonnet-20241022"),
            ("google", "gemini-1.5-pro"),
            ("grok", "grok-2"),
            ("openrouter", "openrouter/anthropic/claude-3.5-sonnet"),
        ]
        .iter()
        .cloned()
        .collect();

        let default_model = default_models.get(provider.as_str()).unwrap_or(&"default");
        print!("Default model [{}]: ", default_model);
        io::stdout().flush().unwrap();
        let mut model_input = String::new();
        io::stdin().read_line(&mut model_input).unwrap();
        let model = if model_input.trim().is_empty() {
            default_model.to_string()
        } else {
            model_input.trim().to_string()
        };

        cfg.add_or_update_provider(
            provider.clone(),
            model.clone(),
            if api_key.is_empty() {
                None
            } else {
                Some(api_key)
            },
        );

        println!("Added/updated provider: {}", provider);

        if std::env::var("NIB_AUTH_ONE").is_err() {
            print!("Add another provider? [y/N]: ");
            io::stdout().flush().unwrap();
            let mut again = String::new();
            io::stdin().read_line(&mut again).unwrap();
            if !again.trim().to_lowercase().starts_with('y') {
                break;
            }
        } else {
            break;
        }
    }

    if let Err(e) = save_config(&project_root, &cfg) {
        eprintln!("Failed to save config: {}", e);
    } else {
        println!("\nProviders configured in .nib/config.json");
        println!("You can now run: nib chat");
    }
}
