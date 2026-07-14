use clap::{Args, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Args)]
pub struct SkillArgs {
    #[command(subcommand)]
    pub command: SkillCommands,
}

#[derive(Subcommand)]
pub enum SkillCommands {
    /// List installed skills
    List,
    /// Install a skill from a local path or Git URL
    Install {
        /// Local path or Git repository URL
        source: String,
    },
    /// Remove an installed skill
    Remove {
        /// Name of the skill to remove
        name: String,
    },
}

pub fn run_skill_cmd(args: &SkillArgs, project_root: &Path) {
    match &args.command {
        SkillCommands::List => list_skills(project_root),
        SkillCommands::Install { source } => install_skill(source),
        SkillCommands::Remove { name } => remove_skill(name),
    }
}

fn get_global_skills_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config").join("nib").join("skills"))
}

fn list_skills(project_root: &Path) {
    println!("Installed Skills:");
    let paths = nib::context::skills::find_skills(project_root);
    if paths.is_empty() {
        println!("  No skills found.");
        return;
    }

    for p in paths {
        if let Some(skill) = nib::context::skills::parse_skill(&p) {
            let is_global = p.to_string_lossy().contains(".config/nib/skills");
            let location = if is_global { "global" } else { "local " };
            println!(
                "  [{}] {} - {}",
                location, skill.frontmatter.name, skill.frontmatter.description
            );
        }
    }
}

fn install_skill(source: &str) {
    let global_dir = match get_global_skills_dir() {
        Some(d) => d,
        None => {
            eprintln!("Could not determine global skills directory.");
            std::process::exit(1);
        }
    };

    if !global_dir.exists() {
        if let Err(e) = fs::create_dir_all(&global_dir) {
            eprintln!("Failed to create global skills directory: {}", e);
            std::process::exit(1);
        }
    }

    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let src_path = if source.starts_with("http") || source.starts_with("git@") {
        println!("Cloning repository...");
        let status = Command::new("git")
            .arg("clone")
            .arg(source)
            .arg(temp_dir.path())
            .status();
        if status.is_err() || !status.unwrap().success() {
            eprintln!("Failed to clone repository.");
            std::process::exit(1);
        }
        temp_dir.path().to_path_buf()
    } else {
        PathBuf::from(source)
    };

    let skill_md = src_path.join("SKILL.md");
    if !skill_md.exists() {
        eprintln!("Error: SKILL.md not found in {}", source);
        std::process::exit(1);
    }

    let skill = match nib::context::skills::parse_skill(&skill_md) {
        Some(s) => s,
        None => {
            eprintln!("Error: Invalid SKILL.md format.");
            std::process::exit(1);
        }
    };

    // Sanitize name for directory
    let safe_name = skill
        .frontmatter
        .name
        .to_lowercase()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "-");

    let target_dir = global_dir.join(&safe_name);
    if target_dir.exists() {
        eprintln!(
            "Skill '{}' is already installed at {:?}",
            skill.frontmatter.name, target_dir
        );
        std::process::exit(1);
    }

    // copy directory
    if let Err(e) = copy_dir_all(&src_path, &target_dir) {
        eprintln!("Failed to copy skill files: {}", e);
        std::process::exit(1);
    }

    println!("Successfully installed skill '{}'.", skill.frontmatter.name);
}

fn remove_skill(name: &str) {
    let global_dir = match get_global_skills_dir() {
        Some(d) => d,
        None => {
            eprintln!("Could not determine global skills directory.");
            std::process::exit(1);
        }
    };

    let safe_name = name
        .to_lowercase()
        .replace(|c: char| !c.is_ascii_alphanumeric(), "-");
    let target_dir = global_dir.join(&safe_name);

    if target_dir.exists() {
        if let Err(e) = fs::remove_dir_all(&target_dir) {
            eprintln!("Failed to remove skill: {}", e);
            std::process::exit(1);
        }
        println!("Successfully removed skill '{}'.", name);
    } else {
        eprintln!("Skill '{}' not found in global skills directory.", name);
        std::process::exit(1);
    }
}

fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> std::io::Result<()> {
    fs::create_dir_all(&dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            // Ignore .git directory
            if entry.file_name() == ".git" {
                continue;
            }
            copy_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
        }
    }
    Ok(())
}
