use clap::Args;
use nib::context::assemble_profile_context;

#[derive(Args, Debug)]
pub struct ContextArgs {
    #[arg(default_value = ".")]
    pub path: String,

    #[arg(short, long)]
    pub task: Option<String>,
}

pub fn run_context(args: &ContextArgs) -> Result<(), String> {
    let path = std::path::PathBuf::from(&args.path);
    let ctx = assemble_profile_context(&path, args.task.as_deref())?;
    println!("{ctx}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn context_command_assembles_profile_context_with_and_without_task() {
        let project = tempdir().expect("project");
        std::fs::write(project.path().join("AGENTS.md"), "runtime context rule")
            .expect("AGENTS fixture");
        let mut config = nib::config::NibConfig::default();
        nib::config::save_nib_config_full(project.path(), &mut config).expect("config");

        for task in [Some("inspect context".to_string()), None] {
            run_context(&ContextArgs {
                path: project.path().to_string_lossy().into_owned(),
                task,
            })
            .expect("context command");
        }
    }

    #[test]
    fn context_command_reports_invalid_config() {
        let project = tempdir().expect("project");
        let paths = nib::config::config_paths(project.path());
        std::fs::create_dir_all(&paths.nib_dir).expect("config directory");
        std::fs::write(&paths.toml, "not = [valid").expect("invalid config");
        assert!(run_context(&ContextArgs {
            path: project.path().to_string_lossy().into_owned(),
            task: Some("inspect".to_string()),
        })
        .is_err());
    }
}
