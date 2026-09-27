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
#[path = "conformance_tests.rs"]
mod tests;
