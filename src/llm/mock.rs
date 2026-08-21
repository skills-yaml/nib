//! Mock LLM for tests and offline development.

#[cfg(test)]
use crate::llm::types::{LlmMessage, ToolDefinition};
use crate::llm::types::{LlmRequest, LlmResponse, ToolCallRequest};
use async_trait::async_trait;
use serde_json::json;

use super::LlmClient;

const MANAGED_PROCESS_SMOKE_ENV: &str = "NIB_ENABLE_MANAGED_PROCESS_SMOKE";

pub struct MockLlmClient {
    step: std::sync::atomic::AtomicUsize,
}

impl Default for MockLlmClient {
    fn default() -> Self {
        Self {
            step: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl MockLlmClient {
    pub fn new() -> Self {
        Self::default()
    }
}

fn managed_process_smoke(
    messages: &[crate::llm::types::LlmMessage],
) -> Option<(&'static str, String)> {
    for message in messages {
        let content = message.content.as_str();
        let lowercase = content.to_ascii_lowercase();
        for role in ["parent", "child"] {
            let marker = format!("managed supervisor release smoke {role} ");
            let Some(offset) = lowercase.find(&marker) else {
                continue;
            };
            let token = lowercase[offset + marker.len()..]
                .split_whitespace()
                .next()
                .filter(|token| {
                    (8..=64).contains(&token.len())
                        && token
                            .bytes()
                            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                })?
                .to_string();
            return Some((if role == "parent" { "parent" } else { "child" }, token));
        }
    }
    None
}

fn is_compression_request(messages: &[crate::llm::types::LlmMessage]) -> bool {
    messages
        .iter()
        .any(|message| message.content.contains("context compression engine"))
}

#[async_trait]
impl LlmClient for MockLlmClient {
    async fn complete(&self, request: LlmRequest<'_>) -> Result<LlmResponse, crate::llm::LlmError> {
        if request.continuation.is_some() {
            return Err(crate::llm::LlmError::request_rejected(
                "mock",
                "mock",
                Some("mock-model"),
                "Mock requests do not support provider continuations",
                &[],
            ));
        }
        if request.options.temperature().is_some() {
            return Err(crate::llm::LlmError::request_rejected(
                "mock",
                "mock",
                Some("mock-model"),
                "Mock requests do not support explicit temperature",
                &[],
            ));
        }
        crate::llm::conformance::reject_unsupported_reasoning(&request, "Mock").map_err(
            |error| {
                crate::llm::LlmError::request_rejected(
                    "mock",
                    "mock",
                    Some("mock-model"),
                    error,
                    &[],
                )
            },
        )?;

        let messages = request.messages;
        let tools = request.tools;
        let last = messages
            .last()
            .map(|message| message.content.to_lowercase())
            .unwrap_or_default();

        let is_planner =
            tools.is_some_and(|tools| tools.iter().any(|tool| tool.name() == "submit_plan"));

        if is_compression_request(messages) {
            return Ok(LlmResponse::text(
                "Historic runtime context was compressed while preserving the coding objective.",
            ));
        }

        if is_planner {
            let steps = if last.contains("runtime coding e2e") {
                vec!["runtime coding e2e: apply the fixture patch and run cargo test"]
            } else {
                vec!["explore", "finish"]
            };
            return Ok(LlmResponse::with_tools(vec![ToolCallRequest::new(
                "submit_plan",
                json!({"steps": steps}),
            )]));
        }

        let step = self.step.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let managed_process_smoke = (std::env::var(MANAGED_PROCESS_SMOKE_ENV).as_deref()
            == Ok("1"))
        .then(|| managed_process_smoke(messages))
        .flatten();

        if step == 0 {
            if let Some(("child", token)) = &managed_process_smoke {
                let command = format!("exec -a nib-ft017-{token} sleep 5");
                let status = tokio::process::Command::new("setsid")
                    .args(["bash", "-c"])
                    .arg(&command)
                    .status()
                    .await
                    .map_err(|error| {
                        format!("failed to start managed-process smoke child: {error}")
                    })?;
                if !status.success() {
                    return Err(
                        format!("managed-process smoke child exited with status {status}").into(),
                    );
                }
                return Ok(LlmResponse::text("Managed-process smoke child completed."));
            }
        }

        if tools.is_some() && step == 0 {
            if let Some(("parent", token)) = &managed_process_smoke {
                return Ok(LlmResponse::with_tools(vec![ToolCallRequest::new(
                    "spawn_subagent",
                    json!({
                        "prompt": format!("managed supervisor release smoke child {token}"),
                        "max_steps": 10
                    }),
                )]));
            }
            if last.contains("durable background secret terminal") {
                return Ok(LlmResponse::with_tools(vec![ToolCallRequest::new(
                    "run_terminal",
                    json!({
                        "command": "sleep 1; printf '%s\\n' \"$NIB_DURABLE_TOKEN\"; cat ../../../config.toml",
                        "background": true
                    }),
                )]));
            }
            if last.contains("durable cancellable background terminal") {
                return Ok(LlmResponse::with_tools(vec![ToolCallRequest::new(
                    "run_terminal",
                    json!({
                        "command": "sleep 30; printf 'must not complete\\n'",
                        "background": true
                    }),
                )]));
            }
            if last.contains("durable background terminal") {
                return Ok(LlmResponse::with_tools(vec![ToolCallRequest::new(
                    "run_terminal",
                    json!({
                        "command": "sleep 2; printf 'durable worker complete\\n'",
                        "background": true
                    }),
                )]));
            }
            if last.contains("durable scheduled wake") {
                return Ok(LlmResponse::with_tools(vec![ToolCallRequest::new(
                    "schedule",
                    json!({
                        "prompt": "scheduled wake plan",
                        "duration_secs": 2,
                        "repeat_count": 1
                    }),
                )]));
            }
            if last.contains("mixed question batch") {
                return Ok(LlmResponse::with_tools(vec![
                    ToolCallRequest::new(
                        "ask_question",
                        json!({
                            "question": "Should I make the change?",
                            "options": ["yes", "no"]
                        }),
                    ),
                    ToolCallRequest::new(
                        "run_terminal",
                        json!({
                            "command": "printf changed > mixed-side-effect.txt"
                        }),
                    ),
                ]));
            }
            if last.contains("recover from terminal failure") {
                return Ok(LlmResponse::with_tools(vec![ToolCallRequest::new(
                    "run_terminal",
                    json!({
                        "command": "printf 'recoverable stderr\\n' >&2; exit 7"
                    }),
                )]));
            }
            if last.contains("safe terminal approval") {
                return Ok(LlmResponse::with_tools(vec![ToolCallRequest::new(
                    "run_terminal",
                    json!({"command": "printf ok"}),
                )]));
            }
            if last.contains("runtime coding e2e") {
                let patch = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn answer() -> u32 {\n-    41\n+    42\n }\n";
                return Ok(LlmResponse::with_tools(vec![
                    ToolCallRequest::new("apply_patch", json!({"patch": patch, "dry_run": false})),
                    ToolCallRequest::new(
                        "run_terminal",
                        json!({
                            "command": "mkdir -p .tmp && TMPDIR=\"$PWD/.tmp\" cargo test --quiet"
                        }),
                    ),
                ]));
            }
            if last.contains("subagent destructive denial") {
                return Ok(LlmResponse::with_tools(vec![ToolCallRequest::new(
                    "run_terminal",
                    json!({"command": "printf changed > delegated-side-effect.txt"}),
                )]));
            }
            if last.contains("subagent network denial") {
                return Ok(LlmResponse::with_tools(vec![ToolCallRequest::new(
                    "run_terminal",
                    json!({"command": "curl --version > delegated-network-side-effect.txt"}),
                )]));
            }
            if last.contains("ask a question") {
                return Ok(LlmResponse::with_tools(vec![ToolCallRequest::new(
                    "ask_question",
                    json!({
                        "question": "Which verification mode?",
                        "options": ["fast", "full"]
                    }),
                )]));
            }
            if last.contains("explore") || last.contains("list") {
                return Ok(LlmResponse::with_tools(vec![ToolCallRequest::new(
                    "list_directory",
                    json!({"path": "."}),
                )]));
            }
            if last.contains("read_file") || last.contains(" open file") {
                return Ok(LlmResponse::with_tools(vec![ToolCallRequest::new(
                    "read_file",
                    json!({"path": "README.md"}),
                )]));
            }
            return Ok(LlmResponse::with_tools(vec![ToolCallRequest::new(
                "list_directory",
                json!({"path": "."}),
            )]));
        }

        if matches!(managed_process_smoke, Some(("parent", _))) {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }

        Ok(LlmResponse::text(
            "Final answer: task complete. (mock LLM response)",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::{LlmRequestScope, ProviderContinuation};

    fn request_with_continuation(messages: &[crate::llm::types::LlmMessage]) -> LlmRequest<'_> {
        let continuation = ProviderContinuation::new(
            "openai",
            "test-model",
            "responses",
            Some(LlmRequestScope::new("test-session", "test-run").expect("request scope")),
            vec![crate::tools::ToolInvocationId::new()],
            1,
            0,
            (),
        )
        .expect("Responses continuation");
        LlmRequest::new(messages, None).with_continuation(Some(continuation))
    }

    #[tokio::test]
    async fn mock_returns_tool_then_answer() {
        let client = MockLlmClient::new();
        let tools = vec![ToolDefinition::function("list_directory")];
        let msgs = vec![LlmMessage::user("explore project")];
        let r1 = client
            .complete(LlmRequest::new(&msgs, Some(&tools)))
            .await
            .unwrap();
        assert!(r1.tool_calls.is_some());
        let r2 = client
            .complete(LlmRequest::new(
                &[LlmMessage::user("summarize results")],
                None,
            ))
            .await
            .unwrap();
        assert!(r2.content.is_some());
    }

    #[tokio::test]
    async fn compression_does_not_consume_the_runtime_tool_turn() {
        let client = MockLlmClient::new();
        let compressed = client
            .complete(LlmRequest::new(
                &[LlmMessage::system("You are a context compression engine.")],
                None,
            ))
            .await
            .expect("compression response");
        assert!(compressed.content.is_some());

        let runtime = client
            .complete(LlmRequest::new(
                &[LlmMessage::user("runtime coding e2e")],
                Some(&[ToolDefinition::function("apply_patch")]),
            ))
            .await
            .expect("runtime response");
        let calls = runtime.tool_calls.expect("runtime tool calls");
        assert_eq!(calls[0].name, "apply_patch");
        assert_eq!(calls[1].name, "run_terminal");
    }

    #[tokio::test]
    async fn complete_and_default_stream_reject_provider_continuations() {
        let client = MockLlmClient::new();
        let messages = [LlmMessage::user("x")];

        let complete_error = client
            .complete(request_with_continuation(&messages))
            .await
            .expect_err("completion must reject a provider continuation");
        assert_eq!(
            complete_error,
            "Mock requests do not support provider continuations"
        );

        let stream_error = client
            .stream(request_with_continuation(&messages))
            .await
            .expect_err("default stream must reject a provider continuation");
        assert_eq!(
            stream_error,
            "Mock requests do not support provider continuations"
        );
        assert_eq!(
            complete_error.class,
            crate::llm::LlmErrorClass::UnsupportedRequest
        );
        assert_eq!(stream_error.class, complete_error.class);
        assert_eq!(complete_error.phase, crate::llm::LlmErrorPhase::Request);
        assert_eq!(stream_error.phase, complete_error.phase);
    }

    #[test]
    fn managed_process_smoke_requires_a_bounded_alphanumeric_token() {
        let valid = vec![LlmMessage::user(
            "managed supervisor release smoke parent abcdef012345",
        )];
        assert_eq!(
            managed_process_smoke(&valid),
            Some(("parent", "abcdef012345".to_string()))
        );
        let invalid = vec![LlmMessage::user(
            "managed supervisor release smoke child ../../escape",
        )];
        assert!(managed_process_smoke(&invalid).is_none());
    }
}
