# Preferences

## 2026-10-07 - Model catalog retention

- Source: user, explicit clarification
- Confidence: high
- Preference: Keep available curated text/tool models from the last three major
  generations per provider family. Point releases do not count as separate major
  generations. Keep fewer generations when older models are unavailable; preserve
  current defaults and configured selections.
- Reference: [T070](../../specs/test/llm-providers/T070_three_generation_model_catalog.md).

## 2026-10-07 - Revised catalog retention and OpenRouter Mistral

- Source: user, subsequent instruction
- Confidence: high
- Supersedes: the three-major-generation preference above
- Preference: Retain available curated text/tool models from the latest two major
  generations per provider family, including Mistral families through OpenRouter.
  Point releases remain one generation. Preserve defaults and explicit selected
  models, including selections outside the bundled retention window.
- Reference: [T071](../../specs/test/llm-providers/T071_two_generation_model_catalog.md).

## 2026-10-07 - Latest-major-generation catalog retention

- Source: user, subsequent instruction
- Confidence: high
- Supersedes: the two-major-generation preference above
- Preference: Retain available curated text/tool models from the latest major
  generation per existing provider family, including Mistral through OpenRouter.
  Point releases remain within the same generation. Preserve current defaults
  and explicit selections outside the bundled retention window.
- Reference: [T072](../../specs/test/llm-providers/T072_latest_generation_model_catalog.md).

## 2026-10-08 - Provider-specific catalog selection

- Source: user, subsequent instruction
- Confidence: high
- Supersedes: the uniform latest-major-generation preference for OpenAI/Grok/Gemini
- Preference: Include general OpenAI 5.6 and newer canonical text/tool models; retain only
  the latest three general Grok releases; keep Gemini Flash 3.8, 3.7, 3.6 and 3.5
  and check for newer ordinary Flash releases. Apply corresponding selections to
  OpenRouter vendors, using route-specific verified IDs. Other families retain
  their latest major generation. Preserve defaults and explicit selections.
- Clarification: The user explicitly chose three newest general Grok releases and
  general OpenAI models only; exclude Cyber, Pro and Grok Build suggestions.
- Reference: [T073](../../specs/test/llm-providers/T073_provider_specific_model_selection.md).

## 2026-10-13 - Compact Claude-style waiting indicator

- Source: user, explicit display example
- Confidence: high
- Preference: Use a compact animated-star activity line such as
  `✽ Flowing… (12s · ↓ 743 tokens)` in nib's TUI.
- Implementation: Preserve explicit human-wait labels; mark generated estimates
  with `~` and keep the current run separate from history.
- Reference: [T085](../../specs/test/interactive/T085_claude_style_waiting_indicator.md).

## 2026-10-19 - Codex-style startup screen

- Source: user, explicit display example
- Confidence: high
- Preference: The TUI startup screen follows Codex: a rounded box for an available
  update (`✨ Update available! <current> -> <latest>`, the update command and
  release notes), then `>_ nib (v<version>)` with the working directory under it.
- Clarification: Keep the Session and Keys hints below the header.
- Reference: [T086](../../specs/test/interactive/T086_codex_style_startup_screen.md).
