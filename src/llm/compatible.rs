//! Distinct provider implementations over shared OpenAI wire codecs.
//!
//! Provider identity remains a concrete type even when providers share Chat
//! Completions or Responses framing. Endpoint/configuration resolution stays in the
//! registry-owned parser; these wrappers own the provider identity passed into error,
//! retry, continuation, and diagnostic paths.

use crate::config::{LlmApiMode, ReasoningEffort};
use crate::llm::openai::OpenAiCompatClient;
use crate::llm::responses::OpenAiResponsesClient;
use crate::llm::{LlmClient, LlmError, LlmRequest, LlmResponse, LlmStream};

enum CompatibleWireClient {
    Chat(OpenAiCompatClient),
    Responses(OpenAiResponsesClient),
}

impl CompatibleWireClient {
    #[allow(clippy::too_many_arguments)]
    fn configured(
        provider: &'static str,
        model: String,
        credentials: Vec<String>,
        diagnostic_secrets: Vec<String>,
        endpoint: String,
        reasoning_effort: Option<ReasoningEffort>,
        api_mode: LlmApiMode,
        http_client: Option<reqwest::Client>,
    ) -> Self {
        match api_mode {
            LlmApiMode::ChatCompletions => Self::Chat(match http_client {
                Some(client) => OpenAiCompatClient::configured_with_diagnostic_secrets_and_client(
                    provider.to_string(),
                    model,
                    credentials,
                    diagnostic_secrets,
                    endpoint,
                    reasoning_effort,
                    client,
                ),
                None => OpenAiCompatClient::configured_with_diagnostic_secrets(
                    provider.to_string(),
                    model,
                    credentials,
                    diagnostic_secrets,
                    endpoint,
                    reasoning_effort,
                ),
            }),
            LlmApiMode::Responses => Self::Responses(match http_client {
                Some(client) => {
                    OpenAiResponsesClient::configured_with_diagnostic_secrets_and_client(
                        provider,
                        model,
                        credentials,
                        diagnostic_secrets,
                        endpoint,
                        reasoning_effort,
                        client,
                    )
                }
                None => OpenAiResponsesClient::configured_with_diagnostic_secrets(
                    provider,
                    model,
                    credentials,
                    diagnostic_secrets,
                    endpoint,
                    reasoning_effort,
                ),
            }),
        }
    }

    // Delegation must preserve the canonical provider-neutral typed failure. Boxing only this
    // wire selector would add conversion churn without changing the underlying error contract.
    #[allow(clippy::result_large_err)]
    async fn complete(&self, request: LlmRequest<'_>) -> Result<LlmResponse, LlmError> {
        match self {
            Self::Chat(client) => client.complete(request).await,
            Self::Responses(client) => client.complete(request).await,
        }
    }

    #[allow(clippy::result_large_err)]
    async fn stream(&self, request: LlmRequest<'_>) -> Result<LlmStream, LlmError> {
        match self {
            Self::Chat(client) => client.stream(request).await,
            Self::Responses(client) => client.stream(request).await,
        }
    }
}

macro_rules! distinct_compatible_provider {
    ($name:ident, $provider:literal) => {
        pub(crate) struct $name {
            wire: CompatibleWireClient,
        }

        impl $name {
            #[allow(clippy::too_many_arguments)]
            pub(crate) fn configured(
                model: String,
                credentials: Vec<String>,
                diagnostic_secrets: Vec<String>,
                endpoint: String,
                reasoning_effort: Option<ReasoningEffort>,
                api_mode: LlmApiMode,
                http_client: Option<reqwest::Client>,
            ) -> Self {
                Self {
                    wire: CompatibleWireClient::configured(
                        $provider,
                        model,
                        credentials,
                        diagnostic_secrets,
                        endpoint,
                        reasoning_effort,
                        api_mode,
                        http_client,
                    ),
                }
            }
        }

        #[async_trait::async_trait]
        impl LlmClient for $name {
            async fn complete(&self, request: LlmRequest<'_>) -> Result<LlmResponse, LlmError> {
                self.wire.complete(request).await
            }

            async fn stream(&self, request: LlmRequest<'_>) -> Result<LlmStream, LlmError> {
                self.wire.stream(request).await
            }
        }
    };
}

distinct_compatible_provider!(OpenAiProvider, "openai");
distinct_compatible_provider!(XaiProvider, "grok");
distinct_compatible_provider!(OpenRouterProvider, "openrouter");
distinct_compatible_provider!(MetaProvider, "meta");
