//! Shared provider-neutral request validation and adapter conformance fixtures.

use super::types::{LlmRequest, ReasoningOption};
use crate::config::ReasoningEffort;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderOperation {
    Complete,
    Stream,
}

/// Enforces registry-declared structural support before an adapter builds or sends a
/// wire request. Provider-specific validators may narrow the accepted shape further.
pub(crate) fn validate_request_capabilities(
    request: &LlmRequest<'_>,
    provider: &str,
    transport: crate::llm::registry::ProviderTransport,
    operation: ProviderOperation,
) -> Result<(), String> {
    let descriptor = crate::llm::registry::provider_descriptor(provider)
        .or_else(|| {
            matches!(
                transport,
                crate::llm::registry::ProviderTransport::ChatCompletions
                    | crate::llm::registry::ProviderTransport::Responses
            )
            .then(|| crate::llm::registry::provider_descriptor("openai"))
            .flatten()
        })
        .ok_or_else(|| format!("unsupported LLM provider: {provider}"))?;
    let capabilities = descriptor.capabilities_for(transport).ok_or_else(|| {
        format!(
            "provider {provider} does not declare the {} transport",
            transport.as_str()
        )
    })?;
    let operation_supported = match operation {
        ProviderOperation::Complete => capabilities.complete,
        ProviderOperation::Stream => capabilities.stream,
    };
    if !operation_supported {
        return Err(format!(
            "provider {provider} does not support {} over {}",
            match operation {
                ProviderOperation::Complete => "complete",
                ProviderOperation::Stream => "stream",
            },
            transport.as_str()
        ));
    }
    if request.tools.is_some_and(|tools| !tools.is_empty()) && !capabilities.tools {
        return Err(format!(
            "provider {provider} does not support custom function tools over {}",
            transport.as_str()
        ));
    }
    if request.continuation.is_some() && !capabilities.tool_continuation {
        return Err(format!(
            "provider {provider} does not support structured correlated tool results over {}",
            transport.as_str()
        ));
    }
    if request.options.reasoning() != ReasoningOption::ProviderDefault
        && capabilities.reasoning
            != crate::llm::registry::ProviderReasoningSupport::ConfigurableEffort
    {
        let label = match descriptor.implementation {
            crate::llm::registry::ProviderImplementation::Mock => "Mock",
            crate::llm::registry::ProviderImplementation::Anthropic => "Anthropic",
            crate::llm::registry::ProviderImplementation::Gemini => "Gemini",
            _ => descriptor.display_name,
        };
        return Err(format!("{label} requests do not support reasoning_effort"));
    }
    Ok(())
}

pub fn reject_explicit_temperature_for_responses(request: &LlmRequest<'_>) -> Result<(), String> {
    if request.options.temperature().is_some() {
        return Err("Responses requests do not support explicit temperature".to_string());
    }
    Ok(())
}

pub fn reject_unsupported_reasoning(
    request: &LlmRequest<'_>,
    provider: &str,
) -> Result<(), String> {
    if request.options.reasoning() != ReasoningOption::ProviderDefault {
        return Err(format!(
            "{provider} requests do not support reasoning_effort"
        ));
    }
    Ok(())
}

pub fn resolved_reasoning(
    request: &LlmRequest<'_>,
    configured: Option<ReasoningEffort>,
) -> Option<ReasoningEffort> {
    request.options.resolved_reasoning(configured)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LlmConfig, ProviderEntry};
    use crate::llm::factory::create_client;
    use crate::llm::mock::MockLlmClient;
    use crate::llm::openai::OpenAiCompatClient;
    use crate::llm::registry::{ProviderTransport, PROVIDERS};
    use crate::llm::responses::OpenAiResponsesClient;
    use crate::llm::test_support::{
        serve_once, serve_once_with_declared_length, serve_once_with_headers, serve_open_stream,
        serve_sequence, ScriptedHttpResponse,
    };
    use crate::llm::types::{
        GenerationOptions, LlmMessage, LlmRequestScope, ToolDefinition, ToolResult,
    };
    use crate::llm::{
        LlmDelta, LlmError, LlmErrorClass, LlmErrorPhase, LlmFinishReason, LlmProvider,
        LlmResponse, LlmStreamEvent, LlmTerminalStatus, RetryDisposition,
    };
    use serde_json::{json, Value};
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{mpsc::Receiver, Arc};
    use std::time::Duration;

    const ACTIVE_CONFORMANCE_KEY: &str = "active-secret-42";
    const INACTIVE_CONFORMANCE_KEY: &str = "inactive-secret-84";
    const REMOTE_PROMPT_SENTINEL: &str = "remote-prompt-sentinel-7531";
    const REMOTE_LABEL_SENTINEL: &str =
        "provider=openai transport=responses model=remote endpoint=/private";
    const SAFE_FIXTURE_MODEL: &str = "fixture-model";

    // Endpoint/transport provenance was reverified from primary provider references
    // on 2026-08-26. These fixtures assert the exact paths documented at:
    // - OpenAI Responses: https://developers.openai.com/api/reference/resources/responses/methods/create
    // - xAI Chat: https://docs.x.ai/developers/model-capabilities/legacy/chat-completions
    // - OpenRouter Chat: https://openrouter.ai/docs/quickstart
    // - Anthropic Messages: https://platform.claude.com/docs/en/api/overview
    // - Gemini GenerateContent: https://ai.google.dev/api/generate-content
    // Meta intentionally has no default endpoint; its compatible fixture always uses
    // an explicit local endpoint and factory readiness fails without one.

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ConformanceAdapter {
        Chat(&'static str),
        Responses(&'static str),
        Anthropic,
        Gemini,
        Mock,
    }

    const ALL_ADAPTERS: [ConformanceAdapter; 11] = [
        ConformanceAdapter::Chat("openai"),
        ConformanceAdapter::Responses("openai"),
        ConformanceAdapter::Chat("grok"),
        ConformanceAdapter::Responses("grok"),
        ConformanceAdapter::Chat("openrouter"),
        ConformanceAdapter::Responses("openrouter"),
        ConformanceAdapter::Chat("meta"),
        ConformanceAdapter::Responses("meta"),
        ConformanceAdapter::Anthropic,
        ConformanceAdapter::Gemini,
        ConformanceAdapter::Mock,
    ];

    const NETWORK_ADAPTERS: [ConformanceAdapter; 10] = [
        ConformanceAdapter::Chat("openai"),
        ConformanceAdapter::Responses("openai"),
        ConformanceAdapter::Chat("grok"),
        ConformanceAdapter::Responses("grok"),
        ConformanceAdapter::Chat("openrouter"),
        ConformanceAdapter::Responses("openrouter"),
        ConformanceAdapter::Chat("meta"),
        ConformanceAdapter::Responses("meta"),
        ConformanceAdapter::Anthropic,
        ConformanceAdapter::Gemini,
    ];

    impl ConformanceAdapter {
        fn label(self) -> &'static str {
            match self {
                Self::Chat(provider) => provider,
                Self::Responses("openai") => "openai-responses",
                Self::Responses("grok") => "grok-responses",
                Self::Responses("openrouter") => "openrouter-responses",
                Self::Responses("meta") => "meta-responses",
                Self::Responses(_) => "compatible-responses",
                Self::Anthropic => "anthropic",
                Self::Gemini => "gemini",
                Self::Mock => "mock",
            }
        }

        fn provider(self) -> &'static str {
            match self {
                Self::Chat(provider) => provider,
                Self::Responses(provider) => provider,
                Self::Anthropic => "anthropic",
                Self::Gemini => "google",
                Self::Mock => "mock",
            }
        }

        fn transport(self) -> &'static str {
            match self {
                Self::Chat(_) => "chat_completions",
                Self::Responses(_) => "responses",
                Self::Anthropic => "anthropic_messages",
                Self::Gemini => "gemini_generate_content",
                Self::Mock => "mock",
            }
        }

        fn registry_transport(self) -> ProviderTransport {
            match self {
                Self::Chat(_) => ProviderTransport::ChatCompletions,
                Self::Responses(_) => ProviderTransport::Responses,
                Self::Anthropic => ProviderTransport::AnthropicMessages,
                Self::Gemini => ProviderTransport::GeminiGenerateContent,
                Self::Mock => ProviderTransport::Local,
            }
        }

        fn expected_path(self, stream: bool) -> &'static str {
            match (self, stream) {
                (Self::Chat(_), _) => "/chat/completions",
                (Self::Responses(_), _) => "/v1/responses",
                (Self::Anthropic, _) => "/v1/messages",
                (Self::Gemini, false) => "/v1beta/models/fixture-model:generateContent",
                (Self::Gemini, true) => {
                    "/v1beta/models/fixture-model:streamGenerateContent?alt=sse"
                }
                (Self::Mock, _) => "",
            }
        }

        fn documented_request_id(self) -> Option<(&'static str, &'static str)> {
            match self {
                Self::Chat("openai") | Self::Responses("openai") => {
                    Some(("x-request-id", "req_openai-matrix-123"))
                }
                Self::Anthropic => Some(("request-id", "req_anthropic-matrix.123")),
                _ => None,
            }
        }

        fn client(self, base_url: String) -> Arc<dyn LlmProvider> {
            let diagnostic_secrets = vec![
                ACTIVE_CONFORMANCE_KEY.to_string(),
                INACTIVE_CONFORMANCE_KEY.to_string(),
            ];
            match self {
                Self::Chat(provider) => {
                    Arc::new(OpenAiCompatClient::configured_with_diagnostic_secrets(
                        provider.to_string(),
                        SAFE_FIXTURE_MODEL.to_string(),
                        vec![ACTIVE_CONFORMANCE_KEY.to_string()],
                        diagnostic_secrets,
                        base_url,
                        None,
                    ))
                }
                Self::Responses(provider) => {
                    Arc::new(OpenAiResponsesClient::configured_with_diagnostic_secrets(
                        provider,
                        SAFE_FIXTURE_MODEL.to_string(),
                        vec![ACTIVE_CONFORMANCE_KEY.to_string()],
                        diagnostic_secrets,
                        format!("{base_url}/v1/responses"),
                        None,
                    ))
                }
                Self::Anthropic => Arc::new(
                    crate::llm::anthropic::AnthropicClient::configured_with_diagnostic_secrets(
                        SAFE_FIXTURE_MODEL.to_string(),
                        vec![ACTIVE_CONFORMANCE_KEY.to_string()],
                        diagnostic_secrets,
                        base_url,
                    )
                    .expect("Anthropic conformance endpoint"),
                ),
                Self::Gemini => Arc::new(
                    crate::llm::gemini::GeminiClient::configured_with_diagnostic_secrets(
                        SAFE_FIXTURE_MODEL.to_string(),
                        vec![ACTIVE_CONFORMANCE_KEY.to_string()],
                        diagnostic_secrets,
                        base_url,
                    )
                    .expect("Gemini conformance endpoint"),
                ),
                Self::Mock => Arc::new(MockLlmClient::new()),
            }
        }
    }

    async fn invoke_failure(client: &dyn LlmProvider, stream: bool, prompt: &str) -> LlmError {
        let messages = [LlmMessage::user(prompt)];
        if stream {
            match client.stream(LlmRequest::new(&messages, None)).await {
                Ok(stream) => stream.finish().await.expect_err("stream must fail closed"),
                Err(error) => error,
            }
        } else {
            client
                .complete(LlmRequest::new(&messages, None))
                .await
                .expect_err("completion must fail closed")
        }
    }

    fn remote_error_body() -> String {
        json!({
            "error": {
                "code": "fixture_failure",
                "message": format!(
                    "{REMOTE_PROMPT_SENTINEL} raw={ACTIVE_CONFORMANCE_KEY} inactive={INACTIVE_CONFORMANCE_KEY} percent=active%2Dsecret%2D42 json=\\u0072emote control=remote\\r\\nINJECT base64=YWN0aXZlLXNlY3JldC00Mg== inactive64=aW5hY3RpdmUtc2VjcmV0LTg0 {REMOTE_LABEL_SENTINEL}"
                ),
                "nested": {"private": REMOTE_LABEL_SENTINEL}
            }
        })
        .to_string()
    }

    fn assert_safe_error_surface(adapter: ConformanceAdapter, error: &LlmError) {
        let serialized = serde_json::to_string(error).expect("bounded typed error JSON");
        let surface = format!(
            "{error}\n{error:?}\n{}\n{serialized}",
            error.user_report(None)
        );
        for forbidden in [
            REMOTE_PROMPT_SENTINEL,
            ACTIVE_CONFORMANCE_KEY,
            INACTIVE_CONFORMANCE_KEY,
            "active%2Dsecret%2D42",
            "\\u0072emote",
            "INJECT",
            "YWN0aXZlLXNlY3JldC00Mg==",
            "aW5hY3RpdmUtc2VjcmV0LTg0",
            REMOTE_LABEL_SENTINEL,
        ] {
            assert!(
                !surface.contains(forbidden),
                "{} leaked {forbidden:?}: {surface}",
                adapter.label()
            );
        }
        assert!(surface.len() < 32 * 1024, "{} error bound", adapter.label());
        assert_eq!(
            error.provider,
            adapter.provider(),
            "{} provider",
            adapter.label()
        );
        assert_eq!(
            error.transport,
            adapter.transport(),
            "{} transport",
            adapter.label()
        );
        assert!(
            surface.contains(adapter.provider()),
            "{} provider context",
            adapter.label()
        );
        assert!(
            surface.contains(adapter.transport()),
            "{} transport context",
            adapter.label()
        );
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum TerminalCase {
        InBandError,
        MissingTerminal,
        Truncated,
        RefusedOrSafetyBlocked,
        UnknownTerminal,
        InconsistentToolTerminal,
        MalformedToolArguments,
    }

    const TERMINAL_CASES: [TerminalCase; 7] = [
        TerminalCase::InBandError,
        TerminalCase::MissingTerminal,
        TerminalCase::Truncated,
        TerminalCase::RefusedOrSafetyBlocked,
        TerminalCase::UnknownTerminal,
        TerminalCase::InconsistentToolTerminal,
        TerminalCase::MalformedToolArguments,
    ];

    struct WireFixture {
        complete: String,
        stream: String,
    }

    fn responses_terminal(output: Value, status: &str) -> Value {
        json!({"id": "resp_matrix", "status": status, "output": output})
    }

    fn terminal_fixture(adapter: ConformanceAdapter, case: TerminalCase) -> WireFixture {
        match adapter {
            ConformanceAdapter::Chat(provider) => {
                let (complete, stream) = match case {
                    TerminalCase::InBandError if provider == "openrouter" => (
                        json!({"choices": [{
                            "message": {"content": null},
                            "finish_reason": "error",
                            "error": {"message": REMOTE_PROMPT_SENTINEL}
                        }]}),
                        format!(
                            "data: {}\n\n",
                            json!({"choices": [{
                                "delta": {},
                                "finish_reason": "error",
                                "error": {"message": REMOTE_PROMPT_SENTINEL}
                            }]})
                        ),
                    ),
                    TerminalCase::InBandError => (
                        json!({"error": {"message": REMOTE_PROMPT_SENTINEL}}),
                        format!(
                            "data: {}\n\n",
                            json!({"error": {"message": REMOTE_PROMPT_SENTINEL}})
                        ),
                    ),
                    TerminalCase::MissingTerminal => (
                        json!({"choices": [{
                            "message": {"content": "partial"}, "finish_reason": null
                        }]}),
                        concat!(
                            "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},",
                            "\"finish_reason\":null}]}\n\n"
                        )
                        .to_string(),
                    ),
                    TerminalCase::Truncated => (
                        json!({"choices": [{
                            "message": {"content": "partial"}, "finish_reason": "length"
                        }]}),
                        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n"
                            .to_string(),
                    ),
                    TerminalCase::RefusedOrSafetyBlocked => (
                        json!({"choices": [{
                            "message": {"content": null, "refusal": REMOTE_PROMPT_SENTINEL},
                            "finish_reason": "stop"
                        }]}),
                        format!(
                            "data: {}\n\n",
                            json!({"choices": [{
                                "delta": {"refusal": REMOTE_PROMPT_SENTINEL},
                                "finish_reason": null
                            }]})
                        ),
                    ),
                    TerminalCase::UnknownTerminal => (
                        json!({"choices": [{
                            "message": {"content": "partial"},
                            "finish_reason": "REMOTE_FUTURE_TERMINAL"
                        }]}),
                        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"REMOTE_FUTURE_TERMINAL\"}]}\n\n"
                            .to_string(),
                    ),
                    TerminalCase::InconsistentToolTerminal => (
                        json!({"choices": [{
                            "message": {"tool_calls": [{
                                "id": "private_call_matrix", "type": "function",
                                "function": {"name": "write_file", "arguments": "{}"}
                            }]},
                            "finish_reason": "stop"
                        }]}),
                        concat!(
                            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
                            "\"id\":\"private_call_matrix\",\"function\":{\"name\":\"write_file\",",
                            "\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
                            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
                        )
                        .to_string(),
                    ),
                    TerminalCase::MalformedToolArguments => (
                        json!({"choices": [{
                            "message": {"tool_calls": [{
                                "id": "private_call_matrix", "type": "function",
                                "function": {"name": "write_file", "arguments": "{"}
                            }]},
                            "finish_reason": "tool_calls"
                        }]}),
                        concat!(
                            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
                            "\"id\":\"private_call_matrix\",\"function\":{\"name\":\"write_file\",",
                            "\"arguments\":\"{\"}}]},\"finish_reason\":null}]}\n\n",
                            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n"
                        )
                        .to_string(),
                    ),
                };
                WireFixture {
                    complete: complete.to_string(),
                    stream,
                }
            }
            ConformanceAdapter::Responses(_) => {
                let (complete, stream_event) = match case {
                    TerminalCase::InBandError => {
                        let response = json!({
                            "id": "resp_matrix", "status": "failed", "output": [],
                            "error": {"message": REMOTE_PROMPT_SENTINEL}
                        });
                        (
                            response.clone(),
                            json!({"type": "response.failed", "response": response}),
                        )
                    }
                    TerminalCase::MissingTerminal => (
                        json!({"id": "resp_matrix", "output": []}),
                        json!({"type": "response.output_text.delta", "delta": "partial"}),
                    ),
                    TerminalCase::Truncated => {
                        let response = json!({
                            "id": "resp_matrix", "status": "incomplete", "output": [],
                            "incomplete_details": {"reason": "max_output_tokens"}
                        });
                        (
                            response.clone(),
                            json!({"type": "response.incomplete", "response": response}),
                        )
                    }
                    TerminalCase::RefusedOrSafetyBlocked => {
                        let response = responses_terminal(
                            json!([{"type": "message", "status": "completed", "content": [{
                                "type": "refusal", "refusal": REMOTE_PROMPT_SENTINEL
                            }]}]),
                            "completed",
                        );
                        (
                            response.clone(),
                            json!({"type": "response.completed", "response": response}),
                        )
                    }
                    TerminalCase::UnknownTerminal => {
                        let response = responses_terminal(json!([]), "REMOTE_FUTURE_TERMINAL");
                        (
                            response.clone(),
                            json!({"type": "response.completed", "response": response}),
                        )
                    }
                    TerminalCase::InconsistentToolTerminal => {
                        let response = responses_terminal(
                            json!([
                                {"type": "function_call", "status": "completed",
                                 "call_id": "private_call_matrix", "name": "write_file",
                                 "arguments": "{}"},
                                {"type": "refusal", "refusal": REMOTE_PROMPT_SENTINEL}
                            ]),
                            "completed",
                        );
                        (
                            response.clone(),
                            json!({"type": "response.completed", "response": response}),
                        )
                    }
                    TerminalCase::MalformedToolArguments => {
                        let response = responses_terminal(
                            json!([{"type": "function_call", "status": "completed",
                                "call_id": "private_call_matrix", "name": "write_file",
                                "arguments": "{"}]),
                            "completed",
                        );
                        (
                            response.clone(),
                            json!({"type": "response.completed", "response": response}),
                        )
                    }
                };
                WireFixture {
                    complete: complete.to_string(),
                    stream: format!("data: {stream_event}\n\n"),
                }
            }
            ConformanceAdapter::Anthropic => {
                let (complete, stream) = match case {
                    TerminalCase::InBandError => (
                        json!({"type": "error", "error": {"message": REMOTE_PROMPT_SENTINEL}}),
                        format!(
                            "event: error\ndata: {}\n\n",
                            json!({"type": "error", "error": {"message": REMOTE_PROMPT_SENTINEL}})
                        ),
                    ),
                    TerminalCase::MissingTerminal => (
                        json!({"content": [{"type": "text", "text": "partial"}], "stop_reason": null}),
                        concat!(
                            "event: content_block_delta\n",
                            "data: {\"index\":0,\"delta\":{\"text\":\"partial\"}}\n\n"
                        )
                        .to_string(),
                    ),
                    TerminalCase::Truncated => (
                        json!({"content": [{"type": "text", "text": "partial"}], "stop_reason": "max_tokens"}),
                        concat!(
                            "event: message_delta\n",
                            "data: {\"delta\":{\"stop_reason\":\"max_tokens\"}}\n\n",
                            "event: message_stop\ndata: {}\n\n"
                        )
                        .to_string(),
                    ),
                    TerminalCase::RefusedOrSafetyBlocked => (
                        json!({"content": [], "stop_reason": "refusal"}),
                        concat!(
                            "event: message_delta\n",
                            "data: {\"delta\":{\"stop_reason\":\"refusal\"}}\n\n",
                            "event: message_stop\ndata: {}\n\n"
                        )
                        .to_string(),
                    ),
                    TerminalCase::UnknownTerminal => (
                        json!({"content": [], "stop_reason": "REMOTE_FUTURE_TERMINAL"}),
                        concat!(
                            "event: message_delta\n",
                            "data: {\"delta\":{\"stop_reason\":\"REMOTE_FUTURE_TERMINAL\"}}\n\n",
                            "event: message_stop\ndata: {}\n\n"
                        )
                        .to_string(),
                    ),
                    TerminalCase::InconsistentToolTerminal => (
                        json!({"content": [{"type": "tool_use", "id": "private_call_matrix",
                            "name": "write_file", "input": {}}], "stop_reason": "end_turn"}),
                        concat!(
                            "event: content_block_start\n",
                            "data: {\"index\":0,\"content_block\":{\"type\":\"tool_use\",",
                            "\"id\":\"private_call_matrix\",\"name\":\"write_file\"}}\n\n",
                            "event: content_block_delta\n",
                            "data: {\"index\":0,\"delta\":{\"partial_json\":\"{}\"}}\n\n",
                            "event: message_delta\n",
                            "data: {\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
                            "event: message_stop\ndata: {}\n\n"
                        )
                        .to_string(),
                    ),
                    TerminalCase::MalformedToolArguments => (
                        json!({"content": [{"type": "tool_use", "id": "private_call_matrix",
                            "name": "write_file", "input": "{"}], "stop_reason": "tool_use"}),
                        concat!(
                            "event: content_block_start\n",
                            "data: {\"index\":0,\"content_block\":{\"type\":\"tool_use\",",
                            "\"id\":\"private_call_matrix\",\"name\":\"write_file\"}}\n\n",
                            "event: content_block_delta\n",
                            "data: {\"index\":0,\"delta\":{\"partial_json\":\"{\"}}\n\n",
                            "event: message_delta\n",
                            "data: {\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
                            "event: message_stop\ndata: {}\n\n"
                        )
                        .to_string(),
                    ),
                };
                WireFixture {
                    complete: complete.to_string(),
                    stream,
                }
            }
            ConformanceAdapter::Gemini => {
                let (complete, stream) = match case {
                    TerminalCase::InBandError => {
                        let value = json!({"error": {"message": REMOTE_PROMPT_SENTINEL}});
                        (value.clone(), format!("data: {value}\n\n"))
                    }
                    TerminalCase::MissingTerminal => {
                        let value =
                            json!({"candidates": [{"content": {"parts": [{"text": "partial"}]}}]});
                        (value.clone(), format!("data: {value}\n\n"))
                    }
                    TerminalCase::Truncated => {
                        let value = json!({"candidates": [{"content": {"parts": [{"text": "partial"}]}, "finishReason": "MAX_TOKENS"}]});
                        (value.clone(), format!("data: {value}\n\n"))
                    }
                    TerminalCase::RefusedOrSafetyBlocked => {
                        let value = json!({"candidates": [{"finishReason": "SAFETY"}]});
                        (value.clone(), format!("data: {value}\n\n"))
                    }
                    TerminalCase::UnknownTerminal => {
                        let value =
                            json!({"candidates": [{"finishReason": "REMOTE_FUTURE_TERMINAL"}]});
                        (value.clone(), format!("data: {value}\n\n"))
                    }
                    TerminalCase::InconsistentToolTerminal => {
                        let value = json!({"candidates": [{"content": {"parts": [{
                            "functionCall": {"name": "write_file", "args": {}}
                        }]}, "finishReason": "SAFETY"}]});
                        (value.clone(), format!("data: {value}\n\n"))
                    }
                    TerminalCase::MalformedToolArguments => {
                        let value = json!({"candidates": [{"content": {"parts": [{
                            "functionCall": {"name": "write_file", "args": "{"}
                        }]}, "finishReason": "STOP"}]});
                        (value.clone(), format!("data: {value}\n\n"))
                    }
                };
                WireFixture {
                    complete: complete.to_string(),
                    stream,
                }
            }
            ConformanceAdapter::Mock => panic!("Mock has no remote terminal wire fixture"),
        }
    }

    fn successful_fixture(adapter: ConformanceAdapter) -> WireFixture {
        match adapter {
            ConformanceAdapter::Chat(_) => WireFixture {
                complete: json!({"choices": [{
                    "message": {"content": "matrix-ok"}, "finish_reason": "stop"
                }]})
                .to_string(),
                stream: concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"matrix-ok\"},",
                    "\"finish_reason\":null}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: [DONE]\n\n"
                )
                .to_string(),
            },
            ConformanceAdapter::Responses(_) => {
                let response = responses_terminal(
                    json!([{"type": "message", "status": "completed", "content": [{
                        "type": "output_text", "text": "matrix-ok"
                    }]}]),
                    "completed",
                );
                WireFixture {
                    complete: response.to_string(),
                    stream: format!(
                        "data: {}\n\n",
                        json!({"type": "response.completed", "response": response})
                    ),
                }
            }
            ConformanceAdapter::Anthropic => WireFixture {
                complete: json!({
                    "content": [{"type": "text", "text": "matrix-ok"}],
                    "stop_reason": "end_turn"
                })
                .to_string(),
                stream: concat!(
                    "event: content_block_delta\n",
                    "data: {\"index\":0,\"delta\":{\"text\":\"matrix-ok\"}}\n\n",
                    "event: message_delta\n",
                    "data: {\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
                    "event: message_stop\ndata: {}\n\n"
                )
                .to_string(),
            },
            ConformanceAdapter::Gemini => {
                let response = json!({"candidates": [{
                    "content": {"parts": [{"text": "matrix-ok"}]}, "finishReason": "STOP"
                }]});
                WireFixture {
                    complete: response.to_string(),
                    stream: format!("data: {response}\n\n"),
                }
            }
            ConformanceAdapter::Mock => panic!("Mock has no network fixture"),
        }
    }

    fn parallel_tool_fixture(adapter: ConformanceAdapter, stream: bool) -> ScriptedHttpResponse {
        let fixture = match adapter {
            ConformanceAdapter::Chat(_) => WireFixture {
                complete: json!({"choices": [{
                    "message": {"tool_calls": [
                        {"id": "call_alpha", "function": {"name": "probe_alpha", "arguments": "{\"slot\":1}"}},
                        {"id": "call_beta", "function": {"name": "probe_beta", "arguments": "{\"slot\":2}"}}
                    ]},
                    "finish_reason": "tool_calls"
                }]})
                .to_string(),
                stream: concat!(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[",
                    "{\"index\":0,\"id\":\"call_alpha\",\"function\":{\"name\":\"probe_alpha\",\"arguments\":\"{\\\"slot\\\":1}\"}},",
                    "{\"index\":1,\"id\":\"call_beta\",\"function\":{\"name\":\"probe_beta\",\"arguments\":\"{\\\"slot\\\":2}\"}}",
                    "]},\"finish_reason\":null}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
                    "data: [DONE]\n\n"
                )
                .to_string(),
            },
            ConformanceAdapter::Responses(_) => {
                let response = responses_terminal(
                    json!([
                        {"type": "function_call", "status": "completed", "call_id": "call_alpha", "name": "probe_alpha", "arguments": "{\"slot\":1}"},
                        {"type": "function_call", "status": "completed", "call_id": "call_beta", "name": "probe_beta", "arguments": "{\"slot\":2}"}
                    ]),
                    "completed",
                );
                WireFixture {
                    complete: response.to_string(),
                    stream: format!(
                        "data: {}\n\n",
                        json!({"type": "response.completed", "response": response})
                    ),
                }
            }
            ConformanceAdapter::Anthropic => WireFixture {
                complete: json!({
                    "content": [
                        {"type": "tool_use", "id": "toolu_alpha", "name": "probe_alpha", "input": {"slot": 1}},
                        {"type": "tool_use", "id": "toolu_beta", "name": "probe_beta", "input": {"slot": 2}}
                    ],
                    "stop_reason": "tool_use"
                })
                .to_string(),
                stream: concat!(
                    "event: content_block_start\n",
                    "data: {\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_alpha\",\"name\":\"probe_alpha\"}}\n\n",
                    "event: content_block_delta\n",
                    "data: {\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"slot\\\":1}\"}}\n\n",
                    "event: content_block_stop\ndata: {\"index\":0}\n\n",
                    "event: content_block_start\n",
                    "data: {\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_beta\",\"name\":\"probe_beta\"}}\n\n",
                    "event: content_block_delta\n",
                    "data: {\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"slot\\\":2}\"}}\n\n",
                    "event: content_block_stop\ndata: {\"index\":1}\n\n",
                    "event: message_delta\n",
                    "data: {\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
                    "event: message_stop\ndata: {}\n\n"
                )
                .to_string(),
            },
            ConformanceAdapter::Gemini => {
                let response = json!({"candidates": [{
                    "content": {"role": "model", "parts": [
                        {"functionCall": {"name": "probe_alpha", "args": {"slot": 1}}},
                        {"functionCall": {"name": "probe_beta", "args": {"slot": 2}}}
                    ]},
                    "finishReason": "STOP"
                }]});
                WireFixture {
                    complete: response.to_string(),
                    stream: format!("data: {response}\n\n"),
                }
            }
            ConformanceAdapter::Mock => panic!("Mock has no network fixture"),
        };
        ScriptedHttpResponse::new(
            "200 OK",
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            },
            if stream {
                fixture.stream
            } else {
                fixture.complete
            },
        )
    }

    struct ObservedTurn {
        result: Result<LlmResponse, LlmError>,
        public_events: Vec<LlmStreamEvent>,
    }

    async fn observe_turn(
        adapter: ConformanceAdapter,
        base_url: String,
        stream: bool,
    ) -> ObservedTurn {
        let client = adapter.client(base_url);
        let messages = [LlmMessage::user(REMOTE_PROMPT_SENTINEL)];
        let request = || {
            LlmRequest::new(&messages, None).with_scope(
                LlmRequestScope::new("conformance-session", "terminal-run")
                    .expect("conformance scope"),
            )
        };
        if !stream {
            return ObservedTurn {
                result: client.complete(request()).await,
                public_events: Vec::new(),
            };
        }
        let stream = match client.stream(request()).await {
            Ok(stream) => stream,
            Err(error) => {
                return ObservedTurn {
                    result: Err(error),
                    public_events: Vec::new(),
                };
            }
        };
        // Provider deltas are private until terminal validation. This conformance observer
        // deliberately has no pre-terminal event path; application projections are tested at
        // the agent boundary from the validated response.
        ObservedTurn {
            result: stream.finish().await,
            public_events: Vec::new(),
        }
    }

    fn assert_no_tool_authority(
        adapter: ConformanceAdapter,
        observed: &ObservedTurn,
        refusal_is_allowed: bool,
    ) {
        for event in &observed.public_events {
            let projected = format!("{event:?}");
            assert!(
                !projected.contains("private_call_matrix"),
                "{} native ID",
                adapter.label()
            );
            assert!(
                !projected.contains(REMOTE_PROMPT_SENTINEL),
                "{} remote error",
                adapter.label()
            );
        }
        match &observed.result {
            Ok(response) => {
                assert!(
                    refusal_is_allowed,
                    "{} non-refusal unsafe fixture returned a successful turn",
                    adapter.label()
                );
                assert_eq!(
                    response.terminal_status,
                    LlmTerminalStatus::Refused,
                    "{} unsafe fixture became completed",
                    adapter.label()
                );
                assert!(
                    response.tool_calls.is_none(),
                    "{} refusal tools",
                    adapter.label()
                );
                assert!(
                    response.continuation.is_none(),
                    "{} refusal continuation",
                    adapter.label()
                );
                assert!(
                    response.content.as_deref().is_none_or(|content| {
                        !content.contains(REMOTE_PROMPT_SENTINEL)
                            && !content.contains("private_call_matrix")
                    }),
                    "{} unsafe response content",
                    adapter.label()
                );
            }
            Err(error) => assert_safe_error_surface(adapter, error),
        }
    }

    fn openai_compat(provider: &str, base_url: String) -> OpenAiCompatClient {
        OpenAiCompatClient::configured(
            provider.to_string(),
            "fixture-model".to_string(),
            vec!["fixture-key".to_string()],
            base_url,
            None,
        )
    }

    fn request_id_fixture(headers: Vec<(String, String)>) -> String {
        serve_once_with_headers(
            "401 Unauthorized",
            "application/json",
            json!({"error": {"code": "invalid_api_key", "message": "private"}}).to_string(),
            headers,
        )
        .0
    }

    fn response_header(name: &str, value: impl Into<String>) -> Vec<(String, String)> {
        vec![(name.to_string(), value.into())]
    }

    fn json_response(status: &str, body: Value) -> ScriptedHttpResponse {
        ScriptedHttpResponse::new(status, "application/json", body.to_string())
    }

    fn retry_then_json(first: Value, final_response: Value) -> Vec<ScriptedHttpResponse> {
        vec![
            json_response("200 OK", first),
            json_response("503 Service Unavailable", json!({"error": {}})),
            json_response("503 Service Unavailable", json!({"error": {}})),
            json_response("200 OK", final_response),
        ]
    }

    fn request_semantics(
        request: &str,
        credential_header: &str,
    ) -> (String, BTreeMap<String, String>, String, String) {
        let (headers, body) = request
            .split_once("\r\n\r\n")
            .expect("captured HTTP request framing");
        let mut lines = headers.lines();
        let request_line = lines.next().expect("request line").to_string();
        let mut semantic_headers = BTreeMap::new();
        let mut credential = None;
        for line in lines {
            let (name, value) = line.split_once(':').expect("captured request header");
            let name = name.to_ascii_lowercase();
            let value = value.trim().to_string();
            if name == credential_header {
                credential = Some(value);
            } else {
                assert!(semantic_headers.insert(name, value).is_none());
            }
        }
        (
            request_line,
            semantic_headers,
            body.to_string(),
            credential.expect("provider credential header"),
        )
    }

    fn assert_three_retry_requests_are_semantically_identical(
        requests: &Receiver<String>,
        tool_result_marker: &str,
        credential_header: &str,
        expected_credential: &str,
    ) -> Value {
        let _initial = requests
            .recv_timeout(Duration::from_secs(5))
            .expect("initial tool request");
        let retried = (0..3)
            .map(|_| {
                requests
                    .recv_timeout(Duration::from_secs(5))
                    .expect("retried continuation request")
            })
            .collect::<Vec<_>>();
        let expected = request_semantics(&retried[0], credential_header);
        assert!(expected.2.contains(tool_result_marker));
        assert_eq!(expected.3, expected_credential);
        for request in &retried[1..] {
            assert_eq!(request_semantics(request, credential_header), expected);
        }
        serde_json::from_str(&expected.2).expect("retried request JSON body")
    }

    async fn chat_http_failure(
        provider: &str,
        headers: Vec<(String, String)>,
        stream: bool,
    ) -> crate::llm::LlmError {
        let client = openai_compat(provider, request_id_fixture(headers));
        let messages = [LlmMessage::user("inspect")];
        if stream {
            client
                .stream(LlmRequest::new(&messages, None))
                .await
                .expect_err("Chat stream HTTP failure")
        } else {
            client
                .complete(LlmRequest::new(&messages, None))
                .await
                .expect_err("Chat completion HTTP failure")
        }
    }

    async fn responses_http_failure(
        provider: &str,
        headers: Vec<(String, String)>,
        stream: bool,
    ) -> crate::llm::LlmError {
        let client = OpenAiResponsesClient::configured(
            provider.to_string(),
            "fixture-model".to_string(),
            vec!["fixture-key".to_string()],
            request_id_fixture(headers),
            None,
        );
        let messages = [LlmMessage::user("inspect")];
        if stream {
            client
                .stream(LlmRequest::new(&messages, None))
                .await
                .expect_err("Responses stream HTTP failure")
        } else {
            client
                .complete(LlmRequest::new(&messages, None))
                .await
                .expect_err("Responses completion HTTP failure")
        }
    }

    async fn anthropic_http_failure(
        headers: Vec<(String, String)>,
        stream: bool,
    ) -> crate::llm::LlmError {
        let client = crate::llm::anthropic::AnthropicClient::with_base_url(
            "claude-fixture".to_string(),
            vec!["fixture-key".to_string()],
            request_id_fixture(headers),
        )
        .expect("Anthropic fixture endpoint");
        let messages = [LlmMessage::user("inspect")];
        if stream {
            client
                .stream(LlmRequest::new(&messages, None))
                .await
                .expect_err("Anthropic stream HTTP failure")
        } else {
            client
                .complete(LlmRequest::new(&messages, None))
                .await
                .expect_err("Anthropic completion HTTP failure")
        }
    }

    async fn gemini_http_failure(
        headers: Vec<(String, String)>,
        stream: bool,
    ) -> crate::llm::LlmError {
        let client = crate::llm::gemini::GeminiClient::with_base_url(
            "gemini-fixture".to_string(),
            vec!["fixture-key".to_string()],
            request_id_fixture(headers),
        )
        .expect("Gemini fixture endpoint");
        let messages = [LlmMessage::user("inspect")];
        if stream {
            client
                .stream(LlmRequest::new(&messages, None))
                .await
                .expect_err("Gemini stream HTTP failure")
        } else {
            client
                .complete(LlmRequest::new(&messages, None))
                .await
                .expect_err("Gemini completion HTTP failure")
        }
    }

    #[test]
    fn generation_options_are_shared_across_the_contract() {
        let options = GenerationOptions::provider_default()
            .with_temperature(1.0)
            .expect("valid");
        assert_eq!(options.temperature(), Some(1.0));
        assert_eq!(
            options.resolved_reasoning(Some(ReasoningEffort::Medium)),
            Some(ReasoningEffort::Medium)
        );
        assert_eq!(
            options
                .with_reasoning(ReasoningOption::Disabled)
                .resolved_reasoning(Some(ReasoningEffort::Medium)),
            Some(ReasoningEffort::None)
        );
    }

    #[test]
    fn every_openai_compatible_adapter_omits_provider_default_temperature() {
        let messages = [LlmMessage::user("inspect")];
        let tools = [ToolDefinition::function("read_file")];
        for provider in ["openai", "grok", "openrouter", "meta"] {
            let client = openai_compat(provider, "https://example.test/v1".to_string());
            let body = client
                .request_body(LlmRequest::new(&messages, Some(&tools)), false)
                .expect("valid Chat request");
            assert!(
                body.get("temperature").is_none(),
                "{provider} leaked provider-default temperature"
            );
            assert_eq!(body["tools"][0]["function"]["name"], "read_file");
        }
    }

    #[test]
    fn every_openai_compatible_adapter_serializes_explicit_finite_temperature() {
        let messages = [LlmMessage::user("inspect")];
        for provider in ["openai", "grok", "openrouter", "meta"] {
            let client = openai_compat(provider, "https://example.test/v1".to_string());
            let request = LlmRequest::new(&messages, None)
                .with_temperature(0.4)
                .expect("valid temperature");
            let body = client
                .request_body(request, false)
                .expect("valid Chat request");
            assert_eq!(body["temperature"], 0.4, "{provider}");
        }
    }

    #[test]
    fn anthropic_and_gemini_omit_provider_default_temperature_and_serialize_explicit_values() {
        let messages = [LlmMessage::user("inspect")];
        let anthropic = crate::llm::anthropic::AnthropicClient::with_base_url(
            "claude".to_string(),
            vec!["key".to_string()],
            "https://api.anthropic.com/v1/messages",
        )
        .expect("anthropic");
        let default_body = anthropic
            .request_body(LlmRequest::new(&messages, None), false)
            .expect("anthropic default");
        assert!(default_body.get("temperature").is_none());
        let explicit = anthropic
            .request_body(
                LlmRequest::new(&messages, None)
                    .with_temperature(0.2)
                    .expect("temp"),
                false,
            )
            .expect("anthropic explicit");
        assert_eq!(explicit["temperature"], 0.2);

        let gemini = crate::llm::gemini::GeminiClient::with_base_url(
            "gemini-test".to_string(),
            vec!["key".to_string()],
            "https://generativelanguage.googleapis.com/v1beta",
        )
        .expect("gemini");
        let default_body = gemini
            .request_body(LlmRequest::new(&messages, None))
            .expect("gemini default");
        assert!(default_body.get("generationConfig").is_none());
        let explicit = gemini
            .request_body(
                LlmRequest::new(&messages, None)
                    .with_temperature(1.5)
                    .expect("temp"),
            )
            .expect("gemini explicit");
        assert_eq!(explicit["generationConfig"]["temperature"], 1.5);
    }

    #[test]
    fn responses_rejects_explicit_temperature_before_io() {
        let client = OpenAiResponsesClient::configured(
            "openai".to_string(),
            "fixture-model".to_string(),
            vec!["fixture-key".to_string()],
            "http://127.0.0.1:9".to_string(),
            None,
        );
        let messages = [LlmMessage::user("inspect")];
        let request = LlmRequest::new(&messages, None)
            .with_temperature(0.75)
            .expect("valid temperature");
        let error = client
            .request_body(request, false)
            .expect_err("Responses must not discard explicit temperature");
        assert!(
            error.contains("do not support explicit temperature"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn mock_rejects_unsupported_reasoning_before_io() {
        let messages = [LlmMessage::user("inspect")];
        let request = || {
            LlmRequest::new(&messages, None).with_reasoning_effort(Some(ReasoningEffort::Medium))
        };

        let mock = MockLlmClient::new();
        let complete = mock.complete(request()).await.expect_err("mock complete");
        let stream = mock.stream(request()).await.expect_err("mock stream");
        assert_eq!(complete.class, LlmErrorClass::UnsupportedRequest);
        assert_eq!(stream.class, complete.class);
        assert_eq!(complete.phase, LlmErrorPhase::Request);
        assert_eq!(stream.phase, complete.phase);
        assert!(complete.contains("do not support reasoning_effort"));
    }

    async fn assert_complete_and_stream_errors_match(
        label: &str,
        complete: crate::llm::LlmError,
        stream: crate::llm::LlmError,
        class: LlmErrorClass,
        phase: LlmErrorPhase,
    ) {
        assert_eq!(complete.class, class, "{label} complete class");
        assert_eq!(stream.class, complete.class, "{label} stream class");
        assert_eq!(complete.phase, phase, "{label} complete phase");
        assert_eq!(stream.phase, complete.phase, "{label} stream phase");
        assert_eq!(complete.provider, stream.provider, "{label} provider");
        assert!(!complete.user_report(None).contains("private"), "{label}");
        assert!(!stream.user_report(None).contains("private"), "{label}");
    }

    fn expected_http_class(status: u16) -> LlmErrorClass {
        match status {
            400 => LlmErrorClass::ProviderRejected,
            401 | 403 => LlmErrorClass::Authentication,
            429 => LlmErrorClass::RateLimited,
            408 | 425 | 500 | 502 | 503 | 504 | 529 => LlmErrorClass::ProviderUnavailable,
            _ => panic!("unhandled conformance status {status}"),
        }
    }

    fn scripted_http_failures(
        adapter: ConformanceAdapter,
        status: u16,
    ) -> Vec<ScriptedHttpResponse> {
        let status_line = match status {
            400 => "400 Bad Request",
            401 => "401 Unauthorized",
            403 => "403 Forbidden",
            408 => "408 Request Timeout",
            425 => "425 Too Early",
            429 => "429 Too Many Requests",
            500 => "500 Internal Server Error",
            502 => "502 Bad Gateway",
            503 => "503 Service Unavailable",
            504 => "504 Gateway Timeout",
            529 => "529 Overloaded",
            _ => panic!("unhandled conformance status {status}"),
        };
        let attempts = if matches!(status, 408 | 425 | 429 | 500 | 502 | 503 | 504 | 529) {
            3
        } else {
            1
        };
        (0..attempts)
            .map(|_| {
                let response =
                    ScriptedHttpResponse::new(status_line, "application/json", remote_error_body())
                        // Every response carries both candidates. The adapter must retain only
                        // the header documented for its own provider boundary.
                        .with_header("x-request-id", "req_openai-matrix-123")
                        .with_header("request-id", "req_anthropic-matrix.123");
                if adapter == ConformanceAdapter::Anthropic && status == 529 {
                    response.with_header("Retry-After", "1")
                } else {
                    response
                }
            })
            .collect()
    }

    #[tokio::test]
    async fn network_conformance_matrix_keeps_http_errors_typed_bounded_and_provider_owned() {
        for adapter in NETWORK_ADAPTERS {
            let mut statuses = vec![400, 401, 403];
            statuses.extend_from_slice(
                crate::llm::registry::retry_capabilities(
                    adapter.provider(),
                    adapter.registry_transport(),
                )
                .retryable_http_statuses,
            );
            statuses.sort_unstable();
            statuses.dedup();
            for status in statuses {
                let mut paired = Vec::new();
                for stream in [false, true] {
                    let scripted = scripted_http_failures(adapter, status);
                    let expected_attempts = scripted.len();
                    let (base_url, requests) = serve_sequence(scripted);
                    let error = invoke_failure(
                        adapter.client(base_url).as_ref(),
                        stream,
                        REMOTE_PROMPT_SENTINEL,
                    )
                    .await;
                    assert_eq!(
                        error.class,
                        expected_http_class(status),
                        "{} {status} class",
                        adapter.label()
                    );
                    assert_eq!(
                        error.phase,
                        LlmErrorPhase::HttpResponse,
                        "{} {status} phase",
                        adapter.label()
                    );
                    assert_eq!(
                        error.http_status,
                        Some(status),
                        "{} status",
                        adapter.label()
                    );
                    assert_eq!(
                        error.attempts.attempts(),
                        expected_attempts as u8,
                        "{} {status} attempts",
                        adapter.label()
                    );
                    assert_eq!(
                        error.request_id.as_deref(),
                        adapter.documented_request_id().map(|(_, value)| value),
                        "{} request-ID ownership",
                        adapter.label()
                    );
                    assert_safe_error_surface(adapter, &error);

                    for _ in 0..expected_attempts {
                        let request = requests
                            .recv_timeout(Duration::from_secs(5))
                            .expect("captured conformance request");
                        let request_line = request.lines().next().expect("request line");
                        assert!(
                            request_line.contains(adapter.expected_path(stream)),
                            "{} expected path in {request_line}",
                            adapter.label()
                        );
                    }
                    paired.push(error);
                }
                assert_eq!(
                    paired[0].class,
                    paired[1].class,
                    "{} {status} complete/stream class",
                    adapter.label()
                );
                assert_eq!(
                    paired[0].phase,
                    paired[1].phase,
                    "{} {status} complete/stream phase",
                    adapter.label()
                );
            }
        }
    }

    #[tokio::test]
    async fn terminal_authority_matrix_rejects_incomplete_unsafe_and_malformed_turns() {
        for adapter in NETWORK_ADAPTERS {
            for case in TERMINAL_CASES {
                let fixture = terminal_fixture(adapter, case);
                let (complete_url, complete_requests) =
                    serve_once("200 OK", "application/json", fixture.complete);
                let complete = observe_turn(adapter, complete_url, false).await;
                complete_requests
                    .recv_timeout(Duration::from_secs(5))
                    .expect("complete terminal request");
                let refusal_is_allowed = case == TerminalCase::RefusedOrSafetyBlocked;
                assert_no_tool_authority(adapter, &complete, refusal_is_allowed);

                let (stream_url, stream_requests) =
                    serve_once("200 OK", "text/event-stream", fixture.stream);
                let streamed = observe_turn(adapter, stream_url, true).await;
                stream_requests
                    .recv_timeout(Duration::from_secs(5))
                    .expect("stream terminal request");
                assert_no_tool_authority(adapter, &streamed, refusal_is_allowed);

                match (&complete.result, &streamed.result) {
                    (Err(complete_error), Err(stream_error)) => {
                        assert_eq!(
                            complete_error.class,
                            stream_error.class,
                            "{} {case:?} class parity",
                            adapter.label()
                        );
                        assert_eq!(
                            complete_error.phase,
                            LlmErrorPhase::TerminalValidation,
                            "{} {case:?} complete phase",
                            adapter.label()
                        );
                        assert_eq!(
                            stream_error.phase,
                            LlmErrorPhase::Stream,
                            "{} {case:?} stream phase",
                            adapter.label()
                        );
                    }
                    (Ok(complete_response), Ok(stream_response)) => {
                        assert_eq!(
                            complete_response.terminal_status,
                            stream_response.terminal_status,
                            "{} {case:?} terminal parity",
                            adapter.label()
                        );
                        assert_eq!(
                            complete_response.finish_reason,
                            stream_response.finish_reason,
                            "{} {case:?} finish parity",
                            adapter.label()
                        );
                    }
                    _ => panic!(
                        "{} {case:?} complete/stream authority mismatch",
                        adapter.label()
                    ),
                }
            }
        }
    }

    #[test]
    fn conformance_matrix_exactly_covers_every_registered_provider_transport() {
        let registered = PROVIDERS
            .iter()
            .flat_map(|provider| {
                provider
                    .transports()
                    .map(move |transport| (provider.id, transport))
            })
            .collect::<std::collections::BTreeSet<_>>();
        let covered = ALL_ADAPTERS
            .iter()
            .map(|adapter| (adapter.provider(), adapter.registry_transport()))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(covered, registered);
        assert_eq!(covered.len(), ALL_ADAPTERS.len());
    }

    #[tokio::test]
    async fn every_registered_transport_completes_the_same_safe_text_turn() {
        for adapter in ALL_ADAPTERS {
            if adapter == ConformanceAdapter::Mock {
                let client = adapter.client(String::new());
                let messages = [LlmMessage::user("safe terminal")];
                let complete = client
                    .complete(LlmRequest::new(&messages, None))
                    .await
                    .expect("Mock completion");
                let stream = client
                    .stream(LlmRequest::new(&messages, None))
                    .await
                    .expect("Mock stream");
                let streamed = stream.finish().await.expect("Mock stream completion");
                assert_eq!(streamed.content, complete.content);
                assert_eq!(streamed.finish_reason, complete.finish_reason);
                assert_eq!(streamed.attempts.attempts(), 0);
                continue;
            }

            let fixture = successful_fixture(adapter);
            let mut responses = Vec::new();
            for (stream, content_type, body) in [
                (false, "application/json", fixture.complete),
                (true, "text/event-stream", fixture.stream),
            ] {
                let (base_url, requests) = serve_once("200 OK", content_type, body);
                let observed = observe_turn(adapter, base_url, stream).await;
                let request = requests
                    .recv_timeout(Duration::from_secs(5))
                    .expect("captured successful request");
                assert!(
                    request
                        .lines()
                        .next()
                        .expect("request line")
                        .contains(adapter.expected_path(stream)),
                    "{} successful path",
                    adapter.label()
                );
                assert!(observed
                    .public_events
                    .iter()
                    .all(|event| { !format!("{event:?}").contains("private_call_matrix") }));
                let response = observed.result.expect("safe terminal completion");
                assert!(
                    observed.public_events.is_empty(),
                    "{} provider deltas must stay private even for a valid stream",
                    adapter.label()
                );
                responses.push(response);
            }
            assert_eq!(responses[0].terminal_status, LlmTerminalStatus::Completed);
            assert_eq!(responses[1].terminal_status, responses[0].terminal_status);
            assert_eq!(responses[0].content.as_deref(), Some("matrix-ok"));
            assert_eq!(responses[1].content, responses[0].content);
            assert_eq!(responses[1].finish_reason, responses[0].finish_reason);
            assert!(responses.iter().all(|response| {
                response.tool_calls.is_none()
                    && response.continuation.is_none()
                    && response.attempts.attempts() == 1
            }));
        }
    }

    #[tokio::test]
    async fn every_network_transport_correlates_parallel_results_across_complete_and_stream() {
        for adapter in NETWORK_ADAPTERS {
            for stream in [false, true] {
                let final_fixture = successful_fixture(adapter);
                let (base_url, requests) = serve_sequence(vec![
                    parallel_tool_fixture(adapter, stream),
                    ScriptedHttpResponse::new(
                        "200 OK",
                        if stream {
                            "text/event-stream"
                        } else {
                            "application/json"
                        },
                        if stream {
                            final_fixture.stream
                        } else {
                            final_fixture.complete
                        },
                    ),
                ]);
                let client = adapter.client(base_url);
                let messages = [LlmMessage::user("correlate parallel tools")];
                let tools = [
                    ToolDefinition::function("probe_alpha"),
                    ToolDefinition::function("probe_beta"),
                ];
                let scope = LlmRequestScope::new("continuation-matrix", "parallel-run")
                    .expect("continuation scope");
                let first_request =
                    LlmRequest::new(&messages, Some(&tools)).with_scope(scope.clone());
                let first = if stream {
                    client
                        .stream(first_request)
                        .await
                        .expect("first provider stream")
                        .finish()
                        .await
                        .expect("first provider stream completion")
                } else {
                    client
                        .complete(first_request)
                        .await
                        .expect("first provider completion")
                };
                assert_eq!(first.finish_reason, LlmFinishReason::ToolCalls);
                let calls = first.tool_calls.as_ref().expect("parallel tool calls");
                assert_eq!(calls.len(), 2, "{} {stream}", adapter.label());
                assert_eq!(calls[0].name, "probe_alpha");
                assert_eq!(calls[1].name, "probe_beta");
                assert_ne!(calls[0].invocation_id, calls[1].invocation_id);
                let alpha_id = calls[0].invocation_id;
                let beta_id = calls[1].invocation_id;

                let mut continuation = first.continuation.expect("provider continuation");
                continuation
                    .record_tool_result(
                        ToolResult::error(alpha_id, json!({"correlation": "alpha"})).unwrap(),
                    )
                    .expect("correlate alpha");
                continuation
                    .record_tool_result(
                        ToolResult::success(beta_id, json!({"correlation": "beta"})).unwrap(),
                    )
                    .expect("correlate beta");
                let second_request = LlmRequest::new(&messages, Some(&tools))
                    .with_scope(scope)
                    .with_continuation(Some(continuation));
                let second = if stream {
                    client
                        .stream(second_request)
                        .await
                        .expect("second provider stream")
                        .finish()
                        .await
                        .expect("second provider stream completion")
                } else {
                    client
                        .complete(second_request)
                        .await
                        .expect("second provider completion")
                };
                assert_eq!(second.finish_reason, LlmFinishReason::Complete);
                assert_eq!(second.content.as_deref(), Some("matrix-ok"));

                let first_wire = requests
                    .recv_timeout(Duration::from_secs(5))
                    .expect("first captured continuation request");
                let second_wire = requests
                    .recv_timeout(Duration::from_secs(5))
                    .expect("second captured continuation request");
                assert!(
                    first_wire
                        .lines()
                        .next()
                        .expect("first request line")
                        .contains(adapter.expected_path(stream)),
                    "{} {stream} first path",
                    adapter.label()
                );
                let second_body = second_wire
                    .split_once("\r\n\r\n")
                    .expect("second HTTP body")
                    .1;
                for marker in ["probe_alpha", "probe_beta", "alpha", "beta"] {
                    assert!(
                        second_body.contains(marker),
                        "{} {stream} missing correlated {marker}: {second_body}",
                        adapter.label()
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn every_registered_transport_has_complete_and_stream_terminal_authority() {
        for adapter in ALL_ADAPTERS {
            if adapter == ConformanceAdapter::Mock {
                let client = adapter.client(String::new());
                let messages = [LlmMessage::user("mock terminal conformance")];
                let complete = client
                    .complete(LlmRequest::new(&messages, None))
                    .await
                    .expect("Mock completion");
                let streamed = client
                    .stream(LlmRequest::new(&messages, None))
                    .await
                    .expect("Mock stream")
                    .finish()
                    .await
                    .expect("Mock stream terminal");
                assert_eq!(complete.terminal_status, LlmTerminalStatus::Completed);
                assert_eq!(streamed.terminal_status, complete.terminal_status);
                assert_eq!(streamed.finish_reason, complete.finish_reason);
                assert!(complete.continuation.is_none());
                assert!(streamed.continuation.is_none());
                continue;
            }

            let fixture = terminal_fixture(adapter, TerminalCase::MissingTerminal);
            for (stream, content_type, body) in [
                (false, "application/json", fixture.complete.clone()),
                (true, "text/event-stream", fixture.stream.clone()),
            ] {
                let (base_url, _) = serve_once("200 OK", content_type, body);
                let observed = observe_turn(adapter, base_url, stream).await;
                assert_no_tool_authority(adapter, &observed, false);
                assert!(
                    observed.result.is_err(),
                    "{} missing terminal",
                    adapter.label()
                );
            }
        }
    }

    #[tokio::test]
    async fn every_network_transport_rejects_complete_stream_and_event_byte_overflow() {
        for adapter in NETWORK_ADAPTERS {
            for (stream, declared_length) in [
                (false, crate::llm::MAX_LLM_COMPLETE_RESPONSE_BYTES + 1),
                (true, crate::llm::MAX_LLM_STREAM_BYTES + 1),
            ] {
                let (base_url, _) = serve_once_with_declared_length(
                    "200 OK",
                    if stream {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                    "",
                    Some(declared_length),
                );
                let error = invoke_failure(
                    adapter.client(base_url).as_ref(),
                    stream,
                    REMOTE_PROMPT_SENTINEL,
                )
                .await;
                assert_eq!(
                    error.class,
                    LlmErrorClass::Protocol,
                    "{} byte class",
                    adapter.label()
                );
                assert_eq!(
                    error.phase,
                    LlmErrorPhase::TerminalValidation,
                    "{} byte phase",
                    adapter.label()
                );
                assert_safe_error_surface(adapter, &error);
            }

            let oversized_event = format!(
                "data: {}\n\n",
                "x".repeat(crate::llm::MAX_LLM_STREAM_EVENT_BYTES + 1)
            );
            let (base_url, _) = serve_once("200 OK", "text/event-stream", oversized_event);
            let error = invoke_failure(
                adapter.client(base_url).as_ref(),
                true,
                REMOTE_PROMPT_SENTINEL,
            )
            .await;
            assert_eq!(
                error.class,
                LlmErrorClass::Protocol,
                "{} event class",
                adapter.label()
            );
            assert_eq!(
                error.phase,
                LlmErrorPhase::Stream,
                "{} event phase",
                adapter.label()
            );
            assert_safe_error_surface(adapter, &error);
        }
    }

    fn first_public_stream_event(adapter: ConformanceAdapter) -> String {
        match adapter {
            ConformanceAdapter::Chat(_) => concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"first\"},",
                "\"finish_reason\":null}]}\n\n"
            )
            .to_string(),
            ConformanceAdapter::Responses(_) => concat!(
                "data: {\"type\":\"response.output_text.delta\",",
                "\"delta\":\"first\"}\n\n"
            )
            .to_string(),
            ConformanceAdapter::Anthropic => concat!(
                "event: content_block_delta\n",
                "data: {\"index\":0,\"delta\":{\"text\":\"first\"}}\n\n"
            )
            .to_string(),
            ConformanceAdapter::Gemini => concat!(
                "data: {\"candidates\":[{\"content\":{\"parts\":[",
                "{\"text\":\"first\"}]}}]}\n\n"
            )
            .to_string(),
            ConformanceAdapter::Mock => panic!("Mock does not own a network response"),
        }
    }

    fn output_item_overflow_fixture(adapter: ConformanceAdapter) -> WireFixture {
        let count = crate::llm::MAX_LLM_RESPONSE_ITEMS + 1;
        match adapter {
            ConformanceAdapter::Chat(_) => {
                let calls = (0..count)
                    .map(|index| {
                        json!({
                            "index": index,
                            "id": format!("call_matrix_{index}"),
                            "type": "function",
                            "function": {"name": "probe", "arguments": "{}"}
                        })
                    })
                    .collect::<Vec<_>>();
                WireFixture {
                    complete: json!({"choices": [{
                        "message": {"tool_calls": calls}, "finish_reason": "tool_calls"
                    }]})
                    .to_string(),
                    stream: format!(
                        "data: {}\n\ndata: {}\n\n",
                        json!({"choices": [{"delta": {"tool_calls": calls}, "finish_reason": null}]}),
                        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]})
                    ),
                }
            }
            ConformanceAdapter::Responses(_) => {
                let output = (0..count)
                    .map(|index| json!({"type": "reasoning", "id": format!("rs_{index}")}))
                    .collect::<Vec<_>>();
                let response = responses_terminal(Value::Array(output), "completed");
                WireFixture {
                    complete: response.to_string(),
                    stream: format!(
                        "data: {}\n\n",
                        json!({"type": "response.completed", "response": response})
                    ),
                }
            }
            ConformanceAdapter::Anthropic => {
                let content = (0..count)
                    .map(|_| json!({"type": "text", "text": "x"}))
                    .collect::<Vec<_>>();
                let stream = (0..count)
                    .map(|index| {
                        format!(
                            "event: content_block_delta\ndata: {}\n\n",
                            json!({"index": index, "delta": {"text": "x"}})
                        )
                    })
                    .chain([
                        "event: message_delta\ndata: {\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n"
                            .to_string(),
                        "event: message_stop\ndata: {}\n\n".to_string(),
                    ])
                    .collect::<String>();
                WireFixture {
                    complete: json!({"content": content, "stop_reason": "end_turn"}).to_string(),
                    stream,
                }
            }
            ConformanceAdapter::Gemini => {
                let parts = (0..count).map(|_| json!({"text": "x"})).collect::<Vec<_>>();
                let response = json!({"candidates": [{
                    "content": {"parts": parts}, "finishReason": "STOP"
                }]});
                WireFixture {
                    complete: response.to_string(),
                    stream: format!("data: {response}\n\n"),
                }
            }
            ConformanceAdapter::Mock => panic!("Mock has no remote output items"),
        }
    }

    #[tokio::test]
    async fn every_network_transport_bounds_complete_and_stream_output_items() {
        for adapter in NETWORK_ADAPTERS {
            let fixture = output_item_overflow_fixture(adapter);
            for (stream, content_type, body) in [
                (false, "application/json", fixture.complete),
                (true, "text/event-stream", fixture.stream),
            ] {
                let (base_url, _) = serve_once("200 OK", content_type, body);
                let observed = observe_turn(adapter, base_url, stream).await;
                assert_no_tool_authority(adapter, &observed, false);
                let error = observed.result.expect_err("item overflow must fail closed");
                assert_eq!(
                    error.phase,
                    if stream {
                        LlmErrorPhase::Stream
                    } else {
                        LlmErrorPhase::TerminalValidation
                    },
                    "{} item-limit phase",
                    adapter.label()
                );
            }
        }
    }

    fn ignorable_stream_event(adapter: ConformanceAdapter) -> &'static str {
        match adapter {
            ConformanceAdapter::Chat(_) => "data: {\"choices\":[]}\n\n",
            ConformanceAdapter::Responses(_) => "data: {\"type\":\"fixture.ignored\"}\n\n",
            ConformanceAdapter::Anthropic => "event: ping\ndata: {}\n\n",
            ConformanceAdapter::Gemini => "data: {\"candidates\":[{}]}\n\n",
            ConformanceAdapter::Mock => panic!("Mock has no network stream events"),
        }
    }

    #[tokio::test]
    async fn every_network_transport_bounds_stream_event_count() {
        for adapter in NETWORK_ADAPTERS {
            let body =
                ignorable_stream_event(adapter).repeat(crate::llm::MAX_LLM_STREAM_EVENTS + 1);
            assert!(body.len() < crate::llm::MAX_LLM_STREAM_BYTES);
            let (base_url, _) = serve_once("200 OK", "text/event-stream", body);
            let error = invoke_failure(
                adapter.client(base_url).as_ref(),
                true,
                REMOTE_PROMPT_SENTINEL,
            )
            .await;
            assert_eq!(
                error.class,
                LlmErrorClass::Protocol,
                "{} event count",
                adapter.label()
            );
            assert_eq!(
                error.phase,
                LlmErrorPhase::Stream,
                "{} event phase",
                adapter.label()
            );
            assert_safe_error_surface(adapter, &error);
        }
    }

    #[tokio::test]
    async fn receiver_drop_cancels_every_network_transport_before_terminal_authority() {
        for adapter in NETWORK_ADAPTERS {
            let (base_url, request_rx, disconnect_rx) =
                serve_open_stream(first_public_stream_event(adapter));
            let client = adapter.client(base_url);
            let messages = [LlmMessage::user("drop conformance stream")];
            let mut stream = client
                .stream(LlmRequest::new(&messages, None))
                .await
                .expect("conformance stream starts");
            request_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("captured streaming request");
            assert_eq!(
                stream
                    .recv_private()
                    .await
                    .expect("first event")
                    .expect("public event"),
                LlmStreamEvent::Delta(LlmDelta::Content("first".to_string())),
                "{} first public event",
                adapter.label()
            );
            drop(stream);
            let disconnected = tokio::task::spawn_blocking(move || {
                disconnect_rx.recv_timeout(Duration::from_secs(5))
            })
            .await
            .expect("disconnect observer")
            .expect("disconnect signal");
            assert!(disconnected, "{} receiver drop", adapter.label());
        }
    }

    #[tokio::test]
    async fn post_handshake_failures_keep_actual_attempts_for_every_network_transport() {
        for adapter in NETWORK_ADAPTERS {
            let missing = terminal_fixture(adapter, TerminalCase::MissingTerminal);
            let (base_url, requests) = serve_sequence(vec![
                ScriptedHttpResponse::new(
                    "503 Service Unavailable",
                    "application/json",
                    remote_error_body(),
                ),
                ScriptedHttpResponse::new("200 OK", "text/event-stream", missing.stream),
            ]);
            let observed = observe_turn(adapter, base_url, true).await;
            assert_no_tool_authority(adapter, &observed, false);
            let error = observed
                .result
                .expect_err("post-handshake missing terminal must fail");
            assert_eq!(error.attempts.attempts(), 2, "{} attempts", adapter.label());
            assert_eq!(
                error.phase,
                LlmErrorPhase::Stream,
                "{} phase",
                adapter.label()
            );
            assert!(!error.attempts.credential_rotation_occurred());
            for _ in 0..2 {
                requests
                    .recv_timeout(Duration::from_secs(5))
                    .expect("captured post-handshake request");
            }
        }
    }

    #[tokio::test]
    async fn complete_and_stream_keep_http_401_classes_equal_for_every_network_provider() {
        let messages = [LlmMessage::user("inspect")];
        let body = json!({"error": {"code": "invalid_api_key", "message": "private"}}).to_string();

        for provider in ["openai", "grok", "openrouter", "meta"] {
            let (base_url, _) = serve_once("401 Unauthorized", "application/json", body.clone());
            let complete_error = openai_compat(provider, base_url)
                .complete(LlmRequest::new(&messages, None))
                .await
                .expect_err("complete 401");
            let (base_url, _) = serve_once("401 Unauthorized", "application/json", body.clone());
            let stream_error = openai_compat(provider, base_url)
                .stream(LlmRequest::new(&messages, None))
                .await
                .expect_err("stream 401");
            assert_eq!(complete_error.provider, provider);
            assert_complete_and_stream_errors_match(
                provider,
                complete_error,
                stream_error,
                LlmErrorClass::Authentication,
                LlmErrorPhase::HttpResponse,
            )
            .await;
        }

        let (base_url, _) = serve_once("401 Unauthorized", "application/json", body.clone());
        let complete_error = crate::llm::anthropic::AnthropicClient::with_base_url(
            "claude".to_string(),
            vec!["key".to_string()],
            base_url,
        )
        .expect("anthropic")
        .complete(LlmRequest::new(&messages, None))
        .await
        .expect_err("anthropic complete 401");
        let (base_url, _) = serve_once("401 Unauthorized", "application/json", body.clone());
        let stream_error = crate::llm::anthropic::AnthropicClient::with_base_url(
            "claude".to_string(),
            vec!["key".to_string()],
            base_url,
        )
        .expect("anthropic")
        .stream(LlmRequest::new(&messages, None))
        .await
        .expect_err("anthropic stream 401");
        assert_eq!(complete_error.provider, "anthropic");
        assert_complete_and_stream_errors_match(
            "anthropic",
            complete_error,
            stream_error,
            LlmErrorClass::Authentication,
            LlmErrorPhase::HttpResponse,
        )
        .await;

        let (base_url, _) = serve_once("401 Unauthorized", "application/json", body.clone());
        let complete_error = crate::llm::gemini::GeminiClient::with_base_url(
            "gemini-test".to_string(),
            vec!["key".to_string()],
            base_url,
        )
        .expect("gemini")
        .complete(LlmRequest::new(&messages, None))
        .await
        .expect_err("gemini complete 401");
        let (base_url, _) = serve_once("401 Unauthorized", "application/json", body.clone());
        let stream_error = crate::llm::gemini::GeminiClient::with_base_url(
            "gemini-test".to_string(),
            vec!["key".to_string()],
            base_url,
        )
        .expect("gemini")
        .stream(LlmRequest::new(&messages, None))
        .await
        .expect_err("gemini stream 401");
        assert_eq!(complete_error.provider, "google");
        assert_complete_and_stream_errors_match(
            "google",
            complete_error,
            stream_error,
            LlmErrorClass::Authentication,
            LlmErrorPhase::HttpResponse,
        )
        .await;
    }

    #[tokio::test]
    async fn documented_request_ids_reach_complete_and_stream_http_failures() {
        let openai_request_id = "req_openai-123";
        assert_eq!(
            chat_http_failure(
                "openai",
                response_header("x-request-id", openai_request_id),
                false,
            )
            .await
            .request_id
            .as_deref(),
            Some(openai_request_id)
        );
        assert_eq!(
            chat_http_failure(
                "openai",
                response_header("x-request-id", openai_request_id),
                true,
            )
            .await
            .request_id
            .as_deref(),
            Some(openai_request_id)
        );
        assert_eq!(
            responses_http_failure(
                "openai",
                response_header("x-request-id", openai_request_id),
                false,
            )
            .await
            .request_id
            .as_deref(),
            Some(openai_request_id)
        );
        assert_eq!(
            responses_http_failure(
                "openai",
                response_header("x-request-id", openai_request_id),
                true,
            )
            .await
            .request_id
            .as_deref(),
            Some(openai_request_id)
        );

        let anthropic_request_id = "req_anthropic.123";
        assert_eq!(
            anthropic_http_failure(response_header("request-id", anthropic_request_id), false,)
                .await
                .request_id
                .as_deref(),
            Some(anthropic_request_id)
        );
        assert_eq!(
            anthropic_http_failure(response_header("request-id", anthropic_request_id), true,)
                .await
                .request_id
                .as_deref(),
            Some(anthropic_request_id)
        );
    }

    #[tokio::test]
    async fn request_id_capture_is_bounded_redacted_and_provider_isolated() {
        for invalid in [
            "x".repeat(129),
            "request id with spaces".to_string(),
            "fixture-key".to_string(),
        ] {
            let error =
                chat_http_failure("openai", response_header("x-request-id", invalid), false).await;
            assert!(error.request_id.is_none());
        }

        assert!(responses_http_failure(
            "openai",
            response_header("request-id", "wrong-openai-header"),
            true,
        )
        .await
        .request_id
        .is_none());
        assert!(anthropic_http_failure(
            response_header("x-request-id", "wrong-anthropic-header"),
            false,
        )
        .await
        .request_id
        .is_none());

        let both_headers = || {
            vec![
                ("x-request-id".to_string(), "req_compatible".to_string()),
                ("request-id".to_string(), "req_native".to_string()),
            ]
        };
        for provider in ["grok", "openrouter", "meta"] {
            assert!(
                chat_http_failure(provider, both_headers(), false)
                    .await
                    .request_id
                    .is_none(),
                "{provider} Chat must not infer OpenAI or Anthropic request-ID headers"
            );
            assert!(
                responses_http_failure(provider, both_headers(), true)
                    .await
                    .request_id
                    .is_none(),
                "{provider} Responses must not infer OpenAI or Anthropic request-ID headers"
            );
        }
        assert!(gemini_http_failure(both_headers(), false)
            .await
            .request_id
            .is_none());
        assert!(gemini_http_failure(both_headers(), true)
            .await
            .request_id
            .is_none());
    }

    #[tokio::test]
    async fn complete_and_stream_reject_unsupported_reasoning_identically_for_native_and_mock() {
        let messages = [LlmMessage::user("inspect")];
        let request = || {
            LlmRequest::new(&messages, None).with_reasoning_effort(Some(ReasoningEffort::Medium))
        };

        let mock = MockLlmClient::new();
        let complete = mock.complete(request()).await.expect_err("mock complete");
        let stream = mock.stream(request()).await.expect_err("mock stream");
        assert_complete_and_stream_errors_match(
            "mock",
            complete,
            stream,
            LlmErrorClass::UnsupportedRequest,
            LlmErrorPhase::Request,
        )
        .await;

        let anthropic = crate::llm::anthropic::AnthropicClient::with_base_url(
            "claude".to_string(),
            vec!["key".to_string()],
            "http://127.0.0.1:9",
        )
        .expect("anthropic");
        let complete = anthropic
            .complete(request())
            .await
            .expect_err("anthropic complete");
        let stream = anthropic
            .stream(request())
            .await
            .expect_err("anthropic stream");
        assert_complete_and_stream_errors_match(
            "anthropic-reasoning",
            complete,
            stream,
            LlmErrorClass::UnsupportedRequest,
            LlmErrorPhase::Request,
        )
        .await;

        let gemini = crate::llm::gemini::GeminiClient::with_base_url(
            "gemini-test".to_string(),
            vec!["key".to_string()],
            "http://127.0.0.1:9",
        )
        .expect("gemini");
        let complete = gemini
            .complete(request())
            .await
            .expect_err("gemini complete");
        let stream = gemini.stream(request()).await.expect_err("gemini stream");
        assert_complete_and_stream_errors_match(
            "gemini-reasoning",
            complete,
            stream,
            LlmErrorClass::UnsupportedRequest,
            LlmErrorPhase::Request,
        )
        .await;
    }

    #[tokio::test]
    async fn final_transient_http_errors_report_actual_exhaustion_rotation_and_hints() {
        for stream in [false, true] {
            let responses = (0..3)
                .map(|attempt| {
                    let response = json_response(
                        "429 Too Many Requests",
                        json!({"error": {"code": "rate_limit_exceeded"}}),
                    );
                    if attempt == 2 {
                        response.with_header("Retry-After", "7")
                    } else {
                        response
                    }
                })
                .collect();
            let (base_url, requests) = serve_sequence(responses);
            let client = OpenAiCompatClient::configured(
                "openai".to_string(),
                "fixture-model".to_string(),
                vec!["credential-a".to_string(), "credential-b".to_string()],
                base_url,
                None,
            );
            let messages = [LlmMessage::user("rate limit")];
            let error = if stream {
                client
                    .stream(LlmRequest::new(&messages, None))
                    .await
                    .expect_err("stream rate limit")
            } else {
                client
                    .complete(LlmRequest::new(&messages, None))
                    .await
                    .expect_err("complete rate limit")
            };
            assert_eq!(error.class, LlmErrorClass::RateLimited);
            assert_eq!(
                error.retry,
                RetryDisposition::ExhaustedAfterCredentialRotation
            );
            assert_eq!(error.retry_after_seconds, Some(7));
            assert_eq!(error.attempts.attempts(), 3);
            assert!(error.attempts.credential_rotation_occurred());
            let captured = (0..3)
                .map(|_| requests.recv_timeout(Duration::from_secs(5)).unwrap())
                .collect::<Vec<_>>();
            assert!(captured[0]
                .to_ascii_lowercase()
                .contains("authorization: bearer credential-a"));
            assert!(captured[1]
                .to_ascii_lowercase()
                .contains("authorization: bearer credential-b"));
            assert!(captured[2]
                .to_ascii_lowercase()
                .contains("authorization: bearer credential-a"));
        }

        let responses = (0..3)
            .map(|attempt| {
                let response = json_response(
                    "529 Overloaded",
                    json!({"type": "error", "error": {"type": "overloaded_error"}}),
                );
                if attempt == 2 {
                    response.with_header("Retry-After", "5")
                } else {
                    response
                }
            })
            .collect();
        let (base_url, _) = serve_sequence(responses);
        let error = crate::llm::anthropic::AnthropicClient::with_base_url(
            "claude-fixture".to_string(),
            vec!["anthropic-key".to_string()],
            base_url,
        )
        .expect("Anthropic fixture")
        .complete(LlmRequest::new(&[LlmMessage::user("overload")], None))
        .await
        .expect_err("Anthropic overload");
        assert_eq!(error.class, LlmErrorClass::ProviderUnavailable);
        assert_eq!(error.retry, RetryDisposition::Exhausted);
        assert_eq!(error.retry_after_seconds, Some(5));
        assert_eq!(error.attempts.attempts(), 3);
        assert!(!error.attempts.credential_rotation_occurred());
    }

    #[tokio::test]
    async fn retried_tool_result_requests_preserve_method_path_and_body_for_each_wire_dialect() {
        let tool_result = json!({"retry_semantic_probe": true});

        let (base_url, requests) = serve_sequence(retry_then_json(
            json!({
                "choices": [{
                    "message": {"tool_calls": [{
                        "id": "call_retry",
                        "function": {"name": "probe", "arguments": "{}"}
                    }]},
                    "finish_reason": "tool_calls"
                }]
            }),
            json!({"choices": [{"message": {"content": "done"}, "finish_reason": "stop"}]}),
        ));
        let chat = OpenAiCompatClient::configured(
            "openai".to_string(),
            "fixture-model".to_string(),
            vec!["chat-key".to_string()],
            base_url,
            None,
        );
        let scope = LlmRequestScope::new("retry-session", "chat-run").unwrap();
        let first = chat
            .complete(
                LlmRequest::new(
                    &[LlmMessage::user("call probe")],
                    Some(&[ToolDefinition::function("probe")]),
                )
                .with_scope(scope.clone()),
            )
            .await
            .unwrap();
        let call = &first.tool_calls.as_ref().unwrap()[0];
        let mut continuation = first.continuation.unwrap();
        continuation
            .record_tool_result(
                ToolResult::success(call.invocation_id, tool_result.clone()).unwrap(),
            )
            .unwrap();
        let response = chat
            .complete(
                LlmRequest::new(
                    &[LlmMessage::user("call probe")],
                    Some(&[ToolDefinition::function("probe")]),
                )
                .with_scope(scope)
                .with_continuation(Some(continuation)),
            )
            .await
            .unwrap();
        assert_eq!(response.attempts.attempts(), 3);
        let retried_body = assert_three_retry_requests_are_semantically_identical(
            &requests,
            "retry_semantic_probe",
            "authorization",
            "Bearer chat-key",
        );
        assert_eq!(retried_body["messages"][0]["role"], "user");
        assert_eq!(retried_body["messages"][1]["role"], "assistant");
        assert_eq!(retried_body["messages"][2]["role"], "tool");

        let (base_url, requests) = serve_sequence(retry_then_json(
            json!({
                "id": "resp_retry_1",
                "status": "completed",
                "output": [{
                    "type": "function_call",
                    "status": "completed",
                    "call_id": "call_retry",
                    "name": "probe",
                    "arguments": "{}"
                }]
            }),
            json!({
                "id": "resp_retry_2",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "done"}]
                }]
            }),
        ));
        let responses = OpenAiResponsesClient::configured(
            "openai",
            "fixture-model".to_string(),
            vec!["responses-key".to_string()],
            format!("{base_url}/v1/responses"),
            None,
        );
        let scope = LlmRequestScope::new("retry-session", "responses-run").unwrap();
        let first = responses
            .complete(
                LlmRequest::new(
                    &[LlmMessage::user("call probe")],
                    Some(&[ToolDefinition::function("probe")]),
                )
                .with_scope(scope.clone()),
            )
            .await
            .unwrap();
        let call = &first.tool_calls.as_ref().unwrap()[0];
        let mut continuation = first.continuation.unwrap();
        continuation
            .record_tool_result(
                ToolResult::success(call.invocation_id, tool_result.clone()).unwrap(),
            )
            .unwrap();
        let response = responses
            .complete(
                LlmRequest::new(
                    &[LlmMessage::user("call probe")],
                    Some(&[ToolDefinition::function("probe")]),
                )
                .with_scope(scope)
                .with_continuation(Some(continuation)),
            )
            .await
            .unwrap();
        assert_eq!(response.attempts.attempts(), 3);
        let retried_body = assert_three_retry_requests_are_semantically_identical(
            &requests,
            "retry_semantic_probe",
            "authorization",
            "Bearer responses-key",
        );
        assert_eq!(retried_body["input"][0]["role"], "user");
        assert_eq!(retried_body["input"][1]["type"], "function_call");
        assert_eq!(retried_body["input"][2]["type"], "function_call_output");

        let (base_url, requests) = serve_sequence(retry_then_json(
            json!({
                "content": [{"type": "tool_use", "id": "toolu_retry", "name": "probe", "input": {}}],
                "stop_reason": "tool_use"
            }),
            json!({"content": [{"type": "text", "text": "done"}], "stop_reason": "end_turn"}),
        ));
        let anthropic = crate::llm::anthropic::AnthropicClient::with_base_url(
            "claude-fixture".to_string(),
            vec!["anthropic-key".to_string()],
            base_url,
        )
        .unwrap();
        let scope = LlmRequestScope::new("retry-session", "anthropic-run").unwrap();
        let first = anthropic
            .complete(
                LlmRequest::new(
                    &[LlmMessage::user("call probe")],
                    Some(&[ToolDefinition::function("probe")]),
                )
                .with_scope(scope.clone()),
            )
            .await
            .unwrap();
        let call = &first.tool_calls.as_ref().unwrap()[0];
        let mut continuation = first.continuation.unwrap();
        continuation
            .record_tool_result(
                ToolResult::success(call.invocation_id, tool_result.clone()).unwrap(),
            )
            .unwrap();
        let response = anthropic
            .complete(
                LlmRequest::new(
                    &[LlmMessage::user("call probe")],
                    Some(&[ToolDefinition::function("probe")]),
                )
                .with_scope(scope)
                .with_continuation(Some(continuation)),
            )
            .await
            .unwrap();
        assert_eq!(response.attempts.attempts(), 3);
        let retried_body = assert_three_retry_requests_are_semantically_identical(
            &requests,
            "retry_semantic_probe",
            "x-api-key",
            "anthropic-key",
        );
        assert_eq!(retried_body["messages"][0]["role"], "user");
        assert_eq!(retried_body["messages"][1]["role"], "assistant");
        assert_eq!(retried_body["messages"][2]["role"], "user");
        assert_eq!(
            retried_body["messages"][2]["content"][0]["type"],
            "tool_result"
        );

        let (base_url, requests) = serve_sequence(retry_then_json(
            json!({
                "candidates": [{
                    "content": {"role": "model", "parts": [{"functionCall": {"name": "probe", "args": {}}}]},
                    "finishReason": "STOP"
                }]
            }),
            json!({
                "candidates": [{"content": {"parts": [{"text": "done"}]}, "finishReason": "STOP"}]
            }),
        ));
        let gemini = crate::llm::gemini::GeminiClient::with_base_url(
            "gemini-fixture".to_string(),
            vec!["gemini-key".to_string()],
            base_url,
        )
        .unwrap();
        let scope = LlmRequestScope::new("retry-session", "gemini-run").unwrap();
        let first = gemini
            .complete(
                LlmRequest::new(
                    &[LlmMessage::user("call probe")],
                    Some(&[ToolDefinition::function("probe")]),
                )
                .with_scope(scope.clone()),
            )
            .await
            .unwrap();
        let call = &first.tool_calls.as_ref().unwrap()[0];
        let mut continuation = first.continuation.unwrap();
        continuation
            .record_tool_result(ToolResult::success(call.invocation_id, tool_result).unwrap())
            .unwrap();
        let response = gemini
            .complete(
                LlmRequest::new(
                    &[LlmMessage::user("call probe")],
                    Some(&[ToolDefinition::function("probe")]),
                )
                .with_scope(scope)
                .with_continuation(Some(continuation)),
            )
            .await
            .unwrap();
        assert_eq!(response.attempts.attempts(), 3);
        let retried_body = assert_three_retry_requests_are_semantically_identical(
            &requests,
            "retry_semantic_probe",
            "x-goog-api-key",
            "gemini-key",
        );
        assert_eq!(retried_body["contents"][0]["role"], "user");
        assert_eq!(retried_body["contents"][1]["role"], "model");
        assert_eq!(retried_body["contents"][2]["role"], "user");
        assert!(retried_body["contents"][2]["parts"][0]
            .get("functionResponse")
            .is_some());
    }

    #[tokio::test]
    async fn post_handshake_stream_failure_retains_successful_retry_attempts() {
        let (base_url, _) = serve_sequence(vec![
            json_response("503 Service Unavailable", json!({"error": {}})),
            ScriptedHttpResponse::new(
                "200 OK",
                "text/event-stream",
                "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
            ),
        ]);
        let client = OpenAiCompatClient::configured(
            "openai".to_string(),
            "fixture-model".to_string(),
            vec!["fixture-key".to_string()],
            base_url,
            None,
        );
        let stream = client
            .stream(LlmRequest::new(
                &[LlmMessage::user("stream after retry")],
                None,
            ))
            .await
            .expect("HTTP handshake succeeds after retry");
        let error = stream
            .finish()
            .await
            .expect_err("premature stream EOF remains a protocol failure");
        assert_eq!(error.phase, LlmErrorPhase::Stream);
        assert_eq!(error.retry, RetryDisposition::NotRetryable);
        assert_eq!(error.attempts.attempts(), 2);
        assert!(!error.attempts.credential_rotation_occurred());

        let (base_url, _) = serve_sequence(vec![
            json_response("503 Service Unavailable", json!({"error": {}})),
            ScriptedHttpResponse::new(
                "200 OK",
                "text/event-stream",
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: [DONE]\n\n"
                ),
            ),
        ]);
        let completed = OpenAiCompatClient::configured(
            "openai".to_string(),
            "fixture-model".to_string(),
            vec!["fixture-key".to_string()],
            base_url,
            None,
        )
        .stream(LlmRequest::new(
            &[LlmMessage::user("successful stream after retry")],
            None,
        ))
        .await
        .unwrap()
        .finish()
        .await
        .unwrap();
        assert_eq!(completed.content.as_deref(), Some("done"));
        assert_eq!(completed.attempts.attempts(), 2);
        assert!(!completed.attempts.credential_rotation_occurred());
    }

    #[tokio::test]
    async fn mock_complete_and_stream_run_correlated_parallel_tool_continuations() {
        async fn two_request_turn(stream: bool) -> Value {
            let client = MockLlmClient::new();
            let scope = LlmRequestScope::new(
                "mock-conformance-session",
                if stream { "stream-run" } else { "complete-run" },
            )
            .expect("Mock scope");
            let messages = [LlmMessage::user("mock parallel continuation")];
            let tools = [
                ToolDefinition::function("record_probe_a"),
                ToolDefinition::function("record_probe_b"),
            ];
            let first_request = LlmRequest::new(&messages, Some(&tools)).with_scope(scope.clone());
            let first = if stream {
                client
                    .stream(first_request)
                    .await
                    .expect("first Mock stream")
                    .finish()
                    .await
                    .expect("first Mock stream completion")
            } else {
                client
                    .complete(first_request)
                    .await
                    .expect("first Mock turn")
            };
            assert_eq!(first.attempts.attempts(), 0);
            assert!(
                first.usage.is_none(),
                "Mock must report usage as unavailable"
            );
            let calls = first.tool_calls.as_ref().expect("parallel Mock calls");
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].name, "record_probe_a");
            assert_eq!(calls[1].name, "record_probe_b");
            assert_ne!(calls[0].invocation_id, calls[1].invocation_id);

            let mut continuation = first.continuation.expect("Mock continuation");
            continuation
                .record_tool_result(
                    ToolResult::success(calls[1].invocation_id, json!({"receipt": "b"}))
                        .expect("second result"),
                )
                .expect("correlate second result first");
            continuation
                .record_tool_result(
                    ToolResult::error(calls[0].invocation_id, json!({"receipt": "a"}))
                        .expect("first result"),
                )
                .expect("correlate first result second");

            let second_request = LlmRequest::new(&messages, Some(&tools))
                .with_scope(scope)
                .with_continuation(Some(continuation));
            let second = if stream {
                client
                    .stream(second_request)
                    .await
                    .expect("second Mock stream")
                    .finish()
                    .await
                    .expect("second Mock stream completion")
            } else {
                client
                    .complete(second_request)
                    .await
                    .expect("second Mock turn")
            };
            assert_eq!(second.attempts.attempts(), 0);
            assert!(
                second.usage.is_none(),
                "Mock must report usage as unavailable"
            );
            let content = second.content.expect("Mock final content");
            let encoded = content
                .strip_prefix("Mock tool results: ")
                .expect("Mock result prefix");
            serde_json::from_str(encoded).expect("Mock result projection")
        }

        let complete = two_request_turn(false).await;
        let streamed = two_request_turn(true).await;
        assert_eq!(complete, streamed);
        assert_eq!(complete[0]["tool"], "record_probe_a");
        assert_eq!(complete[0]["classification"], "error");
        assert_eq!(complete[0]["output"], json!({"receipt": "a"}));
        assert_eq!(complete[1]["tool"], "record_probe_b");
        assert_eq!(complete[1]["classification"], "success");
        assert_eq!(complete[1]["output"], json!({"receipt": "b"}));
    }

    #[tokio::test]
    async fn factory_mock_client_accepts_typed_requests() {
        let mut providers = HashMap::new();
        providers.insert(
            "mock".to_string(),
            ProviderEntry {
                model: "mock-model".to_string(),
                ..ProviderEntry::default()
            },
        );
        let config = LlmConfig {
            active_provider: Some("mock".to_string()),
            providers,
            ..LlmConfig::default()
        };
        let client = create_client(&config, Some("mock")).expect("mock client");
        let messages = [LlmMessage::user("explore project")];
        let tools = [ToolDefinition::function("list_directory")];
        let scope = LlmRequestScope::new("factory-session", "factory-run").expect("scope");
        let response = client
            .complete(LlmRequest::new(&messages, Some(&tools)).with_scope(scope))
            .await
            .expect("typed mock completion");
        assert!(response.tool_calls.is_some());
    }
}
