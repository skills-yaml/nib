use std::path::{Path, PathBuf};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct SkillFrontmatter {
    pub name: String,
    pub description: String,
}

pub struct Skill {
    pub path: PathBuf,
    pub frontmatter: SkillFrontmatter,
    pub body: String,
}

pub fn find_skills(project_path: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![];
    if let Ok(h) = std::env::var("HOME") {
        let global = PathBuf::from(h).join(".config").join("nib").join("skills");
        if global.exists() {
            dirs.push(global);
        }
    }
    let local = project_path.join(".nib").join("skills");
    if local.exists() {
        dirs.push(local);
    }

    let mut skill_files = vec![];
    for dir in dirs {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    let skill_md = p.join("SKILL.md");
                    if skill_md.exists() {
                        skill_files.push(skill_md);
                    }
                }
            }
        }
    }
    skill_files
}

pub fn parse_skill(path: &Path) -> Option<Skill> {
    let content = std::fs::read_to_string(path).ok()?;
    if !content.starts_with("---") {
        return None;
    }
    
    let parts: Vec<&str> = content.splitn(3, "---").collect();
    if parts.len() < 3 {
        return None;
    }

    let yaml_str = parts[1];
    let body = parts[2].trim().to_string();

    // simple manual yaml parse to avoid heavy deps if possible, or use serde_yaml
    // Wait, let's parse basic name/desc manually if no yaml crate.
    // Or we can add serde_yaml.
    let mut name = String::new();
    let mut desc = String::new();
    for line in yaml_str.lines() {
        if let Some(rest) = line.strip_prefix("name:") {
            name = rest.trim().trim_matches('"').trim_matches('\'').to_string();
        }
        if let Some(rest) = line.strip_prefix("description:") {
            desc = rest.trim().trim_matches('"').trim_matches('\'').to_string();
        }
    }

    if name.is_empty() {
        return None;
    }

    Some(Skill {
        path: path.to_path_buf(),
        frontmatter: SkillFrontmatter {
            name,
            description: desc,
        },
        body,
    })
}

pub fn load_relevant_skills(project_path: &Path, task: Option<&str>) -> String {
    let task_lower = task.unwrap_or("").to_lowercase();
    if task_lower.is_empty() {
        return String::new();
    }

    let paths = find_skills(project_path);
    let mut injected = Vec::new();

    for p in paths {
        if let Some(skill) = parse_skill(&p) {
            let name_lower = skill.frontmatter.name.to_lowercase();
            let desc_lower = skill.frontmatter.description.to_lowercase();
            
            // Simple heuristic matching
            let is_relevant = task_lower.contains(&name_lower) 
                || desc_lower.split_whitespace().any(|word| word.len() > 3 && task_lower.contains(word));
            
            if is_relevant {
                injected.push(format!("### Skill: {}\n{}\n\n{}", skill.frontmatter.name, skill.frontmatter.description, skill.body));
            }
        }
    }

    if injected.is_empty() {
        String::new()
    } else {
        format!("## Active Skills\n\n{}", injected.join("\n---\n"))
    }
}
