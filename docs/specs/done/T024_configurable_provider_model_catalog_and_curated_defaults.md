# T024: Configurable Provider Model Catalog and Curated Defaults

**Status:** Done

**Related:**
[T007: Configuration Schema Alignment](../done/T007_configuration_schema_alignment_and_nib_doctor_validation.md),
[T021: OpenAI-Compatible Reasoning and Tool Transport Compatibility](../done/T021_openai_compatible_reasoning_and_tool_transport_compatibility.md),
[T022: Provider-Neutral LLM Contract and Adapter Conformance](../done/T022_provider_neutral_llm_contract_and_adapter_conformance.md),
[T023: Live LLM Provider and Model Integration Qualification](../development/T023_live_llm_provider_model_integration_qualification.md), and
[User Guide](../../user/guide.md)

## Summary

Move nib's curated provider model suggestions out of Rust constants and into a strict,
source-controlled TOML catalog. Refresh that catalog from current first-party provider
guidance, and let users replace the picker suggestions for any configured provider with
an optional `models = [...]` list in `.nib/config.toml`.

The catalog is advisory. `model` remains the selected runtime model and may contain an
ID not present in either the bundled or configured suggestion list. This preserves
custom gateways and newly released models while making `/model` and `nib auth` useful
by default.

## Problem Statement

The provider registry currently hard-codes model suggestions and defaults in
`src/llm/registry.rs`. The entries include retired or superseded models, and changing a
picker list requires a Rust source change. Users can type a custom model ID, but they
cannot persist their own provider-specific suggestion list.

Provider releases move faster than nib releases. Treating a stale source constant as a
support matrix can mislead users, while fetching live catalogs during ordinary CLI use
would add credentials, latency, network dependence, and mutable behavior. nib needs a
curated offline baseline with an explicit user override boundary.

## Goals

- Store the bundled provider default model and curated suggestions in strict TOML data.
- Refresh direct-provider defaults from current first-party model and lifecycle docs.
- Keep OpenRouter to a small reviewed set of exact canonical model IDs.
- Allow `llm.providers.<id>.models` to replace bundled picker suggestions.
- Keep `llm.providers.<id>.model` free-form and always visible in the effective picker.
- Preserve existing configuration files without migration or automatic rewrites.
- Validate count, size, emptiness, duplicates, and unsafe values deterministically.
- Document that suggestions are not a live availability or compatibility guarantee.

## Non-Goals

- Fetching provider model catalogs during `nib auth`, `nib chat`, startup, or doctor.
- Restricting runtime model IDs to the bundled or configured suggestion list.
- Automatically editing user configuration when providers release or retire models.
- Claiming live compatibility before T023 qualification passes.
- Adding model capability, price, context-window, or transport inference by model name.
- Adding image, audio, video, embedding, hosted-tool, or provider-specific reasoning
  features.

## Proposed Design

### Bundled Catalog

Add `src/llm/default_models.toml` and embed it with `include_str!` so installed binaries
have deterministic offline defaults. The file has schema version `1` and one entry for
every registered provider, including Mock. Each provider entry contains:

- `default_model`;
- an ordered, unique `models` list containing the default;
- `source_url`; and
- `verified_on` in `YYYY-MM-DD` form.

The registry retains static provider identity, endpoint, credential, implementation,
and transport metadata. It exposes catalog-backed methods for default model and model
suggestions. Catalog parsing is cached once per process and fails closed if the bundled
document is invalid. Deterministic tests prove the committed catalog is valid, complete,
bounded, and consistent with the provider registry.

The initial curated defaults are:

- OpenAI: `gpt-5.6-sol` (default), `gpt-5.6-terra`, `gpt-5.6-luna`.
- Anthropic: `claude-opus-5` (default), `claude-fable-5`, `claude-sonnet-5`,
  `claude-haiku-4-5-20251001`.
- Google Gemini: `gemini-3.6-flash` (default), `gemini-3.5-flash`,
  `gemini-3.5-flash-lite`, `gemini-3.1-pro-preview`.
- xAI/Grok: `grok-4.5` (default), `grok-4.3`, `grok-build-0.1`.
- OpenRouter: `openai/gpt-5.6-sol` (default), `anthropic/claude-opus-5`,
  `google/gemini-3.6-flash`, `x-ai/grok-4.5`.
- Meta: `muse-spark-1.1`.
- Mock: `mock-model`.

These are text-output models appropriate for nib's current LLM surface. Provider-wide
specialized catalogs remain discoverable by T023 but are not picker defaults.

### User Configuration

Extend `ProviderEntry` with an optional list:

```toml
[llm.providers.openai]
model = "gpt-5.6-terra"
models = ["gpt-5.6-sol", "gpt-5.6-terra", "my-gateway/model"]
api = "responses"
```

Semantics:

- omitted `models` uses the bundled ordered list;
- configured `models` replaces the bundled list for that provider;
- the selected `model` is prepended when absent, including when `models = []`;
- duplicate entries are invalid rather than silently deduplicated;
- `/model <exact-id>` remains accepted even when the ID is not suggested;
- updating the selected model does not mutate the configured suggestion list; and
- auth preserves an existing user list and leaves `models` omitted for new entries.

Each provider list is bounded to 128 entries. Every entry must be non-empty, contain no
NUL, fit the existing model-ID byte limit, and be unique byte-for-byte. No model-name
normalization, alias resolution, family-prefix matching, or case folding occurs.

### Compatibility and Ownership

Existing TOML lacks `models`, so it continues to load with bundled suggestions. The
refresh changes the default selected by new auth entries and by an otherwise
unconfigured provider; it does not rewrite an existing explicit `model`.

The bundled list is project-owned release data. Users own `.nib/config.toml`. T023 owns
live discovery and qualification; it may compare the bundled/configured suggestions
with live catalogs, but it must not edit either automatically.

## Implementation Plan

1. Add failing catalog-schema, provider-completeness, and config-override tests.
2. Add the strict bundled TOML catalog and registry accessors.
3. Replace Rust model/default constants with catalog-backed lookups.
4. Add optional `ProviderEntry.models`, validation, picker precedence, and constructor
   compatibility.
5. Refresh user and technical documentation with current defaults and override syntax.
6. Run focused tests, canonical Task gates, documentation validation, and diff review.

## Rollout Plan

The schema change is additive. New binaries immediately use the refreshed bundled
catalog when a provider entry has no `models` override. Existing selected models remain
unchanged. Operators may copy and modify the documented arrays in project config when
they need private, preview, routed, or newly released model IDs.

Future catalog refreshes are ordinary reviewed repository changes with updated source
URLs and verification dates. Live results may motivate a refresh but never perform it.

## Alternatives Considered

### Keep Rust Constants

Rejected. It preserves the current coupling between data refreshes and source edits and
offers no user-maintained picker list.

### Fetch Live Catalogs for Every Picker

Rejected. It would make ordinary configuration depend on credentials, network access,
provider uptime, pagination, mutable catalogs, and potentially private model metadata.
T023 provides an explicit protected workflow for that responsibility.

### Treat the List as an Allowlist

Rejected. Custom gateways and provider releases routinely expose IDs newer than nib's
release. The selected model remains free-form and provider errors remain authoritative.

### Merge User Entries with Bundled Entries

Rejected. Replacement semantics are predictable and let a user remove irrelevant or
costly suggestions. The selected model is the only automatic addition.

## Risks and Tradeoffs

- **Defaults can still age:** Source-controlled data is not live. Mitigation: record
  sources/dates, keep overrides, and use T023 for drift detection.
- **New defaults can change cost or behavior:** New auth entries may select a newer
  flagship. Mitigation: document defaults and never rewrite existing explicit models.
- **Bundled parse failure:** Invalid embedded TOML would block model lookup. Mitigation:
  strict deterministic tests and all-target validation before release.
- **User confusion about support:** Suggestions may be read as guarantees. Mitigation:
  call them curated suggestions in CLI/docs and keep live qualification separate.
- **Large or hostile lists:** Configuration could consume memory or render control
  characters. Mitigation: existing config file bounds plus per-list count, byte, NUL,
  duplicate, and control-safe display validation.

## Acceptance Criteria

- [x] No provider model suggestion/default list remains hard-coded as a Rust array.
- [x] The bundled TOML catalog is strict, versioned, complete for all registered
  providers, bounded, source-dated, and validated by deterministic tests.
- [x] The curated IDs and defaults match the list in this spec.
- [x] Existing configs without `models` load unchanged and use bundled suggestions.
- [x] A configured `models` list replaces bundled suggestions and preserves order.
- [x] The selected model appears exactly once at the start of the effective list when
  absent, without mutating persisted configuration.
- [x] Custom exact model selection remains accepted outside all suggestion lists.
- [x] Empty entries, oversized entries/lists, NULs, and duplicates fail configuration
  validation with provider-scoped errors.
- [x] Auth preserves existing lists and new entries omit the override.
- [x] `/model` renders the effective provider list and can select by number or exact ID.
- [x] User documentation includes current defaults, override syntax, precedence, and
  the advisory—not live-qualified—support boundary.
- [x] `task check`, `task test`, `task docs:check`, `task check:all-targets`, and
  `git diff --check` pass.

## Affected Areas

- `src/llm/default_models.toml`
- `src/llm/registry.rs`
- `src/config/mod.rs`
- `src/auth.rs` and `src/chat.rs`
- provider registry, config round-trip, validation, auth, and chat tests
- `docs/user/guide.md`, `docs/tech/backend_rust.md`, and `docs/specs/README.md`
- T023 relationship/evidence if live catalog work consumes the new defaults

## Validation Gates

- Bundled catalog schema and provider-completeness tests
- Config compatibility, replacement, ordering, validation, and round-trip tests
- Auth preservation and `/model` effective-list tests
- Focused provider registry/config/CLI tests
- `task check`
- `task test`
- `task docs:check`
- `task check:all-targets`
- `git diff --check`
- Spec-compliance review followed by code-quality/security review

## Open Questions

No blocking design questions remain. Model capabilities and live availability remain
provider/account dependent and are intentionally delegated to T022/T023.

## Implementation and Validation Evidence (2026-08-06)

- The catalog, registry accessors, optional config override, validation, auth
  preservation, free-form chat selection, tests, and documentation are implemented.
- Focused Task-driven validation passed: four registry tests, 41 config tests, the
  exact chat-selection test, and 27 deterministic live-qualification tests; the real
  credential/cost-gated test remained ignored as designed.
- `task docs:check` passed all five documentation invariants,
  `task check:all-targets` passed, and `git diff --check` passed.
- The final `task check` passed installer syntax, formatting, Clippy with warnings
  denied, compilation, all 694 library tests, all 78 CLI tests, and the integration
  suite through the LLM live target. It then failed in the unrelated Linux
  `crashed_supervisor_is_recovered_only_after_pid_namespace_exit` timing fixture
  because the namespace root had already exited. The exact managed-process recovery
  target reproduced the same precondition failure in isolation (three tests passed,
  one failed). T024 therefore remained in Development until the canonical repository
  gates were green; no LLM catalog/config failure remained open.

## Implementation Reconciliation (2026-08-21)

Canonical gates on this Linux revision are green, including the previously failing
Linux `crashed_supervisor_is_recovered_only_after_pid_namespace_exit` fixture:

- `task docs:check` (5/5)
- `task check` (installers, `cargo fmt -- --check`, Clippy `-D warnings`, `cargo check`,
  serial unit and integration suite)
- `task test` (independent serial suite, same revision)
- `task check:all-targets`
- `git diff --check`

No catalog or configuration defect remains. T024 moves to done.

## External References

Verified on 2026-08-06:

- [OpenAI models](https://developers.openai.com/api/docs/models)
- [Anthropic models overview](https://platform.claude.com/docs/en/about-claude/models/overview)
- [Gemini models](https://ai.google.dev/gemini-api/docs/models)
- [xAI Grok 4.5](https://docs.x.ai/developers/grok-4-5)
- [xAI models](https://docs.x.ai/developers/models)
- [OpenRouter models](https://openrouter.ai/models)
- [Meta Model API preview](https://ai.meta.com/blog/introducing-muse-spark-meta-model-api/)
