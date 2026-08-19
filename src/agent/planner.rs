use crate::context::budget::{build_bounded_planning_input, PlanningPromptRequest};
use crate::context::RuntimeContextSections;
use crate::llm::types::{LlmRequest, LlmRequestScope, StreamEvent, ToolCallRequest};
use crate::llm::LlmClient;
use crate::session::{Plan, PlanStep};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;

pub async fn generate_plan(
    llm: &Arc<dyn LlmClient>,
    goal: &str,
) -> Result<Plan, crate::llm::LlmError> {
    generate_plan_with_events(llm, goal, None).await
}

pub async fn generate_plan_with_events(
    llm: &Arc<dyn LlmClient>,
    goal: &str,
    event_tx: Option<&Sender<StreamEvent>>,
) -> Result<Plan, crate::llm::LlmError> {
    generate_plan_with_events_bounded(llm, goal, event_tx, 128_000).await
}

pub async fn generate_plan_with_events_bounded(
    llm: &Arc<dyn LlmClient>,
    goal: &str,
    event_tx: Option<&Sender<StreamEvent>>,
    context_length: usize,
) -> Result<Plan, crate::llm::LlmError> {
    let context = RuntimeContextSections {
        agents: String::new(),
        task: goal.to_string(),
        project_docs: Vec::new(),
        skills: Vec::new(),
        memory: Vec::new(),
        workload: Vec::new(),
    };
    generate_plan_with_context_events_bounded(llm, goal, &context, None, event_tx, context_length)
        .await
}

pub async fn generate_plan_with_context_events_bounded(
    llm: &Arc<dyn LlmClient>,
    goal: &str,
    context: &RuntimeContextSections,
    session: Option<&crate::session::Session>,
    event_tx: Option<&Sender<StreamEvent>>,
    context_length: usize,
) -> Result<Plan, crate::llm::LlmError> {
    generate_plan_with_context_events_bounded_scoped(
        llm,
        goal,
        context,
        session,
        event_tx,
        context_length,
        None,
    )
    .await
}

pub async fn generate_plan_with_context_events_bounded_scoped(
    llm: &Arc<dyn LlmClient>,
    goal: &str,
    context: &RuntimeContextSections,
    session: Option<&crate::session::Session>,
    event_tx: Option<&Sender<StreamEvent>>,
    context_length: usize,
    scope: Option<LlmRequestScope>,
) -> Result<Plan, crate::llm::LlmError> {
    if goal.trim().is_empty() {
        return Err("cannot plan an empty goal".into());
    }

    let tools = json!([{
        "type": "function",
        "function": {
            "name": "submit_plan",
            "description": "Submit a structured plan",
            "parameters": {
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "items": {
                            "type": "string"
                        }
                    }
                },
                "required": ["steps"]
            }
        }
    }]);

    let bounded = build_bounded_planning_input(PlanningPromptRequest {
        context,
        session,
        goal,
        tools: tools.as_array().unwrap(),
        context_length,
    })?;
    let scope = match scope {
        Some(scope) => scope,
        None => LlmRequestScope::new(
            "standalone-planner",
            uuid::Uuid::new_v4().simple().to_string(),
        )?,
    };
    let request =
        LlmRequest::new(&bounded.messages, bounded.tools.as_deref(), 0.3).with_scope(scope);
    let mut stream = llm.stream(request).await?;
    while let Some(result) = stream.recv().await {
        let event = result.map_err(|error| *error)?;
        if let Some(tx) = event_tx.filter(|_| matches!(&event, StreamEvent::Content(_))) {
            let _ = tx.send(event).await;
        }
    }
    let completed = stream.finish().await?;
    plan_from_tool_calls(goal, completed.tool_calls.unwrap_or_default()).map_err(Into::into)
}

pub fn plan_from_tool_calls(goal: &str, calls: Vec<ToolCallRequest>) -> Result<Plan, String> {
    let call = calls
        .into_iter()
        .find(|call| call.name == "submit_plan")
        .ok_or_else(|| "planner did not submit a structured plan".to_string())?;
    let steps = call
        .arguments
        .get("steps")
        .and_then(|steps| steps.as_array())
        .ok_or_else(|| "structured plan is missing a steps array".to_string())?;
    let plan_steps: Vec<PlanStep> = steps
        .iter()
        .filter_map(|step| {
            step.as_str()
                .or_else(|| step.get("description").and_then(|value| value.as_str()))
        })
        .map(str::trim)
        .filter(|step| !step.is_empty())
        .map(|description| PlanStep {
            description: description.to_string(),
            status: "Pending".to_string(),
            outcome: None,
            attempts: 0,
            updated_at: None,
        })
        .collect();
    let plan = Plan::new(goal, plan_steps);
    if !plan.is_structured() {
        return Err("planner submitted an empty or invalid plan".to_string());
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::budget::approximate_llm_input_tokens;
    use crate::context::RuntimeContextSection;
    use crate::llm::types::LlmResponse;
    use serde_json::Value;
    use std::sync::Mutex;

    #[test]
    fn parses_non_empty_structured_plan() {
        let plan = plan_from_tool_calls(
            "  inspect\tand verify ",
            vec![ToolCallRequest::new(
                "submit_plan",
                json!({"steps": ["inspect", {"description": "verify"}]}),
            )],
        )
        .unwrap();
        assert_eq!(plan.steps.len(), 2);
        assert!(!plan.approved);
        assert_eq!(plan.goal, "inspect and verify");
        assert!(plan.id.starts_with("plan-"));
    }

    #[test]
    fn rejects_empty_structured_plan() {
        let error = plan_from_tool_calls(
            "empty plan",
            vec![ToolCallRequest::new("submit_plan", json!({"steps": [" "]}))],
        )
        .unwrap_err();
        assert!(error.contains("empty or invalid"));
    }

    type RecordedPlannerRequest = (Vec<Value>, Option<Vec<Value>>);

    #[derive(Default)]
    struct RecordingPlannerLlm {
        request: Mutex<Option<RecordedPlannerRequest>>,
    }

    #[async_trait::async_trait]
    impl LlmClient for RecordingPlannerLlm {
        async fn complete(
            &self,
            request: LlmRequest<'_>,
        ) -> Result<LlmResponse, crate::llm::LlmError> {
            *self.request.lock().expect("request lock") = Some((
                request.messages.to_vec(),
                request.tools.map(<[Value]>::to_vec),
            ));
            Ok(LlmResponse::with_tools(vec![ToolCallRequest::new(
                "submit_plan",
                json!({"steps": ["inspect context", "perform work", "verify"]}),
            )]))
        }
    }

    #[tokio::test]
    async fn contextual_planner_receives_bounded_runtime_and_session_markers() {
        let recorder = Arc::new(RecordingPlannerLlm::default());
        let llm: Arc<dyn LlmClient> = recorder.clone();
        let context = RuntimeContextSections {
            agents: "AGENTS_PLANNER_MARKER follow the project rule".to_string(),
            task: "GOAL_PLANNER_MARKER implement the requested change".to_string(),
            project_docs: vec![RuntimeContextSection {
                label: "docs/standards/planner.md".to_string(),
                content: "PROJECT_DOC_PLANNER_MARKER follow the library boundary".to_string(),
            }],
            skills: vec![RuntimeContextSection {
                label: "Skill: selected-skill-marker".to_string(),
                content: "SKILL_PLANNER_MARKER use the selected workflow".to_string(),
            }],
            memory: vec![RuntimeContextSection {
                label: "user.preference-marker".to_string(),
                content: "MEMORY_PLANNER_MARKER keep verification deterministic".to_string(),
            }],
            workload: vec![RuntimeContextSection {
                label: "workload.snapshot-marker".to_string(),
                content: "WORKLOAD_PLANNER_MARKER active=1 prepared=0".to_string(),
            }],
        };
        let session: crate::session::Session = serde_json::from_value(json!({
            "id": "planner-session-marker",
            "messages": [
                {"index": 0, "role": "user", "content": "SESSION_PLANNER_MARKER prior request"}
            ]
        }))
        .expect("session");
        let context_length = 900;

        let plan = generate_plan_with_context_events_bounded(
            &llm,
            "GOAL_PLANNER_MARKER implement the requested change",
            &context,
            Some(&session),
            None,
            context_length,
        )
        .await
        .expect("plan");

        assert_eq!(plan.steps.len(), 3);
        let (messages, tools) = recorder
            .request
            .lock()
            .expect("request lock")
            .clone()
            .expect("recorded request");
        assert!(approximate_llm_input_tokens(&messages, tools.as_deref()) <= context_length);
        let prompt = serde_json::to_string(&messages).expect("prompt json");
        for marker in [
            "AGENTS_PLANNER_MARKER",
            "PROJECT_DOC_PLANNER_MARKER",
            "SKILL_PLANNER_MARKER",
            "MEMORY_PLANNER_MARKER",
            "WORKLOAD_PLANNER_MARKER",
            "SESSION_PLANNER_MARKER",
            "planner-session-marker",
        ] {
            assert!(prompt.contains(marker), "missing planner marker: {marker}");
        }
    }
}
