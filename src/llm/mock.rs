//! Mock LLM for tests and offline development.

#[cfg(test)]
use crate::llm::types::{LlmMessage, ToolDefinition};
use crate::llm::types::{
    LlmRequest, LlmRequestScope, LlmResponse, ProviderContinuation, ToolCallRequest, ToolResult,
};
use crate::tools::ToolInvocationId;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;

use super::LlmClient;

const MANAGED_PROCESS_SMOKE_ENV: &str = "NIB_ENABLE_MANAGED_PROCESS_SMOKE";
const INTERACTIVE_SMOKE_ENV: &str = "NIB_ENABLE_INTERACTIVE_SMOKE";
const INTERACTIVE_SMOKE_GOAL: &str = "interactive queue smoke";
const INTERACTIVE_FAILURE_SMOKE_GOAL: &str = "interactive provider failure smoke";
const EXACT_STEERING_SMOKE_ENV: &str = "NIB_ENABLE_EXACT_STEERING_SMOKE";
const EXACT_STEERING_SMOKE_GOAL: &str = "exact run steering response smoke";
const EXACT_STEERING_TOOL_SMOKE_GOAL: &str = "exact run steering tool smoke";
const EXACT_STEERING_FINAL_TURN_SMOKE_GOAL: &str = "exact run steering final turn smoke";
const EXACT_STEERING_SMOKE_INSTRUCTION: &str = "replacement steering marker";
const MOCK_MODEL: &str = "mock-model";
const MOCK_PROVIDER: &str = "mock";
const MOCK_TRANSPORT: &str = "mock";
const MOCK_RESULT_PREFIX: &str = "Mock tool results: ";

struct MockPendingCall {
    invocation_id: ToolInvocationId,
    provider_correlation: String,
    name: String,
}

struct MockTurnState {
    calls: Vec<MockPendingCall>,
    report_results: bool,
}

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

// Keep the canonical `LlmError` shape used by `LlmClient`; boxing only these
// internal helpers would add an adapter-specific error boundary.
#[allow(clippy::result_large_err)]
fn mock_tool_response(
    calls: Vec<ToolCallRequest>,
    scope: Option<LlmRequestScope>,
    report_results: bool,
) -> Result<LlmResponse, crate::llm::LlmError> {
    let state_calls = calls
        .iter()
        .enumerate()
        .map(|(index, call)| MockPendingCall {
            invocation_id: call.invocation_id,
            provider_correlation: format!("mock-call-{index}"),
            name: call.name.clone(),
        })
        .collect::<Vec<_>>();
    let encoded_bytes = serde_json::to_vec(
        &state_calls
            .iter()
            .map(|call| {
                json!({
                    "provider_correlation": call.provider_correlation,
                    "name": call.name,
                })
            })
            .collect::<Vec<_>>(),
    )
    .map_err(|error| mock_protocol_error(format!("failed to measure Mock continuation: {error}")))?
    .len();
    let continuation = ProviderContinuation::new(
        MOCK_PROVIDER,
        MOCK_MODEL,
        MOCK_TRANSPORT,
        scope,
        state_calls.iter().map(|call| call.invocation_id).collect(),
        state_calls.len(),
        encoded_bytes,
        MockTurnState {
            calls: state_calls,
            report_results,
        },
    )
    .map_err(mock_protocol_error)?;
    let mut response = LlmResponse::with_tools(calls);
    response.continuation = Some(continuation);
    Ok(response)
}

#[allow(clippy::result_large_err)]
fn mock_continuation_response(
    continuation: ProviderContinuation,
    scope: Option<&LlmRequestScope>,
) -> Result<LlmResponse, crate::llm::LlmError> {
    let (state, results): (MockTurnState, BTreeMap<ToolInvocationId, ToolResult>) = continuation
        .consume(MOCK_PROVIDER, MOCK_MODEL, MOCK_TRANSPORT, scope)
        .map_err(mock_continuation_error)?;
    let projected = state
        .calls
        .iter()
        .map(|call| {
            let result = results.get(&call.invocation_id).ok_or_else(|| {
                mock_continuation_error("Mock continuation is missing a correlated tool result")
            })?;
            Ok(json!({
                "tool": call.name,
                "classification": if result.classification().is_error() {
                    "error"
                } else {
                    "success"
                },
                "output": result.output(),
            }))
        })
        .collect::<Result<Vec<Value>, crate::llm::LlmError>>()?;
    if state.report_results {
        let encoded = serde_json::to_string(&projected).map_err(|error| {
            mock_protocol_error(format!("failed to encode Mock results: {error}"))
        })?;
        Ok(LlmResponse::text(format!("{MOCK_RESULT_PREFIX}{encoded}")))
    } else {
        Ok(LlmResponse::text(
            "Final answer: task complete. (mock LLM response)",
        ))
    }
}

fn mock_protocol_error(message: impl AsRef<str>) -> crate::llm::LlmError {
    crate::llm::LlmError::provider_protocol(
        MOCK_PROVIDER,
        MOCK_TRANSPORT,
        Some(MOCK_MODEL),
        crate::llm::LlmErrorPhase::Continuation,
        message,
        &[],
    )
}

fn mock_continuation_error(message: impl AsRef<str>) -> crate::llm::LlmError {
    crate::llm::LlmError::request_rejected(
        MOCK_PROVIDER,
        MOCK_TRANSPORT,
        Some(MOCK_MODEL),
        message,
        &[],
    )
    .with_phase(crate::llm::LlmErrorPhase::Continuation)
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
        crate::llm::conformance::validate_request_capabilities(
            &request,
            "mock",
            crate::llm::registry::ProviderTransport::Local,
            crate::llm::conformance::ProviderOperation::Complete,
        )
        .map_err(|error| {
            crate::llm::LlmError::request_rejected("mock", "mock", Some("mock-model"), error, &[])
        })?;
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

        let LlmRequest {
            messages,
            tools,
            scope,
            continuation,
            ..
        } = request;
        if let Some(continuation) = continuation {
            return mock_continuation_response(continuation, scope.as_ref());
        }
        let last = messages
            .last()
            .map(|message| message.content.to_lowercase())
            .unwrap_or_default();

        let is_planner =
            tools.is_some_and(|tools| tools.iter().any(|tool| tool.name() == "submit_plan"));

        // Keep the release PTY failure/recovery probe deterministic and entirely
        // offline. The fault is available only under the explicit smoke environment
        // and one exact fixture goal; ordinary Mock callers cannot trigger it.
        if is_planner
            && std::env::var(INTERACTIVE_SMOKE_ENV).as_deref() == Ok("1")
            && last.contains(INTERACTIVE_FAILURE_SMOKE_GOAL)
        {
            return Err(crate::llm::LlmError::http(
                MOCK_PROVIDER,
                MOCK_TRANSPORT,
                Some(MOCK_MODEL),
                reqwest::StatusCode::UNAUTHORIZED,
                None,
                "offline interactive smoke authentication rejection",
                &[],
            ));
        }

        if is_compression_request(messages) {
            return Ok(LlmResponse::text(
                "Historic runtime context was compressed while preserving the coding objective.",
            ));
        }

        if is_planner {
            // The release PTY smoke needs a deterministic interval in which a second
            // composer submission can be proven to queue rather than steer. Keep the
            // delay opt-in and bound it to an exact fixture goal so ordinary Mock use
            // remains fast and production providers are never involved.
            if std::env::var(INTERACTIVE_SMOKE_ENV).as_deref() == Ok("1")
                && last.contains(INTERACTIVE_SMOKE_GOAL)
            {
                tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
            }
            let steps = if last.contains("runtime coding e2e") {
                vec!["runtime coding e2e: apply the fixture patch and run cargo test"]
            } else {
                vec!["explore", "finish"]
            };
            return mock_tool_response(
                vec![ToolCallRequest::new("submit_plan", json!({"steps": steps}))],
                scope,
                false,
            );
        }

        let step = self.step.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let exact_steering_final_turn_smoke = messages.iter().any(|message| {
            message
                .content
                .to_ascii_lowercase()
                .contains(EXACT_STEERING_FINAL_TURN_SMOKE_GOAL)
        });
        let exact_steering_smoke = std::env::var(EXACT_STEERING_SMOKE_ENV).as_deref() == Ok("1")
            && messages.iter().any(|message| {
                let content = message.content.to_ascii_lowercase();
                content.contains(EXACT_STEERING_SMOKE_GOAL)
                    || content.contains(EXACT_STEERING_TOOL_SMOKE_GOAL)
                    || content.contains(EXACT_STEERING_FINAL_TURN_SMOKE_GOAL)
            });
        let delayed_response_smoke = messages.iter().any(|message| {
            message
                .content
                .to_ascii_lowercase()
                .contains(EXACT_STEERING_SMOKE_GOAL)
        });
        if exact_steering_smoke && delayed_response_smoke && step == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
        }
        if exact_steering_smoke
            && exact_steering_final_turn_smoke
            && messages.iter().any(|message| {
                message
                    .content
                    .to_ascii_lowercase()
                    .contains(EXACT_STEERING_SMOKE_INSTRUCTION)
            })
        {
            return Ok(LlmResponse::text(
                "Final answer: replacement steering marker observed.",
            ));
        }
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
            let report_results = last.contains("mock parallel continuation");
            if report_results {
                return mock_tool_response(
                    vec![
                        ToolCallRequest::new("record_probe_a", json!({"probe": "a"})),
                        ToolCallRequest::new("record_probe_b", json!({"probe": "b"})),
                    ],
                    scope,
                    true,
                );
            }
            if let Some(("parent", token)) = &managed_process_smoke {
                return mock_tool_response(
                    vec![ToolCallRequest::new(
                        "spawn_subagent",
                        json!({
                            "prompt": format!("managed supervisor release smoke child {token}"),
                            "max_steps": 10
                        }),
                    )],
                    scope,
                    false,
                );
            }
            if last.contains("durable background secret terminal") {
                return mock_tool_response(
                    vec![ToolCallRequest::new(
                        "run_terminal",
                        json!({
                            "command": "sleep 1; printf '%s\\n' \"$NIB_DURABLE_TOKEN\"; cat ../../../config.toml",
                            "background": true
                        }),
                    )],
                    scope,
                    false,
                );
            }
            if last.contains("durable cancellable background terminal") {
                return mock_tool_response(
                    vec![ToolCallRequest::new(
                        "run_terminal",
                        json!({
                            "command": "sleep 30; printf 'must not complete\\n'",
                            "background": true
                        }),
                    )],
                    scope,
                    false,
                );
            }
            if last.contains("durable background terminal") {
                return mock_tool_response(
                    vec![ToolCallRequest::new(
                        "run_terminal",
                        json!({
                            "command": "sleep 2; printf 'durable worker complete\\n'",
                            "background": true
                        }),
                    )],
                    scope,
                    false,
                );
            }
            if last.contains("durable scheduled wake") {
                return mock_tool_response(
                    vec![ToolCallRequest::new(
                        "schedule",
                        json!({
                            "prompt": "scheduled wake plan",
                            "duration_secs": 2,
                            "repeat_count": 1
                        }),
                    )],
                    scope,
                    false,
                );
            }
            if last.contains("mixed question batch") {
                return mock_tool_response(
                    vec![
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
                    ],
                    scope,
                    false,
                );
            }
            if last.contains("recover from terminal failure") {
                return mock_tool_response(
                    vec![ToolCallRequest::new(
                        "run_terminal",
                        json!({
                            "command": "printf 'recoverable stderr\\n' >&2; exit 7"
                        }),
                    )],
                    scope,
                    false,
                );
            }
            if last.contains("safe terminal approval") {
                return mock_tool_response(
                    vec![ToolCallRequest::new(
                        "run_terminal",
                        json!({"command": "printf ok"}),
                    )],
                    scope,
                    false,
                );
            }
            if last.contains(EXACT_STEERING_TOOL_SMOKE_GOAL) {
                return mock_tool_response(
                    vec![ToolCallRequest::new(
                        "run_terminal",
                        json!({"command": "sleep 1; printf 'completed before steering\\n'"}),
                    )],
                    scope,
                    false,
                );
            }
            if last.contains("runtime coding e2e") {
                let patch = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn answer() -> u32 {\n-    41\n+    42\n }\n";
                return mock_tool_response(
                    vec![
                        ToolCallRequest::new(
                            "apply_patch",
                            json!({"patch": patch, "dry_run": false}),
                        ),
                        ToolCallRequest::new(
                            "run_terminal",
                            json!({
                                "command": "mkdir -p .tmp && TMPDIR=\"$PWD/.tmp\" cargo test --quiet"
                            }),
                        ),
                    ],
                    scope,
                    false,
                );
            }
            if last.contains("subagent destructive denial") {
                return mock_tool_response(
                    vec![ToolCallRequest::new(
                        "run_terminal",
                        json!({"command": "printf changed > delegated-side-effect.txt"}),
                    )],
                    scope,
                    false,
                );
            }
            if last.contains("subagent network denial") {
                return mock_tool_response(
                    vec![ToolCallRequest::new(
                        "run_terminal",
                        json!({"command": "curl --version > delegated-network-side-effect.txt"}),
                    )],
                    scope,
                    false,
                );
            }
            if last.contains("ask a question") {
                return mock_tool_response(
                    vec![ToolCallRequest::new(
                        "ask_question",
                        json!({
                            "question": "Which verification mode?",
                            "options": ["fast", "full"]
                        }),
                    )],
                    scope,
                    false,
                );
            }
            if last.contains("explore") || last.contains("list") {
                return mock_tool_response(
                    vec![ToolCallRequest::new("list_directory", json!({"path": "."}))],
                    scope,
                    false,
                );
            }
            if last.contains("read_file") || last.contains(" open file") {
                return mock_tool_response(
                    vec![ToolCallRequest::new(
                        "read_file",
                        json!({"path": "README.md"}),
                    )],
                    scope,
                    false,
                );
            }
            return mock_tool_response(
                vec![ToolCallRequest::new("list_directory", json!({"path": "."}))],
                scope,
                false,
            );
        }

        if matches!(managed_process_smoke, Some(("parent", _))) {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }

        if exact_steering_smoke
            && messages.iter().any(|message| {
                message
                    .content
                    .to_ascii_lowercase()
                    .contains(EXACT_STEERING_SMOKE_INSTRUCTION)
            })
        {
            return Ok(LlmResponse::text(
                "Final answer: replacement steering marker observed.",
            ));
        }

        Ok(LlmResponse::text(
            "Final answer: task complete. (mock LLM response)",
        ))
    }

    async fn stream(
        &self,
        request: LlmRequest<'_>,
    ) -> Result<crate::llm::LlmStream, crate::llm::LlmError> {
        crate::llm::conformance::validate_request_capabilities(
            &request,
            "mock",
            crate::llm::registry::ProviderTransport::Local,
            crate::llm::conformance::ProviderOperation::Stream,
        )
        .map_err(|error| {
            crate::llm::LlmError::request_rejected("mock", "mock", Some("mock-model"), error, &[])
        })?;
        let response = self.complete(request).await?;
        Ok(crate::llm::LlmStream::from_response(response))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::ToolResultClass;

    fn scope(run_id: &str) -> LlmRequestScope {
        LlmRequestScope::new("test-session", run_id).expect("request scope")
    }

    fn foreign_continuation() -> ProviderContinuation {
        ProviderContinuation::new(
            "openai",
            "test-model",
            "responses",
            Some(LlmRequestScope::new("test-session", "test-run").expect("request scope")),
            vec![crate::tools::ToolInvocationId::new()],
            1,
            0,
            (),
        )
        .expect("Responses continuation")
    }

    fn parallel_tools() -> [ToolDefinition; 2] {
        [
            ToolDefinition::function("record_probe_a"),
            ToolDefinition::function("record_probe_b"),
        ]
    }

    async fn parallel_turn(client: &MockLlmClient, request_scope: LlmRequestScope) -> LlmResponse {
        let messages = [LlmMessage::user("mock parallel continuation")];
        let tools = parallel_tools();
        client
            .complete(LlmRequest::new(&messages, Some(&tools)).with_scope(request_scope))
            .await
            .expect("parallel Mock turn")
    }

    #[tokio::test]
    async fn mock_returns_tool_then_answer() {
        let client = MockLlmClient::new();
        let tools = vec![ToolDefinition::function("list_directory")];
        let msgs = vec![LlmMessage::user("explore project")];
        let request_scope = scope("complete-run");
        let r1 = client
            .complete(LlmRequest::new(&msgs, Some(&tools)).with_scope(request_scope.clone()))
            .await
            .unwrap();
        let call = &r1.tool_calls.as_ref().expect("tool call")[0];
        let mut continuation = r1.continuation.expect("Mock continuation");
        continuation
            .record_tool_result(
                ToolResult::success(call.invocation_id, json!({"entries": ["README.md"]}))
                    .expect("bounded result"),
            )
            .expect("correlated result");
        let r2 = client
            .complete(
                LlmRequest::new(&msgs, Some(&tools))
                    .with_scope(request_scope)
                    .with_continuation(Some(continuation)),
            )
            .await
            .unwrap();
        assert_eq!(
            r2.content.as_deref(),
            Some("Final answer: task complete. (mock LLM response)")
        );
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
            .complete(
                LlmRequest::new(
                    &[LlmMessage::user("runtime coding e2e")],
                    Some(&[ToolDefinition::function("apply_patch")]),
                )
                .with_scope(scope("runtime-run")),
            )
            .await
            .expect("runtime response");
        let calls = runtime.tool_calls.expect("runtime tool calls");
        assert_eq!(calls[0].name, "apply_patch");
        assert_eq!(calls[1].name, "run_terminal");
    }

    #[tokio::test]
    async fn invalid_mock_results_and_continuations_fail_before_a_second_step() {
        let capabilities = crate::llm::registry::provider_descriptor("mock")
            .expect("Mock descriptor")
            .capabilities_for(crate::llm::registry::ProviderTransport::Local)
            .expect("Mock local capabilities");
        assert!(capabilities.tools);
        assert!(capabilities.tool_continuation);
        assert!(capabilities.parallel_tools);

        let client = MockLlmClient::new();
        let request_scope = scope("missing-run");
        let first = parallel_turn(&client, request_scope.clone()).await;
        let continuation = first.continuation.expect("continuation");
        let error = client
            .complete(
                LlmRequest::new(&[LlmMessage::user("continue")], None)
                    .with_scope(request_scope)
                    .with_continuation(Some(continuation)),
            )
            .await
            .expect_err("missing results must fail");
        assert!(error.contains("missing one or more tool outputs"));
        assert_eq!(error.phase, crate::llm::LlmErrorPhase::Continuation);
        assert_eq!(client.step.load(std::sync::atomic::Ordering::SeqCst), 1);

        let duplicate_client = MockLlmClient::new();
        let first = parallel_turn(&duplicate_client, scope("duplicate-run")).await;
        let call = &first.tool_calls.as_ref().expect("calls")[0];
        let mut continuation = first.continuation.expect("continuation");
        continuation
            .record_tool_result(
                ToolResult::success(call.invocation_id, json!({"value": 1})).unwrap(),
            )
            .expect("first result");
        let duplicate = continuation
            .record_tool_result(ToolResult::error(call.invocation_id, json!({"value": 2})).unwrap())
            .expect_err("duplicate result");
        assert!(duplicate.contains("already completed"));
        assert_eq!(
            duplicate_client
                .step
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );

        let foreign_client = MockLlmClient::new();
        let first = parallel_turn(&foreign_client, scope("foreign-run")).await;
        let mut continuation = first.continuation.expect("continuation");
        let foreign = continuation
            .record_tool_result(
                ToolResult::success(ToolInvocationId::new(), json!({"value": 1})).unwrap(),
            )
            .expect_err("foreign result");
        assert!(foreign.contains("does not belong"));
        assert_eq!(
            foreign_client
                .step
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );

        let source_client = MockLlmClient::new();
        let source = parallel_turn(&source_client, scope("source-run")).await;
        let replayed_id = source.tool_calls.as_ref().expect("source calls")[0].invocation_id;
        let target_client = MockLlmClient::new();
        let target = parallel_turn(&target_client, scope("target-run")).await;
        let mut target_continuation = target.continuation.expect("target continuation");
        let replay = target_continuation
            .record_tool_result(ToolResult::success(replayed_id, json!({"replay": true})).unwrap())
            .expect_err("replayed result");
        assert!(replay.contains("does not belong"));

        let cross_scope_client = MockLlmClient::new();
        let first_scope = scope("first-run");
        let first = parallel_turn(&cross_scope_client, first_scope).await;
        let calls = first.tool_calls.as_ref().expect("calls");
        let mut continuation = first.continuation.expect("continuation");
        for call in calls {
            continuation
                .record_tool_result(
                    ToolResult::success(call.invocation_id, json!({"ok": true})).unwrap(),
                )
                .expect("result");
        }
        let error = cross_scope_client
            .stream(
                LlmRequest::new(&[LlmMessage::user("continue")], None)
                    .with_scope(scope("different-run"))
                    .with_continuation(Some(continuation)),
            )
            .await
            .expect_err("cross-scope continuation");
        assert!(error.contains("does not match"));
        assert_eq!(
            cross_scope_client
                .step
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );

        let foreign_provider_client = MockLlmClient::new();
        let messages = [LlmMessage::user("x")];
        let complete_error = foreign_provider_client
            .complete(
                LlmRequest::new(&messages, None)
                    .with_scope(scope("foreign-provider-run"))
                    .with_continuation(Some(foreign_continuation())),
            )
            .await
            .expect_err("foreign provider continuation");
        assert!(complete_error.contains("does not match"));
        assert_eq!(
            complete_error.phase,
            crate::llm::LlmErrorPhase::Continuation
        );
        assert_eq!(
            foreign_provider_client
                .step
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[tokio::test]
    async fn private_mock_correlation_is_distinct_from_durable_invocation_ids() {
        let request_scope = scope("private-run");
        let response = parallel_turn(&MockLlmClient::new(), request_scope.clone()).await;
        let calls = response.tool_calls.as_ref().expect("calls");
        assert_ne!(calls[0].invocation_id, calls[1].invocation_id);
        let mut continuation = response.continuation.expect("continuation");
        assert_eq!(
            format!("{continuation:?}"),
            "ProviderContinuation { value: \"<redacted>\" }"
        );
        for call in calls {
            continuation
                .record_tool_result(
                    ToolResult::new(
                        call.invocation_id,
                        json!({"ok": true}),
                        ToolResultClass::Success,
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        let (state, _): (MockTurnState, BTreeMap<ToolInvocationId, ToolResult>) = continuation
            .consume(
                MOCK_PROVIDER,
                MOCK_MODEL,
                MOCK_TRANSPORT,
                Some(&request_scope),
            )
            .expect("Mock state");
        for (index, pending) in state.calls.iter().enumerate() {
            assert_eq!(pending.provider_correlation, format!("mock-call-{index}"));
            assert_ne!(
                pending.provider_correlation,
                pending.invocation_id.to_string()
            );
        }
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
