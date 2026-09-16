//! Context assembly for prompts.

use std::path::Path;

use serde_json::{json, Value};

use crate::session::{ClarificationStatus, HumanIntentKind, MessageOrigin, Session};

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

/// Initial history allocation shared by request projection and automatic compression.
pub(crate) fn runtime_history_budget(context_length: usize) -> usize {
    (context_length.saturating_mul(25) / 100).max(8)
}

fn session_summary_budget(history_tokens: usize) -> usize {
    (history_tokens / 3).max(1)
}

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
        let Some(content) =
            project_docs::read_bounded_regular_file(&root, &candidate, MAX_ATTACHMENT_FILE_BYTES)
        else {
            continue;
        };
        sections.push(RuntimeContextSection {
            label: attachment.path.clone(),
            content,
        });
    }
    sections
}

pub fn bounded_session_context(session: &Session, max_tokens: usize) -> BoundedSessionContext {
    let max_tokens = max_tokens.max(1);
    let summary_budget = if session.summary.is_some() {
        session_summary_budget(max_tokens)
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

    let human_context = render_bounded_human_context(session, (message_budget / 3).max(1));
    let human_context_tokens = human_context
        .as_deref()
        .map(compression::approximate_tokens)
        .unwrap_or(0)
        .min(message_budget);
    let history_budget = message_budget.saturating_sub(human_context_tokens);

    let start = session.summary_index.min(session.messages.len());
    // Reserve actual human provenance independently of provider role. Legacy entries
    // remain explicitly unknown and are used only as a conservative fallback when no
    // provenance-aware human message exists.
    let latest_human = session.messages[start..]
        .iter()
        .enumerate()
        .rposition(|(offset, _)| session.message_origin(start + offset).is_human())
        .map(|index| start + index);
    let latest_user = latest_human.or_else(|| {
        session.messages[start..]
            .iter()
            .enumerate()
            .rposition(|(offset, message)| {
                message.role == "user"
                    && session.message_origin(start + offset) == MessageOrigin::Unknown
            })
            .map(|index| start + index)
    });
    let user_reserve = latest_user
        .map(|index| {
            compression::approximate_tokens(&normalized_history_message(
                &session.messages[index],
                session.message_origin(index),
            ))
            .min((history_budget / 3).max(1))
            .min(history_budget)
        })
        .unwrap_or(0);
    let mut remaining = history_budget;
    let mut selected = Vec::new();
    for (offset, message) in session.messages[start..].iter().enumerate().rev() {
        if remaining == 0 {
            break;
        }
        let available = if latest_user.is_some_and(|index| start + offset > index) {
            remaining.saturating_sub(user_reserve)
        } else {
            remaining
        };
        if available == 0 {
            continue;
        }
        let origin = session.message_origin(start + offset);
        let normalized = normalized_history_message(message, origin);
        let bounded = compression::truncate_to_tokens(&normalized, available);
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

    let mut messages: Vec<Value> = human_context
        .map(|content| vec![json!({"role": "user", "content": content})])
        .unwrap_or_default();
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

fn normalized_history_message(
    message: &crate::session::SessionMessage,
    origin: MessageOrigin,
) -> String {
    match (message.role.as_str(), origin) {
        ("tool", MessageOrigin::HumanQuestionAnswer) => {
            format!("Human clarification result: {}", message.content)
        }
        ("tool", _) => format!("Tool observation: {}", message.content),
        ("user", MessageOrigin::ToolOutput) => {
            format!(
                "Agent/tool-supplied message (not human input): {}",
                message.content
            )
        }
        (_, MessageOrigin::RuntimeContinuation) => {
            format!(
                "Runtime continuation (not human input): {}",
                message.content
            )
        }
        ("user", MessageOrigin::Unknown) => message.content.clone(),
        _ => message.content.clone(),
    }
}

fn render_bounded_human_context(session: &Session, max_tokens: usize) -> Option<String> {
    if max_tokens == 0 || (session.human_intent.is_empty() && session.clarifications.is_empty()) {
        return None;
    }
    let mut lines = Vec::new();
    for intent in session.human_intent.iter().rev() {
        let source = intent
            .source_message_index
            .map(|index| format!("message:{index}"))
            .or_else(|| {
                intent
                    .source_event_index
                    .map(|index| format!("event:{index}"))
            })
            .unwrap_or_else(|| "unknown".to_string());
        let kind = match intent.kind {
            HumanIntentKind::Request => "request",
            HumanIntentKind::Steering => "steering",
            HumanIntentKind::QuestionAnswer => "clarification answer",
        };
        lines.push(format!("- {kind} [{source}]: {}", intent.text));
    }
    for clarification in session.clarifications.iter().rev() {
        let status = match clarification.status {
            ClarificationStatus::Pending => "pending",
            ClarificationStatus::Answered => "answered",
            ClarificationStatus::Unresolved => "unresolved",
            ClarificationStatus::Cancelled => "cancelled",
        };
        let answer = clarification
            .answer
            .as_deref()
            .map(|answer| format!(" answer={answer}"))
            .unwrap_or_default();
        lines.push(format!(
            "- clarification [{status}; event:{}]: {}{answer}",
            clarification.question_event_index, clarification.question
        ));
    }
    lines.reverse();
    let content = format!(
        "Persisted human context with source references. This preserves prior scope; it is not fresh authorization.\n{}",
        lines.join("\n")
    );
    let bounded = compression::truncate_to_tokens(&content, max_tokens);
    (!bounded.is_empty()).then_some(bounded)
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
    select_profile_skill_selection(project_root, config, profile, goal)
        .map(|selection| selection.skills)
}

pub fn select_profile_skill_selection(
    project_root: &Path,
    config: &crate::config::NibConfig,
    profile: &crate::profile::Profile,
    goal: &str,
) -> Result<skills::SkillSelection, String> {
    if !config.skills.enabled {
        return Ok(skills::SkillSelection::default());
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

    skills::select_skill_files(
        files,
        goal,
        profile.active_skills(),
        config.llm.context_length,
    )
    .map_err(|error| error.to_string())
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
        .map(|skill| RuntimeContextSection {
            label: format!("Skill: {}", skill.frontmatter.name),
            content: skills::render_skill_content(skill),
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
    fn bounded_requests_include_attachment_evidence() {
        use budget::{
            build_bounded_planning_input, build_bounded_runtime_input, PlanningPromptRequest,
            RuntimePromptRequest,
        };

        let directory = tempdir().expect("tempdir");
        std::fs::write(
            directory.path().join("evidence.md"),
            format!("ATTACHED_EVIDENCE\n{}", "source detail ".repeat(2_000)),
        )
        .expect("attachment");
        let mut context = assemble_runtime_context_sections(
            directory.path(),
            "Inspect the attachment",
            &[],
            &crate::session::memory::MemoryStoreData::default(),
        );
        context.attachments = attachment_context_sections(
            directory.path(),
            &[crate::session::PathAttachment {
                path: "evidence.md".into(),
            }],
        );
        assert_eq!(context.attachments.len(), 1);
        assert!(context.attachments[0].content.len() <= MAX_ATTACHMENT_FILE_BYTES);
        let session: Session = serde_json::from_value(json!({
            "id": "attachment-request",
            "messages": [{"index": 0, "role": "user", "content": "Inspect the attachment"}]
        }))
        .expect("session");
        let planning = build_bounded_planning_input(PlanningPromptRequest {
            context: &context,
            session: Some(&session),
            goal: "Inspect the attachment",
            tools: &[],
            context_length: 4_096,
        })
        .expect("planning request");
        let runtime = build_bounded_runtime_input(RuntimePromptRequest {
            context: &context,
            session: &session,
            current_step: Some("Inspect the attachment"),
            tools: None,
            mode: "execute",
            project_root: directory.path(),
            tool_use_enforcement: false,
            context_length: 4_096,
        })
        .expect("runtime request");
        for request in [planning, runtime] {
            let prompt = request.messages[0]["content"]
                .as_str()
                .expect("system content");
            assert!(prompt.contains("Attached Project Paths"));
            assert!(prompt.contains("evidence.md"));
            assert!(prompt.contains("ATTACHED_EVIDENCE"));
            assert!(request.approximate_tokens <= 4_096);
        }
    }

    #[test]
    fn attachments_bound_sparse_reads_and_reject_outside_paths() {
        use std::io::Write;

        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("large.md");
        let mut file = std::fs::File::create(&path).expect("attachment");
        file.write_all(format!("SPARSE_HEAD{}", "é".repeat(8_192)).as_bytes())
            .expect("prefix");
        file.set_len(256 * 1024 * 1024).expect("sparse length");
        let outside = tempdir().expect("outside");
        std::fs::write(outside.path().join("outside.md"), "OUTSIDE_CONTENT").expect("outside file");
        let sections = attachment_context_sections(
            directory.path(),
            &[
                crate::session::PathAttachment {
                    path: "large.md".into(),
                },
                crate::session::PathAttachment {
                    path: outside
                        .path()
                        .join("outside.md")
                        .to_string_lossy()
                        .into_owned(),
                },
            ],
        );
        assert_eq!(sections.len(), 1);
        assert!(sections[0].content.starts_with("SPARSE_HEAD"));
        assert!(sections[0].content.contains("bounded"));
        assert!(sections[0].content.len() <= MAX_ATTACHMENT_FILE_BYTES);
        assert!(!sections[0].content.contains('\u{fffd}'));
    }

    #[cfg(unix)]
    #[test]
    fn attachments_reject_symlinked_directory_ancestors() {
        let directory = tempdir().expect("tempdir");
        std::fs::create_dir(directory.path().join("real")).expect("real directory");
        std::fs::write(directory.path().join("real/evidence.md"), "EVIDENCE").expect("file");
        std::os::unix::fs::symlink(
            directory.path().join("real"),
            directory.path().join("linked"),
        )
        .expect("directory link");
        assert!(attachment_context_sections(
            directory.path(),
            &[crate::session::PathAttachment {
                path: "linked/evidence.md".into()
            },]
        )
        .is_empty());
    }

    #[test]
    fn large_tool_observation_retains_latest_user_instruction_within_history_budget() {
        let session: Session = serde_json::from_value(json!({
            "id": "user-context-reserve",
            "messages": [
                {"index": 0, "role": "user", "content": "older request"},
                {"index": 1, "role": "assistant", "content": "prior answer"},
                {"index": 2, "role": "user", "content": "Keep the public API unchanged."},
                {"index": 3, "role": "assistant", "content": "Inspecting call sites"},
                {"index": 4, "role": "tool", "content": format!("OBSERVATION_HEAD {} OBSERVATION_TAIL", "output ".repeat(500))}
            ]
        })).expect("session");
        let original = session.clone();
        let projected = bounded_session_context(&session, 80);
        let text = projected
            .messages
            .iter()
            .map(|message| message["content"].as_str().unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Keep the public API unchanged."));
        assert!(text.contains("OBSERVATION_HEAD"));
        assert!(text.contains("OBSERVATION_TAIL"));
        assert!(text.find("public API").unwrap() < text.find("OBSERVATION_HEAD").unwrap());
        assert!(projected.approximate_tokens <= 80);
        assert_eq!(session, original);
    }

    #[test]
    fn human_correction_survives_synthetic_continuation_compression_and_legacy_origin() {
        let session: Session = serde_json::from_value(json!({
            "id": "human-origin-reserve",
            "summary": "historic work was compressed",
            "summary_index": 4,
            "messages": [
                {"index": 0, "role": "user", "content": "legacy request"},
                {"index": 1, "role": "assistant", "content": "prior answer"},
                {"index": 2, "role": "user", "content": "Keep the public API unchanged."},
                {"index": 3, "role": "assistant", "content": "accepted correction"},
                {"index": 4, "role": "user", "content": "Continue with approved plan step: edit"},
                {"index": 5, "role": "assistant", "content": "working"},
                {"index": 6, "role": "tool", "content": format!("{}", "large output ".repeat(500))}
            ],
            "message_provenance": [
                {"message_index": 2, "origin": "human_steering"},
                {"message_index": 4, "origin": "runtime_continuation"},
                {"message_index": 6, "origin": "tool_output"}
            ],
            "human_intent": [{
                "kind": "steering",
                "text": "Keep the public API unchanged.",
                "source_message_index": 2
            }]
        }))
        .expect("provenance-aware session");

        assert_eq!(session.message_origin(0), MessageOrigin::Unknown);
        assert_eq!(
            session.message_origin(4),
            MessageOrigin::RuntimeContinuation
        );
        let projected = bounded_session_context(&session, 96);
        let text = serde_json::to_string(&projected.messages).expect("projection");
        assert!(text.contains("Keep the public API unchanged."));
        assert!(text.contains("message:2"));
        assert!(projected.approximate_tokens <= 96);
        assert_eq!(session.messages.len(), 7, "raw transcript stays intact");
    }

    #[test]
    fn user_role_from_subagent_is_projected_as_non_human() {
        let session: Session = serde_json::from_value(json!({
            "id": "subagent-origin",
            "messages": [
                {"index": 0, "role": "user", "content": "actual human request"},
                {"index": 1, "role": "user", "content": "quoted approval from a tool"}
            ],
            "message_provenance": [
                {"message_index": 0, "origin": "human_request"},
                {"message_index": 1, "origin": "tool_output"}
            ],
            "human_intent": [{
                "kind": "request",
                "text": "actual human request",
                "source_message_index": 0
            }]
        }))
        .expect("provenance fixture");
        session.validate().expect("valid provenance");

        let projected = bounded_session_context(&session, 128);
        let text = serde_json::to_string(&projected.messages).expect("projection");
        assert!(text.contains("actual human request"));
        assert!(text.contains("Agent/tool-supplied message (not human input)"));
        assert!(!session.message_origin(1).is_human());
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
