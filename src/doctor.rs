use std::path::Path;

use nib::config::{config_paths, load_config_with_source, load_nib_config, ConfigSource};
use nib::llm::factory::provider_ready;
use nib::sandbox;

pub fn run_doctor(project: &Path) {
    println!("nib doctor");
    println!("==========");

    match load_config_with_source(project) {
        Ok((llm, source)) => {
            let label = match source {
                ConfigSource::Toml => "config.toml",
                ConfigSource::MigratedFromJson => "migrated from config.json",
                ConfigSource::Default => "defaults (no config file)",
            };
            println!("Config: {label}");
            println!("Active provider: {}", llm.get_active_provider());
            for (name, _) in nib::config::SUPPORTED_PROVIDERS {
                let ready = provider_ready(llm.get_provider(Some(name)), name);
                println!(
                    "  Provider {name}: {}",
                    if ready { "ready" } else { "missing key" }
                );
            }
        }
        Err(e) => println!("Config error: {e}"),
    }

    let paths = config_paths(project);
    println!(
        "Config path: {} (exists: {})",
        paths.toml.display(),
        paths.toml.exists()
    );

    let store = nib::session::SessionStore::new(project);
    println!(
        "Sessions: {} in {}",
        store.list().len(),
        store.sessions_dir().display()
    );

    println!("\n{}", sandbox::doctor_report());

    let nib_cfg = load_nib_config(project);
    println!(
        "Execution provider: {} (profile: {})",
        nib_cfg.execution.provider, nib_cfg.execution.default_profile
    );
}
