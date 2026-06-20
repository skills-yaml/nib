"""LLM integration for nib (FT-004).

Provides pluggable clients so the AgentLoop can drive reasoning.

Supported via LiteLLM (or mock):
- openai / gpt-*
- anthropic / claude-*
- google / gemini/*
- grok / xai/*
- openrouter / openrouter/*

Example:
    from nib.llm import get_llm_client
    llm = get_llm_client("openai", model="gpt-4o")
    llm = get_llm_client("grok")
    llm = get_llm_client("openrouter", model="openrouter/meta-llama/llama-3.1-70b-instruct")
"""

from .base import (
    LLMClient,
    LLMResponse,
    LiteLLMClient,
    MockLLMClient,
    ToolCallRequest,
    get_llm_client,
)

__all__ = [
    "LLMClient",
    "LLMResponse",
    "LiteLLMClient",
    "MockLLMClient",
    "ToolCallRequest",
    "get_llm_client",
]
