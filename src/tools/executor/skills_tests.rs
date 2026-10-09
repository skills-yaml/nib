use super::*;
use tempfile::tempdir;

fn fixture(body: &str) -> (tempfile::TempDir, SkillCatalog, SessionStore, String) {
    let root = tempdir().unwrap();
    let skill = root.path().join(".agents/skills/review");
    std::fs::create_dir_all(skill.join("references")).unwrap();
    std::fs::write(skill.join("SKILL.md"), body).unwrap();
    std::fs::write(skill.join("references/guide.md"), "supporting guide").unwrap();
    let catalog = SkillCatalog::from_roots(&[root.path().join(".agents/skills")], &[]).unwrap();
    let store = SessionStore::new(root.path());
    let session = store.create_session_with_id("skills-session");
    (root, catalog, store, session.id)
}

#[tokio::test]
async fn activation_audits_usage_installs_controls_and_preserves_general_file_scope() {
    let (root, catalog, store, session) = fixture("---\nname: review\ndescription: Review code\nconstraints:\n  deny_tools: [read_file]\nhooks:\n  after_tool:\n    - tool: list_directory\n      command: echo checked\n---\nWorkflow body\n");
    let mut executor = ToolExecutor::new(root.path().into(), ExecutionConfig::default())
        .with_session_store(store.clone())
        .with_skill_catalog(catalog, Vec::new());
    let output = executor
        .execute_skill_tool("load_skill", &json!({"skill":"review"}), Some(&session))
        .unwrap();
    assert!(output["content"]
        .as_str()
        .unwrap()
        .contains("Workflow body"));
    assert_eq!(executor.active_skills.len(), 1);
    assert!(executor
        .policy_rules
        .iter()
        .any(|rule| rule.tool_name == "read_file" && rule.effect == PolicyEffect::Deny));
    assert_eq!(executor.after_tool_hooks.len(), 1);
    assert_eq!(store.load(&session).unwrap().skill_usage.len(), 1);
    executor
        .execute_skill_tool("load_skill", &json!({"skill":"review"}), Some(&session))
        .unwrap();
    assert_eq!(executor.after_tool_hooks.len(), 1);
    assert_eq!(store.load(&session).unwrap().skill_usage.len(), 1);
    let result = executor
        .execute(
            ToolCall {
                invocation_id: crate::tools::ToolInvocationId::new(),
                tool_name: "read_file".into(),
                arguments: json!({"path":".agents/skills/review/SKILL.md"}),
                session_id: Some(session.clone()),
                project_root: Some(root.path().into()),
            },
            Some(&session),
        )
        .await;
    assert!(!result.success);
    assert_eq!(result.approval_source.as_deref(), Some("policy"));
}

#[test]
fn resources_require_activation_and_reject_escape_and_over_budget() {
    let (root, catalog, store, session) =
        fixture("---\nname: review\ndescription: Review code\n---\nWorkflow\n");
    let mut executor = ToolExecutor::new(root.path().into(), ExecutionConfig::default())
        .with_session_store(store)
        .with_skill_catalog(catalog, Vec::new());
    let args = json!({"skill":"review", "path":"references/guide.md"});
    assert!(executor
        .execute_skill_tool("read_skill_resource", &args, Some(&session))
        .is_err());
    executor
        .execute_skill_tool("load_skill", &json!({"skill":"review"}), Some(&session))
        .unwrap();
    assert_eq!(
        executor
            .execute_skill_tool("read_skill_resource", &args, Some(&session))
            .unwrap()["content"],
        "supporting guide"
    );
    for path in ["../outside", "/etc/passwd", "references/../../outside"] {
        assert!(executor
            .execute_skill_tool(
                "read_skill_resource",
                &json!({"skill":"review","path":path}),
                Some(&session)
            )
            .is_err());
    }
    let large = root.path().join(".agents/skills/review/large.txt");
    std::fs::write(&large, vec![b'a'; 32_769]).unwrap();
    assert!(executor
        .execute_skill_tool(
            "read_skill_resource",
            &json!({"skill":"review","path":"large.txt"}),
            Some(&session)
        )
        .is_err());
}

#[test]
fn failed_usage_audit_does_not_activate_a_skill() {
    let (root, catalog, store, _) = fixture("---\nname: review\n---\nWorkflow\n");
    let mut executor = ToolExecutor::new(root.path().into(), ExecutionConfig::default())
        .with_session_store(store)
        .with_skill_catalog(catalog, Vec::new());
    assert!(executor
        .execute_skill_tool(
            "load_skill",
            &json!({"skill":"review"}),
            Some("missing-session")
        )
        .is_err());
    assert!(executor.active_skills.is_empty());
    assert!(executor.after_tool_hooks.is_empty());
}

#[tokio::test]
async fn skill_tools_are_only_advertised_with_a_catalog_and_not_exposed_by_mcp() {
    let (root, catalog, _, _) = fixture("---\nname: review\n---\nWorkflow\n");
    let executor = ToolExecutor::new(root.path().into(), ExecutionConfig::default());
    assert!(!executor
        .get_tools_schema()
        .await
        .iter()
        .any(|tool| tool["function"]["name"] == "load_skill"));
    let executor = executor.with_skill_catalog(catalog, Vec::new());
    assert!(executor
        .get_tools_schema()
        .await
        .iter()
        .any(|tool| tool["function"]["name"] == "load_skill"));
    assert!(!get_tool_metadata("load_skill").unwrap().mcp_exposable);
}

#[tokio::test]
async fn skill_activation_and_constraints_survive_session_permission_modes() {
    for mode in [
        ApprovalMode::Manual,
        ApprovalMode::Plan,
        ApprovalMode::Smart,
        ApprovalMode::Policy,
        ApprovalMode::Off,
    ] {
        let (root, catalog, store, session) =
            fixture("---\nname: review\nconstraints:\n  deny_tools: [read_file]\n---\nWorkflow\n");
        let mut executor = ToolExecutor::new(root.path().into(), ExecutionConfig::default())
            .with_session_store(store.clone())
            .with_skill_catalog(catalog, Vec::new())
            .with_approval_mode(mode);
        let call = |name: &str, arguments| ToolCall {
            invocation_id: crate::tools::ToolInvocationId::new(),
            tool_name: name.into(),
            arguments,
            session_id: Some(session.clone()),
            project_root: Some(root.path().into()),
        };
        let activation = executor
            .execute(
                call("load_skill", json!({"skill": "review"})),
                Some(&session),
            )
            .await;
        assert!(activation.success, "{mode:?}: {activation:?}");
        assert_eq!(store.load(&session).unwrap().skill_usage.len(), 1);
        let resource = executor
            .execute(
                call(
                    "read_skill_resource",
                    json!({"skill": "review", "path": "references/guide.md"}),
                ),
                Some(&session),
            )
            .await;
        assert!(resource.success, "{mode:?}: {resource:?}");
        let denied = executor
            .execute(
                call(
                    "read_file",
                    json!({"path": ".agents/skills/review/SKILL.md"}),
                ),
                Some(&session),
            )
            .await;
        assert!(
            !denied.success,
            "{mode:?}: skill policy must remain authoritative"
        );
        assert_eq!(denied.approval_source.as_deref(), Some("policy"));
    }
}
