"""LLM client abstraction for nib.

Supports pluggable providers (Grok, OpenAI-compatible, etc.).
The agent loop uses this to drive reasoning and tool selection.
"""

from __future__ import annotations

import json
from abc import ABC, abstractmethod
from typing import Any

from pydantic import BaseModel, Field

try:
    import litellm
    from litellm import acompletion
except ImportError:
    litellm = None
    acompletion = None


class ToolCallRequest(BaseModel):
    """A tool call requested by the LLM."""
    name: str
    arguments: dict[str, Any]


class LLMResponse(BaseModel):
    """Normalized response from an LLM."""
    content: str | None = None
    tool_calls: list[ToolCallRequest] | None = None
    finish_reason: str = "stop"
    usage: dict[str, int] | None = None
    raw: Any | None = None  # provider-specific raw response


class LLMClient(ABC):
    """Abstract interface for LLM providers."""

    @abstractmethod
    async def complete(
        self,
        messages: list[dict[str, str]],
        tools: list[dict[str, Any]] | None = None,
        temperature: float = 0.7,
        max_tokens: int | None = None,
        **kwargs,
    ) -> LLMResponse:
        """Send a completion request.

        messages: [{"role": "system"|"user"|"assistant", "content": "..."}, ...]
        tools: optional list of tool schemas (OpenAI/Anthropic style).
        """
        raise NotImplementedError


class LiteLLMClient(LLMClient):
    """Unified client using LiteLLM for OpenAI, Anthropic, Google Gemini, Grok, OpenRouter, etc.

    Usage examples:
        LiteLLMClient(model="gpt-4o", api_key=...)
        LiteLLMClient(model="anthropic/claude-3-5-sonnet-20241022")
        LiteLLMClient(model="gemini/gemini-1.5-pro")
        LiteLLMClient(model="xai/grok-2-1212")  # or "grok-beta"
        LiteLLMClient(model="openrouter/anthropic/claude-3.5-sonnet")
    """

    def __init__(
        self,
        model: str,
        api_key: str | None = None,
        base_url: str | None = None,
        **litellm_kwargs,
    ):
        if litellm is None:
            raise ImportError("litellm is required for LiteLLMClient. Install with: pip install litellm")
        self.model = model
        self.api_key = api_key
        self.base_url = base_url
        self.litellm_kwargs = litellm_kwargs

    async def complete(
        self,
        messages: list[dict[str, str]],
        tools: list[dict[str, Any]] | None = None,
        temperature: float = 0.7,
        max_tokens: int | None = None,
        **kwargs,
    ) -> LLMResponse:
        call_kwargs = {
            "model": self.model,
            "messages": messages,
            "temperature": temperature,
            "max_tokens": max_tokens,
            **self.litellm_kwargs,
            **kwargs,
        }

        if self.api_key:
            call_kwargs["api_key"] = self.api_key
        if self.base_url:
            call_kwargs["base_url"] = self.base_url

        if tools:
            call_kwargs["tools"] = tools
            call_kwargs["tool_choice"] = "auto"

        response = await acompletion(**call_kwargs)

        choice = response.choices[0].message
        tool_calls = None

        if getattr(choice, "tool_calls", None):
            tool_calls = []
            for tc in choice.tool_calls:
                args = {}
                if tc.function.arguments:
                    try:
                        args = json.loads(tc.function.arguments)
                    except Exception:
                        args = {"raw": tc.function.arguments}
                tool_calls.append(
                    ToolCallRequest(name=tc.function.name, arguments=args)
                )

        return LLMResponse(
            content=choice.content,
            tool_calls=tool_calls,
            finish_reason=getattr(response.choices[0], "finish_reason", "stop"),
            usage=response.usage.model_dump() if response.usage else None,
            raw=response,
        )


class MockLLMClient(LLMClient):
    """Simple mock client for testing and early development.

    It returns a fixed response or simulates basic tool use based on the last message.
    Replace with a real client (Grok, etc.) in production.
    """

    def __init__(self, responses: list[str] | None = None):
        self.responses = responses or [
            "I will use the list_directory tool to explore the project.",
            "To fix the issue I need to read the main file first.",
        ]
        self.index = 0

    async def complete(
        self,
        messages: list[dict[str, str]],
        tools: list[dict[str, Any]] | None = None,
        temperature: float = 0.7,
        max_tokens: int | None = None,
        **kwargs,
    ) -> LLMResponse:
        last = messages[-1]["content"] if messages else ""
        # Very naive simulation for demo
        if "explore" in last.lower() or "list" in last.lower():
            return LLMResponse(
                content=None,
                tool_calls=[ToolCallRequest(name="list_directory", arguments={"path": "."})],
                finish_reason="tool_calls",
            )
        if "read" in last.lower() or "main" in last.lower():
            return LLMResponse(
                content=None,
                tool_calls=[ToolCallRequest(name="read_file", arguments={"path": "src/nib/__main__.py"})],
                finish_reason="tool_calls",
            )
        # Default: final answer
        resp = self.responses[self.index % len(self.responses)]
        self.index += 1
        return LLMResponse(content=resp, finish_reason="stop")


def get_llm_client(
    provider: str = "openai",
    model: str | None = None,
    api_key: str | None = None,
    base_url: str | None = None,
    **kwargs,
) -> LLMClient:
    """Factory to get an LLM client for common providers.

    Supported providers / model strings:
    - "openai" or model="gpt-4o" (uses OPENAI_API_KEY)
    - "anthropic" or "claude-3-5-sonnet-20241022"
    - "google" / "gemini" or "gemini/gemini-1.5-pro"
    - "grok" / "xai" or "xai/grok-2"
    - "openrouter" or "openrouter/..."
    - "mock"

    You can also pass full litellm model strings like "openrouter/anthropic/claude-3.5-sonnet".
    """
    provider = provider.lower()

    if provider == "mock":
        return MockLLMClient(**kwargs)

    # Normalize model string based on provider so that switching models
    # stays within the correct provider's API (using its key).
    provider_prefix = {
        "openai": "",
        "anthropic": "anthropic/",
        "google": "gemini/",
        "gemini": "gemini/",
        "grok": "xai/",
        "xai": "xai/",
        "openrouter": "openrouter/",
    }

    prefix = provider_prefix.get(provider, "")

    if model:
        # user provided a model name, e.g. "grok-3" or full "xai/grok-3"
        m = model
        if prefix and not m.startswith(prefix):
            # prepend the provider prefix if not already present
            # strip common variants
            m = m.lstrip("openai/").lstrip("anthropic/").lstrip("gemini/").lstrip("xai/").lstrip("openrouter/")
            final_model = prefix + m
        else:
            final_model = m
    else:
        # use default for the provider
        defaults = {
            "openai": "gpt-4o",
            "anthropic": "claude-3-5-sonnet-20241022",
            "google": "gemini-1.5-pro",
            "gemini": "gemini-1.5-pro",
            "grok": "grok-2-1212",
            "xai": "grok-2-1212",
            "openrouter": "anthropic/claude-3.5-sonnet",
        }
        base = defaults.get(provider, provider)
        final_model = prefix + base if prefix and not base.startswith(prefix) else base

    if acompletion is None:
        # Fallback to mock if litellm not installed
        print("[nib] litellm not installed, falling back to MockLLMClient")
        return MockLLMClient(**kwargs)

    return LiteLLMClient(
        model=final_model,
        api_key=api_key,
        base_url=base_url,
        **kwargs,
    )
