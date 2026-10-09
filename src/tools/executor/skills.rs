//! Runtime skill capabilities: catalog-scoped reads and audited activation.
use super::*;
use crate::context::skill_catalog::SkillCatalog;
use crate::context::skills::{render_skill_content, Skill, SkillPolicyEffect};

impl ToolExecutor {
    pub fn with_skill_catalog(mut self, catalog: SkillCatalog, active: Vec<Skill>) -> Self {
        self.skill_catalog = Some(catalog);
        self.active_skills = active;
        self
    }

    pub fn with_skill_context_budget(mut self, context_length: usize) -> Self {
        self.skill_context_budget = context_length;
        self
    }

    pub fn skill_context(
        &self,
        context_length: usize,
    ) -> Vec<crate::context::RuntimeContextSection> {
        let mut sections = self
            .active_skills
            .iter()
            .map(|skill| crate::context::RuntimeContextSection {
                label: format!("Skill: {}", skill.frontmatter.name),
                content: render_skill_content(skill),
            })
            .collect::<Vec<_>>();
        if let Some(catalog) = &self.skill_catalog {
            sections.insert(
                0,
                crate::context::RuntimeContextSection {
                    label: "Available skill catalog".into(),
                    content: catalog.prompt(context_length),
                },
            );
        }
        sections
    }

    pub(crate) fn execute_skill_tool(
        &mut self,
        tool: &str,
        args: &Value,
        session: Option<&str>,
    ) -> Result<Value, String> {
        let selector = args
            .get("skill")
            .and_then(Value::as_str)
            .ok_or("skill selector is required")?;
        let catalog = self
            .skill_catalog
            .as_ref()
            .ok_or("skills are disabled or no catalog is configured")?;
        let entry = catalog.resolve(selector)?;
        let already_active = self
            .active_skills
            .iter()
            .any(|skill| skill.path == entry.canonical_path);
        let skill = catalog.load(selector, already_active)?;
        if tool == "read_skill_resource" {
            if !already_active {
                return Err("load the skill before reading its resources".into());
            }
            let requested = args
                .get("path")
                .and_then(Value::as_str)
                .ok_or("resource path is required")?;
            let relative = crate::context::skills::validated_skill_resource_path(requested)
                .map_err(|error| error.to_string())?;
            let root = skill.path.parent().ok_or("skill root is missing")?;
            let path = root.join(relative);
            let bytes = crate::context::skills::read_skill_resource_file(&path, 32_768)?;
            let content = String::from_utf8(bytes).map_err(|error| error.to_string())?;
            catalog.load(selector, true)?;
            return Ok(
                json!({"skill": skill.frontmatter.name, "path": requested, "content": content}),
            );
        }
        if !already_active {
            let required = self
                .active_skills
                .iter()
                .chain(std::iter::once(&skill))
                .map(|skill| {
                    crate::context::compression::approximate_tokens(&render_skill_content(skill))
                })
                .sum::<usize>();
            if required > self.skill_context_budget {
                return Err("activated skills exceed llm.context_length".into());
            }
            let store = self
                .session_store
                .as_ref()
                .ok_or("skill activation requires an authoritative session")?;
            let session = session.ok_or("skill activation requires an authoritative session id")?;
            if store
                .load_result(session)
                .map_err(|error| error.to_string())?
                .is_none()
            {
                return Err("skill activation session does not exist".into());
            }
            store
                .record_skill_usage(
                    session,
                    &skill.frontmatter.name,
                    Some("model selected skill".into()),
                )
                .map_err(|error| error.to_string())?;
            self.install_skill_controls(&skill);
            self.active_skills.push(skill.clone());
        }
        Ok(
            json!({"name":skill.frontmatter.name,"path":skill.path,"content":render_skill_content(&skill),
            "already_active":already_active}),
        )
    }

    fn install_skill_controls(&mut self, skill: &Skill) {
        self.policy_rules.extend(
            crate::context::skills::policy_rules_for_skills(std::slice::from_ref(skill))
                .into_iter()
                .map(|rule| PolicyRule {
                    effect: match rule.effect {
                        SkillPolicyEffect::Deny => PolicyEffect::Deny,
                        SkillPolicyEffect::RequireApproval => PolicyEffect::RequireApproval,
                    },
                    tool_name: rule.tool_name.unwrap_or_else(|| "*".into()),
                    argument_contains: rule.argument_contains,
                    reason: format!("skill '{}' constraint", rule.skill_name),
                }),
        );
        self.after_tool_hooks.extend(
            skill
                .frontmatter
                .hooks
                .after_tool
                .iter()
                .filter(|hook| !hook.tool.trim().is_empty() && !hook.command.trim().is_empty())
                .map(|hook| AfterToolHook {
                    source: skill.frontmatter.name.clone(),
                    tool_name: hook.tool.clone(),
                    command: hook.command.clone(),
                }),
        );
    }
}

#[cfg(test)]
#[path = "skills_tests.rs"]
mod tests;
