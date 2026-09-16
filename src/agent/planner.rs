use crate::context::budget::{
    build_bounded_planning_input, ensure_required_instructions_present, PlanningPromptRequest,
};
use crate::context::RuntimeContextSections;
use crate::llm::types::{LlmRequest, LlmRequestScope, StreamEvent, ToolCallRequest};
use crate::llm::{LlmClient, LlmResponse, LlmStream};
use crate::session::{Plan, PlanStep, VerificationObligation};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;

fn planning_tools() -> serde_json::Value {
    json!([{
        "type": "function",
        "function": {
            "name": "submit_plan",
            "description": "Submit a structured plan",
            "parameters": {
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 64,
                        "items": {
                            "anyOf": [
                                {"type": "string", "minLength": 1, "maxLength": 4096},
                                {
                                    "type": "object",
                                    "properties": {
                                        "description": {"type": "string", "minLength": 1, "maxLength": 4096},
                                        "verification_obligations": {
                                            "type": "array",
                                            "maxItems": 16,
                                            "items": {
                                                "type": "object",
                                                "properties": {
                                                    "id": {"type": "string", "minLength": 1, "maxLength": 128},
                                                    "description": {"type": "string", "minLength": 1, "maxLength": 1024},
                                                    "required": {"type": "boolean", "default": true},
                                                    "affected_paths": {
                                                        "type": "array",
                                                        "maxItems": 32,
                                                        "items": {"type": "string", "minLength": 1, "maxLength": 4096}
                                                    }
                                                },
                                                "required": ["id", "description"],
                                                "additionalProperties": false
                                            }
                                        }
                                    },
                                    "required": ["description"],
                                    "additionalProperties": false
                                }
                            ]
                        }
                    }
                },
                "required": ["steps"]
            }
        }
    }])
}

pub(crate) fn validate_planning_instruction_context(
    goal: &str,
    context: &RuntimeContextSections,
    session: Option<&crate::session::Session>,
    context_length: usize,
) -> Result<(), String> {
    let tools = planning_tools();
    let bounded = build_bounded_planning_input(PlanningPromptRequest {
        context,
        session,
        goal,
        tools: tools
            .as_array()
            .expect("planning tool schema is always an array"),
        context_length,
    })?;
    ensure_required_instructions_present(&bounded, &context.agents)
}

// Planning APIs preserve the canonical typed LLM failure (including retry/phase metadata) for
// their callers. Boxing only these adapters would create a parallel error contract without
// reducing the authoritative error type.
#[allow(clippy::result_large_err)]
pub async fn generate_plan(
    llm: &Arc<dyn LlmClient>,
    goal: &str,
) -> Result<Plan, crate::llm::LlmError> {
    generate_plan_with_events(llm, goal, None).await
}

#[allow(clippy::result_large_err)]
pub async fn generate_plan_with_events(
    llm: &Arc<dyn LlmClient>,
    goal: &str,
    event_tx: Option<&Sender<StreamEvent>>,
) -> Result<Plan, crate::llm::LlmError> {
    generate_plan_with_events_bounded(llm, goal, event_tx, 128_000).await
}

#[allow(clippy::result_large_err)]
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
        attachments: Vec::new(),
    };
    generate_plan_with_context_events_bounded(llm, goal, &context, None, event_tx, context_length)
        .await
}

#[allow(clippy::result_large_err)]
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

#[allow(clippy::result_large_err)]
pub async fn generate_plan_with_context_events_bounded_scoped(
    llm: &Arc<dyn LlmClient>,
    goal: &str,
    context: &RuntimeContextSections,
    session: Option<&crate::session::Session>,
    _event_tx: Option<&Sender<StreamEvent>>,
    context_length: usize,
    scope: Option<LlmRequestScope>,
) -> Result<Plan, crate::llm::LlmError> {
    if goal.trim().is_empty() {
        return Err("cannot plan an empty goal".into());
    }

    let tools = planning_tools();

    let bounded = build_bounded_planning_input(PlanningPromptRequest {
        context,
        session,
        goal,
        tools: tools.as_array().unwrap(),
        context_length,
    })?;
    ensure_required_instructions_present(&bounded, &context.agents)?;
    let scope = match scope {
        Some(scope) => scope,
        None => LlmRequestScope::new(
            "standalone-planner",
            uuid::Uuid::new_v4().simple().to_string(),
        )?,
    };
    let typed_messages = crate::llm::LlmMessage::from_openai_values(&bounded.messages)?;
    let typed_tools = crate::llm::ToolDefinition::from_openai_values_opt(bounded.tools.as_deref())?;
    let request = LlmRequest::new(&typed_messages, typed_tools.as_deref()).with_scope(scope);
    let completed = finish_private_planning_stream(llm.stream(request).await?).await?;
    let plan = plan_from_tool_calls(goal, completed.tool_calls.unwrap_or_default())
        .map_err(crate::llm::LlmError::from)?;
    Ok(plan)
}

#[allow(clippy::result_large_err)]
async fn finish_private_planning_stream(
    stream: LlmStream,
) -> Result<LlmResponse, crate::llm::LlmError> {
    // `LlmStream` exposes no unvalidated provider-delta receiver outside `crate::llm`.
    // Planning keeps the completed response private for the additional structured-plan
    // validation boundary.
    stream.finish().await
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
    let plan_steps = steps
        .iter()
        .map(parse_plan_step)
        .collect::<Result<Vec<_>, _>>()?;
    let plan = Plan::new(goal, plan_steps);
    if !plan.is_structured() {
        return Err("planner submitted an empty or invalid plan".to_string());
    }
    Ok(plan)
}

fn parse_plan_step(step: &serde_json::Value) -> Result<PlanStep, String> {
    let description = step
        .as_str()
        .or_else(|| step.get("description").and_then(serde_json::Value::as_str))
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .ok_or_else(|| "planner submitted an empty or invalid plan".to_string())?;
    let verification_values = step
        .get("verification_obligations")
        .map(|value| {
            value
                .as_array()
                .cloned()
                .ok_or_else(|| "verification_obligations must be an array".to_string())
        })
        .transpose()?
        .unwrap_or_default();
    if verification_values.len() > 16 {
        return Err("a plan step cannot declare more than 16 verification obligations".to_string());
    }
    let verification_obligations = verification_values
        .into_iter()
        .map(|obligation| {
            let id = obligation
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| "verification obligation is missing an id".to_string())?;
            let obligation_description = obligation
                .get("description")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|description| !description.is_empty())
                .ok_or_else(|| {
                    format!("verification obligation {id:?} is missing a description")
                })?;
            let affected_paths = obligation
                .get("affected_paths")
                .map(|paths| {
                    paths
                        .as_array()
                        .ok_or_else(|| {
                            format!(
                                "verification obligation {id:?} affected_paths must be an array"
                            )
                        })?
                        .iter()
                        .map(|path| {
                            path.as_str()
                                .map(str::trim)
                                .filter(|path| !path.is_empty())
                                .map(str::to_string)
                                .ok_or_else(|| {
                                    format!(
                                        "verification obligation {id:?} has an invalid affected path"
                                    )
                                })
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?
                .unwrap_or_default();
            let mut parsed =
                VerificationObligation::pending(id, obligation_description, affected_paths);
            parsed.required = obligation
                .get("required")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            Ok(parsed)
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(PlanStep {
        description: description.to_string(),
        status: "Pending".to_string(),
        outcome: None,
        attempts: 0,
        updated_at: None,
        verification_obligations,
        content_generation: 0,
    })
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
    fn parses_bounded_verification_obligations_into_the_plan() {
        let plan = plan_from_tool_calls(
            "repair and verify",
            vec![ToolCallRequest::new(
                "submit_plan",
                json!({
                    "steps": [{
                        "description": "repair the defect",
                        "verification_obligations": [{
                            "id": "required-check",
                            "description": "run the focused behavior test",
                            "affected_paths": ["src/agent", "tests/test_runtime_e2e.rs"]
                        }]
                    }]
                }),
            )],
        )
        .expect("structured verification plan");

        let obligation = &plan.steps[0].verification_obligations[0];
        assert_eq!(obligation.id, "required-check");
        assert!(obligation.required);
        assert_eq!(
            obligation.status,
            crate::session::VerificationStatus::Pending
        );
        assert_eq!(
            obligation.affected_paths,
            ["src/agent", "tests/test_runtime_e2e.rs"]
        );
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

    #[tokio::test]
    async fn planning_deltas_remain_private_when_terminal_validation_fails() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.send(Ok(crate::llm::LlmStreamEvent::Delta(
            crate::llm::LlmDelta::Content("private planning echo\u{1b}[31m".to_string()),
        )))
        .await
        .expect("delta");
        tx.send(Err(crate::llm::LlmStreamFailure::from(
            "late planning rejection",
        )))
        .await
        .expect("failure");
        drop(tx);

        let error = finish_private_planning_stream(LlmStream::from_public_receiver(rx))
            .await
            .expect_err("late failure rejects private planning output");
        assert_eq!(error.class, crate::llm::LlmErrorClass::Protocol);
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
                request
                    .messages
                    .iter()
                    .map(crate::llm::LlmMessage::to_openai_chat)
                    .collect(),
                request.tools.map(|tools| {
                    tools
                        .iter()
                        .map(crate::llm::ToolDefinition::to_openai_tool)
                        .collect()
                }),
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
            attachments: Vec::new(),
        };
        let session: crate::session::Session = serde_json::from_value(json!({
            "id": "planner-session-marker",
            "messages": [
                {"index": 0, "role": "user", "content": "SESSION_PLANNER_MARKER prior request"}
            ]
        }))
        .expect("session");
        let context_length = 2_400;

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
