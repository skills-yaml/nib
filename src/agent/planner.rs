use crate::llm::LlmClient;
use crate::session::{Plan, PlanStep};
use serde_json::json;
use std::sync::Arc;

pub async fn generate_plan(llm: &Arc<dyn LlmClient>, goal: &str) -> Result<Plan, String> {
    let system_prompt = "You are a senior planner agent. Generate a step-by-step plan for the following goal. Use the `submit_plan` tool to submit the plan.";

    let messages = vec![
        json!({ "role": "system", "content": system_prompt }),
        json!({ "role": "user", "content": goal }),
    ];

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

    let response = llm
        .complete(&messages, Some(tools.as_array().unwrap()), 0.3)
        .await?;

    if let Some(calls) = response.tool_calls {
        for call in calls {
            if call.name == "submit_plan" {
                if let Some(steps) = call.arguments.get("steps").and_then(|s| s.as_array()) {
                    let plan_steps: Vec<PlanStep> = steps
                        .iter()
                        .filter_map(|s| s.as_str())
                        .map(|s| PlanStep {
                            description: s.to_string(),
                            status: "Pending".to_string(),
                        })
                        .collect();
                    return Ok(Plan {
                        steps: plan_steps,
                        current_step_index: 0,
                    });
                }
            }
        }
    }

    Ok(Plan {
        steps: vec![PlanStep {
            description: goal.to_string(),
            status: "Pending".to_string(),
        }],
        current_step_index: 0,
    })
}
