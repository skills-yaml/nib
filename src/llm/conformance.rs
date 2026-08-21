//! Shared provider-neutral request validation and adapter conformance fixtures.

use super::types::{LlmRequest, ReasoningOption};
use crate::config::ReasoningEffort;

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
    use crate::llm::responses::OpenAiResponsesClient;
    use crate::llm::test_support::serve_once;
    use crate::llm::types::{GenerationOptions, LlmMessage, ToolDefinition};
    use crate::llm::{LlmClient, LlmErrorClass, LlmErrorPhase};
    use serde_json::json;
    use std::collections::HashMap;

    fn openai_compat(provider: &str, base_url: String) -> OpenAiCompatClient {
        OpenAiCompatClient::configured(
            provider.to_string(),
            "fixture-model".to_string(),
            vec!["fixture-key".to_string()],
            base_url,
            None,
        )
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

    #[tokio::test]
    async fn complete_and_stream_keep_http_401_classes_equal_for_openai_compatible_providers() {
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

            assert_eq!(
                complete_error.class,
                LlmErrorClass::Authentication,
                "{provider}"
            );
            assert_eq!(stream_error.class, complete_error.class, "{provider}");
            assert_eq!(
                complete_error.phase,
                LlmErrorPhase::HttpResponse,
                "{provider}"
            );
            assert_eq!(stream_error.phase, complete_error.phase, "{provider}");
            assert_eq!(complete_error.provider, provider);
            assert_eq!(stream_error.provider, provider);
        }
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
        let response = client
            .complete(LlmRequest::new(&messages, Some(&tools)))
            .await
            .expect("typed mock completion");
        assert!(response.tool_calls.is_some());
    }
}
