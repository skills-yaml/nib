#[path = "conformance_tests/anthropic_continuation.rs"]
mod anthropic_continuation;

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
    LlmDelta, LlmError, LlmErrorClass, LlmErrorPhase, LlmFinishReason, LlmProvider, LlmResponse,
    LlmStreamEvent, LlmTerminalStatus, RetryDisposition,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::{mpsc::Receiver, Arc};
use std::time::Duration;

pub(crate) const ACTIVE_CONFORMANCE_KEY: &str = "active-secret-42";
pub(crate) const INACTIVE_CONFORMANCE_KEY: &str = "inactive-secret-84";
pub(crate) const REMOTE_PROMPT_SENTINEL: &str = "remote-prompt-sentinel-7531";
pub(crate) const REMOTE_LABEL_SENTINEL: &str =
    "provider=openai transport=responses model=remote endpoint=/private";
pub(crate) const SAFE_FIXTURE_MODEL: &str = "fixture-model";

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
pub(crate) enum ConformanceAdapter {
    Chat(&'static str),
    Responses(&'static str),
    Anthropic,
    Gemini,
    Mock,
}

pub(crate) const ALL_ADAPTERS: [ConformanceAdapter; 11] = [
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

pub(crate) const NETWORK_ADAPTERS: [ConformanceAdapter; 10] = [
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
    pub(crate) fn label(self) -> &'static str {
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

    pub(crate) fn provider(self) -> &'static str {
        match self {
            Self::Chat(provider) => provider,
            Self::Responses(provider) => provider,
            Self::Anthropic => "anthropic",
            Self::Gemini => "google",
            Self::Mock => "mock",
        }
    }

    pub(crate) fn transport(self) -> &'static str {
        match self {
            Self::Chat(_) => "chat_completions",
            Self::Responses(_) => "responses",
            Self::Anthropic => "anthropic_messages",
            Self::Gemini => "gemini_generate_content",
            Self::Mock => "mock",
        }
    }

    pub(crate) fn registry_transport(self) -> ProviderTransport {
        match self {
            Self::Chat(_) => ProviderTransport::ChatCompletions,
            Self::Responses(_) => ProviderTransport::Responses,
            Self::Anthropic => ProviderTransport::AnthropicMessages,
            Self::Gemini => ProviderTransport::GeminiGenerateContent,
            Self::Mock => ProviderTransport::Local,
        }
    }

    pub(crate) fn expected_path(self, stream: bool) -> &'static str {
        match (self, stream) {
            (Self::Chat(_), _) => "/chat/completions",
            (Self::Responses(_), _) => "/v1/responses",
            (Self::Anthropic, _) => "/v1/messages",
            (Self::Gemini, false) => "/v1beta/models/fixture-model:generateContent",
            (Self::Gemini, true) => "/v1beta/models/fixture-model:streamGenerateContent?alt=sse",
            (Self::Mock, _) => "",
        }
    }

    pub(crate) fn documented_request_id(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Chat("openai") | Self::Responses("openai") => {
                Some(("x-request-id", "req_openai-matrix-123"))
            }
            Self::Anthropic => Some(("request-id", "req_anthropic-matrix.123")),
            _ => None,
        }
    }

    pub(crate) fn client(self, base_url: String) -> Arc<dyn LlmProvider> {
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

pub(crate) async fn invoke_failure(
    client: &dyn LlmProvider,
    stream: bool,
    prompt: &str,
) -> LlmError {
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

pub(crate) fn remote_error_body() -> String {
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

pub(crate) fn assert_safe_error_surface(adapter: ConformanceAdapter, error: &LlmError) {
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
pub(crate) enum TerminalCase {
    InBandError,
    MissingTerminal,
    Truncated,
    RefusedOrSafetyBlocked,
    UnknownTerminal,
    InconsistentToolTerminal,
    MalformedToolArguments,
}

pub(crate) const TERMINAL_CASES: [TerminalCase; 7] = [
    TerminalCase::InBandError,
    TerminalCase::MissingTerminal,
    TerminalCase::Truncated,
    TerminalCase::RefusedOrSafetyBlocked,
    TerminalCase::UnknownTerminal,
    TerminalCase::InconsistentToolTerminal,
    TerminalCase::MalformedToolArguments,
];

pub(crate) struct WireFixture {
    complete: String,
    stream: String,
}

pub(crate) fn responses_terminal(output: Value, status: &str) -> Value {
    json!({"id": "resp_matrix", "status": status, "output": output})
}

#[expect(clippy::too_many_lines, reason = "legacy function recorded by T044")]
pub(crate) fn terminal_fixture(adapter: ConformanceAdapter, case: TerminalCase) -> WireFixture {
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
                    let value = json!({"candidates": [{"finishReason": "REMOTE_FUTURE_TERMINAL"}]});
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

async fn observe_turn(adapter: ConformanceAdapter, base_url: String, stream: bool) -> ObservedTurn {
    let client = adapter.client(base_url);
    let messages = [LlmMessage::user(REMOTE_PROMPT_SENTINEL)];
    let request = || {
        LlmRequest::new(&messages, None).with_scope(
            LlmRequestScope::new("conformance-session", "terminal-run").expect("conformance scope"),
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

async fn gemini_http_failure(headers: Vec<(String, String)>, stream: bool) -> crate::llm::LlmError {
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

pub(crate) async fn assert_complete_and_stream_errors_match(
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

pub(crate) fn expected_http_class(status: u16) -> LlmErrorClass {
    match status {
        400 => LlmErrorClass::ProviderRejected,
        401 | 403 => LlmErrorClass::Authentication,
        429 => LlmErrorClass::RateLimited,
        408 | 425 | 500 | 502 | 503 | 504 | 529 => LlmErrorClass::ProviderUnavailable,
        _ => panic!("unhandled conformance status {status}"),
    }
}

pub(crate) fn scripted_http_failures(
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

pub(crate) fn first_public_stream_event(adapter: ConformanceAdapter) -> String {
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

pub(crate) fn output_item_overflow_fixture(adapter: ConformanceAdapter) -> WireFixture {
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

pub(crate) fn ignorable_stream_event(adapter: ConformanceAdapter) -> &'static str {
    match adapter {
        ConformanceAdapter::Chat(_) => "data: {\"choices\":[]}\n\n",
        ConformanceAdapter::Responses(_) => "data: {\"type\":\"fixture.ignored\"}\n\n",
        ConformanceAdapter::Anthropic => "event: ping\ndata: {}\n\n",
        ConformanceAdapter::Gemini => "data: {\"candidates\":[{}]}\n\n",
        ConformanceAdapter::Mock => panic!("Mock has no network stream events"),
    }
}

#[path = "test_part_0.rs"]
mod test_part_0;
