//! Context assembly for prompts.

use std::path::Path;

use serde_json::{json, Value};

use crate::session::Session;

pub mod agents;
pub mod budget;
pub mod compression;
pub mod project_docs;
pub mod skills;

pub use agents::{find_agents_md, format_context_for_prompt, load_agents_md};

#[derive(Debug, Clone, PartialEq)]
pub struct BoundedSessionContext {
    pub summary: Option<String>,
    pub messages: Vec<Value>,
    pub approximate_tokens: usize,
    pub raw_message_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeContextSection {
    pub label: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeContextSections {
    pub agents: String,
    pub task: String,
    pub project_docs: Vec<RuntimeContextSection>,
    pub skills: Vec<RuntimeContextSection>,
    pub memory: Vec<RuntimeContextSection>,
    pub workload: Vec<RuntimeContextSection>,
    pub attachments: Vec<RuntimeContextSection>,
}

impl RuntimeContextSections {
    pub fn render(&self) -> String {
        let mut context = format!(
            "## Project Agent Guidelines\n{}\n\n## Current Task\n{}",
            self.agents, self.task
        );
        if !self.project_docs.is_empty() {
            context.push_str("\n\n## Project Standards and Library Documentation\n");
            for section in &self.project_docs {
                context.push_str(&format!("\n### {}\n{}\n", section.label, section.content));
            }
        }
        if !self.skills.is_empty() {
            context.push_str("\n\n## Active Skills\n");
            for section in &self.skills {
                context.push_str(&format!("\n### {}\n{}\n", section.label, section.content));
            }
        }
        if !self.memory.is_empty() {
            context.push_str("\n\n## Profile Memory\n");
            for section in &self.memory {
                context.push_str(&format!("\n- {}: {}", section.label, section.content));
            }
        }
        if !self.workload.is_empty() {
            context.push_str("\n\n## Workload Snapshot\n");
            for section in &self.workload {
                context.push_str(&format!("\n- {}: {}", section.label, section.content));
            }
        }
        if !self.attachments.is_empty() {
            context.push_str("\n\n## Attached Project Paths\n");
            for section in &self.attachments {
                context.push_str(&format!("\n### {}\n{}\n", section.label, section.content));
            }
        }
        context
    }
}

const MAX_ATTACHMENT_FILE_BYTES: usize = 8 * 1024;

pub fn attachment_context_sections(
    project_root: &Path,
    attachments: &[crate::session::PathAttachment],
) -> Vec<RuntimeContextSection> {
    let Ok(root) = project_root.canonicalize() else {
        return Vec::new();
    };
    let mut sections = Vec::new();
    for attachment in attachments {
        let candidate = root.join(&attachment.path);
        let Ok(metadata) = candidate.symlink_metadata() else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }
        let Ok(canonical) = candidate.canonicalize() else {
            continue;
        };
        if !canonical.starts_with(&root) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&canonical) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        let truncated = if text.len() > MAX_ATTACHMENT_FILE_BYTES {
            let mut end = MAX_ATTACHMENT_FILE_BYTES.min(text.len());
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}{}", &text[..end], "\n\n...[attached file bounded]...")
        } else {
            text.into_owned()
        };
        sections.push(RuntimeContextSection {
            label: attachment.path.clone(),
            content: truncated,
        });
    }
    sections
}

pub fn bounded_session_context(session: &Session, max_tokens: usize) -> BoundedSessionContext {
    let max_tokens = max_tokens.max(1);
    let summary_budget = if session.summary.is_some() {
        (max_tokens / 3).max(1)
    } else {
        0
    };
    let summary = session
        .summary
        .as_deref()
        .map(|value| compression::truncate_to_tokens(value, summary_budget))
        .filter(|value| !value.is_empty());
    let summary_tokens = summary
        .as_deref()
        .map(compression::approximate_tokens)
        .unwrap_or(0);
    let message_budget = max_tokens.saturating_sub(summary_tokens);

    let start = session.summary_index.min(session.messages.len());
    let mut remaining = message_budget;
    let mut selected = Vec::new();
    for message in session.messages[start..].iter().rev() {
        if remaining == 0 {
            break;
        }
        let normalized = if message.role == "tool" {
            format!("Tool observation: {}", message.content)
        } else {
            message.content.clone()
        };
        let bounded = compression::truncate_to_tokens(&normalized, remaining);
        if bounded.is_empty() {
            break;
        }
        let used = compression::approximate_tokens(&bounded).min(remaining);
        remaining -= used;
        let role = if message.role == "tool" {
            "user"
        } else {
            message.role.as_str()
        };
        selected.push((role.to_string(), bounded));
    }
    selected.reverse();

    let mut messages: Vec<Value> = Vec::new();
    for (role, content) in selected {
        if let Some(last) = messages.last_mut() {
            if last.get("role").and_then(Value::as_str) == Some(role.as_str()) {
                let previous = last
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                *last = json!({"role": role, "content": format!("{previous}\n\n{content}")});
                continue;
            }
        }
        messages.push(json!({"role": role, "content": content}));
    }

    let mut message_tokens = messages
        .iter()
        .filter_map(|message| message.get("content").and_then(Value::as_str))
        .map(compression::approximate_tokens)
        .sum::<usize>();
    while message_tokens > message_budget && !messages.is_empty() {
        let overflow = message_tokens - message_budget;
        let first_content = messages[0]
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let first_tokens = compression::approximate_tokens(first_content);
        if first_tokens <= overflow {
            messages.remove(0);
            message_tokens = message_tokens.saturating_sub(first_tokens);
        } else {
            let bounded = compression::truncate_to_tokens(first_content, first_tokens - overflow);
            messages[0]["content"] = json!(bounded);
            message_tokens = messages
                .iter()
                .filter_map(|message| message.get("content").and_then(Value::as_str))
                .map(compression::approximate_tokens)
                .sum();
        }
    }

    BoundedSessionContext {
        summary,
        messages,
        approximate_tokens: summary_tokens + message_tokens,
        raw_message_count: session.messages.len(),
    }
}

pub fn assemble_context(project_path: &Path, task: Option<&str>) -> String {
    let mut ctx = format_context_for_prompt(project_path, task);

    let project_docs = project_docs::load_project_docs(project_path);
    if !project_docs.is_empty() {
        ctx.push_str("\n\n## Project Standards and Library Documentation\n");
        for section in project_docs {
            ctx.push_str(&format!("\n### {}\n{}\n", section.label, section.content));
        }
    }

    // Inject Memory Store
    let memory_store = crate::session::memory::MemoryStore::new(project_path);
    let mem = memory_store.load();
    if !mem.environment.is_empty() || !mem.user.is_empty() {
        ctx.push_str("\n\n## Long-Term Memory\n");
        if !mem.environment.is_empty() {
            ctx.push_str("### Environment Facts\n");
            for (k, v) in &mem.environment {
                ctx.push_str(&format!("- {}: {}\n", k, v));
            }
        }
        if !mem.user.is_empty() {
            ctx.push_str("### User Preferences\n");
            for (k, v) in &mem.user {
                ctx.push_str(&format!("- {}: {}\n", k, v));
            }
        }
    }

    // Inject Skills
    let skills_block = crate::context::skills::load_relevant_skills(project_path, task);
    if !skills_block.is_empty() {
        ctx.push_str("\n\n");
        ctx.push_str(&skills_block);
    }

    ctx
}

pub fn select_profile_skills(
    project_root: &Path,
    config: &crate::config::NibConfig,
    profile: &crate::profile::Profile,
    goal: &str,
) -> Result<Vec<skills::Skill>, String> {
    if !config.skills.enabled {
        return Ok(Vec::new());
    }
    let mut files = skills::find_skills(project_root);
    let mut configured_paths = config
        .skills
        .paths
        .iter()
        .map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                project_root.join(path)
            }
        })
        .collect::<Vec<_>>();
    configured_paths.extend(profile.skill_paths().iter().cloned());
    configured_paths.push(profile.managed_skills_dir().to_path_buf());
    files.extend(skills::find_skills_in_paths(&configured_paths));
    files.sort();
    files.dedup_by(|left, right| {
        left.canonicalize().unwrap_or_else(|_| left.clone())
            == right.canonicalize().unwrap_or_else(|_| right.clone())
    });

    let active = profile
        .active_skills()
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect::<std::collections::BTreeSet<_>>();
    let parsed = files
        .into_iter()
        .map(|path| {
            skills::parse_skill_file(&path)
                .map_err(|error| format!("invalid skill {}: {error}", path.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(parsed
        .into_iter()
        .filter(|skill| {
            if active.is_empty() {
                skills::skill_matches_task(skill, goal)
            } else {
                active.contains(&skill.frontmatter.name.to_ascii_lowercase())
            }
        })
        .collect())
}

pub fn assemble_runtime_context(
    project_root: &Path,
    goal: &str,
    active_skills: &[skills::Skill],
    memory: &crate::session::memory::MemoryStoreData,
) -> String {
    assemble_runtime_context_sections(project_root, goal, active_skills, memory).render()
}

pub fn assemble_runtime_context_sections(
    project_root: &Path,
    goal: &str,
    active_skills: &[skills::Skill],
    memory: &crate::session::memory::MemoryStoreData,
) -> RuntimeContextSections {
    let skill_sections = active_skills
        .iter()
        .map(|skill| {
            let mut content = format!("{}\n\n{}", skill.frontmatter.description, skill.body);
            for reference in &skill.references {
                content.push_str(&format!(
                    "\n\n#### Skill Reference: {}\n{}",
                    reference.path.display(),
                    reference.content
                ));
            }
            if !skill.assets.is_empty() {
                content.push_str("\n\nVerified skill assets:\n");
                for asset in &skill.assets {
                    content.push_str(&format!("- {}\n", asset.display()));
                }
            }
            RuntimeContextSection {
                label: format!("Skill: {}", skill.frontmatter.name),
                content,
            }
        })
        .collect();

    let mut memory_sections = Vec::new();
    let mut environment = memory.environment.iter().collect::<Vec<_>>();
    environment.sort_by(|left, right| left.0.cmp(right.0));
    memory_sections.extend(
        environment
            .into_iter()
            .map(|(key, value)| RuntimeContextSection {
                label: format!("environment.{key}"),
                content: value.clone(),
            }),
    );
    let mut user = memory.user.iter().collect::<Vec<_>>();
    user.sort_by(|left, right| left.0.cmp(right.0));
    memory_sections.extend(user.into_iter().map(|(key, value)| RuntimeContextSection {
        label: format!("user.{key}"),
        content: value.clone(),
    }));

    RuntimeContextSections {
        agents: load_agents_md(project_root),
        task: goal.to_string(),
        project_docs: project_docs::load_project_docs(project_root),
        skills: skill_sections,
        memory: memory_sections,
        workload: Vec::new(),
        attachments: Vec::new(),
    }
}

pub fn assemble_profile_context(project_root: &Path, task: Option<&str>) -> Result<String, String> {
    let config =
        crate::config::load_nib_config_full(project_root).map_err(|error| error.to_string())?;
    let profiles = crate::profile::ProfileRegistry::load(project_root, &config.profiles)
        .map_err(|error| error.to_string())?;
    let profile = profiles
        .for_workspace(project_root)
        .unwrap_or_else(|| profiles.default_profile());
    profile
        .ensure_state_dirs()
        .map_err(|error| error.to_string())?;
    let goal = task.unwrap_or_default();
    let active_skills = select_profile_skills(profile.root_path(), &config, profile, goal)?;
    let memory = if config.memory.enabled {
        profile.memory_store().load_result()?
    } else {
        crate::session::memory::MemoryStoreData::default()
    };
    Ok(assemble_runtime_context(
        profile.root_path(),
        goal,
        &active_skills,
        &memory,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{save_nib_config_full, NibConfig, ProfileConfig, ProfilesConfig};
    use tempfile::tempdir;

    #[test]
    fn profile_context_uses_selected_memory_and_explicit_skills() {
        let directory = tempdir().expect("tempdir");
        std::fs::write(directory.path().join("AGENTS.md"), "profile instructions")
            .expect("agents file");
        let skill_dir = directory.path().join(".nib/skills/profile-skill");
        std::fs::create_dir_all(&skill_dir).expect("skill directory");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: profile-skill\ndescription: selected profile skill\n---\nProfile body\n",
        )
        .expect("skill file");
        let mut config = NibConfig {
            profiles: ProfilesConfig {
                default: "workspace".to_string(),
                active: vec![ProfileConfig {
                    id: "workspace".to_string(),
                    root: ".".into(),
                    active_skills: vec!["profile-skill".to_string()],
                    ..ProfileConfig::default()
                }],
            },
            ..NibConfig::default()
        };
        save_nib_config_full(directory.path(), &mut config).expect("config");
        let profiles = crate::profile::ProfileRegistry::load(directory.path(), &config.profiles)
            .expect("profiles");
        profiles
            .default_profile()
            .memory_store()
            .set_user("style", "profile-value")
            .expect("profile memory");
        crate::session::memory::MemoryStore::new(directory.path())
            .set_user("style", "legacy-value")
            .expect("legacy memory");

        let context = assemble_profile_context(directory.path(), Some("unrelated task"))
            .expect("profile context");

        assert!(context.contains("profile instructions"));
        assert!(context.contains("Profile body"));
        assert!(context.contains("user.style: profile-value"));
        assert!(!context.contains("legacy-value"));
    }

    #[test]
    fn runtime_context_keeps_long_agents_tail_until_aggregate_bounding() {
        let directory = tempdir().expect("tempdir");
        let tail_rule = "TAIL_RULE_REQUIRES_DURABLE_RECONCILIATION";
        let agents = format!(
            "PROJECT_RULES_HEAD\n{}\n{tail_rule}",
            "long project instruction\n".repeat(2_000)
        );
        std::fs::write(directory.path().join("AGENTS.md"), &agents).expect("agents file");

        let sections = assemble_runtime_context_sections(
            directory.path(),
            "current task",
            &[],
            &crate::session::memory::MemoryStoreData::default(),
        );

        assert!(sections.agents.ends_with(&agents));
        assert!(sections.agents.contains(tail_rule));
        assert!(!sections.agents.contains("...[bounded]..."));
    }

    #[test]
    fn runtime_context_includes_scoped_project_documentation() {
        let directory = tempdir().expect("tempdir");
        let standard = directory.path().join("docs/tech/runtime.md");
        std::fs::create_dir_all(standard.parent().expect("parent")).expect("docs");
        std::fs::write(&standard, "RUNTIME_BOUNDARY_STANDARD").expect("standard");
        let library = directory.path().join("libs/payments/README.md");
        std::fs::create_dir_all(library.parent().expect("parent")).expect("library");
        std::fs::write(&library, "PAYMENTS_DOMAIN_BOUNDARY").expect("library docs");

        let sections = assemble_runtime_context_sections(
            directory.path(),
            "current task",
            &[],
            &crate::session::memory::MemoryStoreData::default(),
        );
        let rendered = sections.render();

        assert_eq!(sections.project_docs.len(), 2);
        assert!(rendered.contains("## Project Standards and Library Documentation"));
        assert!(rendered.contains("docs/tech/runtime.md"));
        assert!(rendered.contains("RUNTIME_BOUNDARY_STANDARD"));
        assert!(rendered.contains("libs/payments/README.md"));
        assert!(rendered.contains("PAYMENTS_DOMAIN_BOUNDARY"));
    }

    #[test]
    fn attachment_context_is_structured_and_bounded() {
        let directory = tempdir().expect("tempdir");
        std::fs::create_dir_all(directory.path().join("src")).expect("src");
        std::fs::write(directory.path().join("src/lib.rs"), "ATTACHMENT_MARKER").expect("file");
        let sections = attachment_context_sections(
            directory.path(),
            &[crate::session::PathAttachment {
                path: "src/lib.rs".to_string(),
            }],
        );
        let rendered = RuntimeContextSections {
            agents: String::new(),
            task: "inspect @src/lib.rs".to_string(),
            project_docs: Vec::new(),
            skills: Vec::new(),
            memory: Vec::new(),
            workload: Vec::new(),
            attachments: sections,
        }
        .render();
        assert!(rendered.contains("## Attached Project Paths"));
        assert!(rendered.contains("src/lib.rs"));
        assert!(rendered.contains("ATTACHMENT_MARKER"));
        assert!(!rendered.contains("inspect @src/lib.rs\nATTACHMENT_MARKER"));
    }
}
