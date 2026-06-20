"""Configuration management for nib, focused on LLM providers.

Stored in <project>/.nib/config.json for simplicity (JSON is easy and human-readable).
API keys are stored in plain text — this is intentional for local dev tools.
Users should .gitignore .nib/ if they don't want keys in the repo (or use env vars).
"""

from __future__ import annotations

import json
from dataclasses import dataclass, asdict
from pathlib import Path
from typing import Dict, Optional


SUPPORTED_PROVIDERS = {
    "openai": "OpenAI (gpt-4o, etc.)",
    "anthropic": "Anthropic (Claude)",
    "google": "Google Gemini",
    "grok": "xAI Grok",
    "openrouter": "OpenRouter (any model)",
}

AVAILABLE_MODELS = {
    "openai": [
        "gpt-4o",
        "gpt-4o-mini",
        "gpt-4-turbo",
        "o1-preview",
    ],
    "anthropic": [
        "claude-3-5-sonnet-20241022",
        "claude-3-opus-20240229",
        "claude-3-haiku-20240307",
    ],
    "google": [
        "gemini-1.5-pro",
        "gemini-1.5-flash",
        "gemini-2.0-flash-exp",
    ],
    "grok": [
        "grok-2-1212",
        "grok-beta",
        "grok-3",
    ],
    "openrouter": [
        "openrouter/anthropic/claude-3.5-sonnet",
        "openrouter/meta-llama/llama-3.1-70b-instruct",
        "openrouter/google/gemini-1.5-pro",
        "openrouter/mistralai/mistral-large",
    ],
    "mock": ["mock-model"],
}


@dataclass
class ProviderConfig:
    provider: str
    model: str
    api_key: Optional[str] = None
    base_url: Optional[str] = None

    def to_dict(self):
        return {k: v for k, v in asdict(self).items() if v is not None}


class LLMConfig:
    """Manages multiple LLM providers for a project."""

    def __init__(self, project_root: Path | None = None):
        if project_root is None:
            project_root = Path.cwd()
        self.project_root = project_root.resolve()
        self.config_path = self.project_root / ".nib" / "config.json"
        self.config_path.parent.mkdir(parents=True, exist_ok=True)

        self.active_provider: str = "mock"
        self.providers: Dict[str, ProviderConfig] = {}

        self._load()

    def _load(self):
        if not self.config_path.exists():
            return
        try:
            data = json.loads(self.config_path.read_text())
            self.active_provider = data.get("active_provider", "mock")
            providers_data = data.get("providers", {})
            self.providers = {
                name: ProviderConfig(**cfg) for name, cfg in providers_data.items()
            }
        except Exception:
            pass  # start fresh on corruption

    def save(self):
        data = {
            "active_provider": self.active_provider,
            "providers": {name: cfg.to_dict() for name, cfg in self.providers.items()},
        }
        self.config_path.write_text(json.dumps(data, indent=2))

    def add_or_update_provider(
        self, provider: str, model: str, api_key: Optional[str] = None, base_url: Optional[str] = None
    ):
        existing = self.providers.get(provider)
        final_api_key = api_key if api_key is not None else (existing.api_key if existing else None)
        final_base_url = base_url if base_url is not None else (existing.base_url if existing else None)

        self.providers[provider] = ProviderConfig(
            provider=provider, model=model, api_key=final_api_key, base_url=final_base_url
        )
        if self.active_provider == "mock":
            self.active_provider = provider
        self.save()

    def update_model_for_active(self, new_model: str):
        """Change only the model for the currently active provider (keeps api key)."""
        cfg = self.get_provider()
        if cfg:
            cfg.model = new_model
            self.save()

    def get_provider(self, name: Optional[str] = None) -> Optional[ProviderConfig]:
        name = name or self.active_provider
        return self.providers.get(name)

    def get_available_models(self, provider: Optional[str] = None) -> list[str]:
        provider = provider or self.active_provider
        return AVAILABLE_MODELS.get(provider, [])

    def set_active(self, provider: str):
        if provider in self.providers or provider == "mock":
            self.active_provider = provider
            self.save()
        else:
            raise ValueError(f"Provider {provider} not configured. Add it first.")

    def list_providers(self) -> Dict[str, ProviderConfig]:
        return self.providers

    def get_current_client(self):
        """Returns an LLM client for the active provider."""
        from nib.llm import get_llm_client

        cfg = self.get_provider()
        if not cfg or cfg.provider == "mock":
            return get_llm_client("mock")

        return get_llm_client(
            provider=cfg.provider,
            model=cfg.model,
            api_key=cfg.api_key,
            base_url=cfg.base_url,
        )
