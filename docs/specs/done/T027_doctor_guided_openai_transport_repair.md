# T027: Doctor-Guided OpenAI Transport Repair

**Status:** Done

**Related:**
[T007: Configuration Schema Alignment and Doctor Validation](../development/T007_configuration_schema_alignment_and_nib_doctor_validation.md),
[T021: OpenAI-Compatible Reasoning and Tool Transport Compatibility](../development/T021_openai_compatible_reasoning_and_tool_transport_compatibility.md),
[T024: Configurable Provider Model Catalog and Curated Defaults](T024_configurable_provider_model_catalog_and_curated_defaults.md), and
[T026: Actionable, Redaction-Safe LLM Failure Reporting](../development/T026_actionable_redaction_safe_llm_failure_reporting.md)

## Summary

Teach `nib doctor` to recognize the locally observable OpenAI transport configuration
behind the production planning failure reported on 2026-08-17 and repair it only after
an explicit `nib doctor --fix` request. The repair changes a canonical OpenAI
Chat Completions configuration with provider-default or non-`none` reasoning to the
Responses API, then reruns the complete doctor suite against the committed
configuration.

The repair is configuration-only. It does not retry a failed model request, infer
capabilities from a model-name prefix, alter the selected model or credentials, or
change custom OpenAI-compatible gateways.

## Incident and Problem

An installed production binary selected `openai` with `gpt-5.6-luna` and failed during
planning with an HTTP 400 from Chat Completions. nib always supplies the `submit_plan`
function tool during planning. Existing OpenAI entries without an explicit `api` retain
the legacy `chat_completions` default, while current OpenAI guidance recommends
Responses for GPT-5.6 reasoning, tool-calling, and multi-turn workflows.

T021 made the effective transport visible and made new OpenAI auth entries select
Responses, but ordinary doctor output only emits a warning and cannot perform the
documented migration. Re-running `nib auth` also intentionally preserves the transport
of an existing entry. The operator therefore receives a generic provider rejection
without one safe command that diagnoses and repairs the local cause.

## Decision

Add an explicit `--fix` flag to `nib doctor` and one narrowly allowlisted repair.

### Diagnosis Eligibility

The OpenAI transport issue is repairable only when all of these are true:

- the active registered provider is exactly `openai`;
- its resolved endpoint is the canonical `https://api.openai.com/v1` service, including
  the exact canonical root or Chat Completions endpoint when explicitly configured;
- the resolved API mode is `chat_completions`; and
- reasoning is `provider_default` or an explicit value other than `none`.

Ordinary `nib doctor` reports this as a failed runtime-readiness check, identifies the
resolved Chat Completions path, explains that nib's planning/runtime loops require
function tools, and names `nib doctor --fix` as the deterministic local action. It does
not make a provider request or claim to recover the provider's omitted message.

An explicit `chat_completions` plus `reasoning_effort = "none"` is treated as an
acknowledged operator choice and retains T021's model-dependent warning. A custom
host, port, path, query, fragment, or credential-bearing URL is never repairable.
Existing strict endpoint validation remains authoritative.

### Repair Contract

`nib doctor --fix`:

1. loads and validates the current configuration under the existing identity-bound
   configuration lock;
2. re-evaluates the exact repair predicate inside `update_nib_config_conditionally` so
   a concurrent change cannot be overwritten;
3. sets only `llm.providers.openai.api = "responses"` and, when the configured URL is
   the exact canonical Chat Completions endpoint, normalizes it to the canonical
   `https://api.openai.com/v1` root;
4. preserves the selected model, model suggestions, credentials, credential rotation,
   reasoning effort, active provider, and every unrelated configuration field;
5. commits through the existing atomic revisioned writer; and
6. reloads the committed configuration and runs the full doctor suite.

If no eligible issue exists, `--fix` performs no write and reports that no eligible
repair was needed. A second invocation is idempotent and does not increment the
configuration revision. If the predicate changes before the locked update, the repair
fails closed without changing configuration.

### Compatibility Boundary

This explicitly invoked repair does not violate T021's ban on automatic semantic
fallback: no failed request is retried and ordinary runtime startup never rewrites
configuration. The user chooses a durable migration before starting a new agent turn.
T026 continues to own typed provider-error propagation and user-facing incident
classification; T027 does not parse existing error strings or expose provider bodies.

## Scope

- `nib doctor --fix` CLI parsing and help text.
- Pure, credential-free detection of the repairable canonical OpenAI configuration.
- Atomic, concurrent-change-safe transport repair through the existing config writer.
- Doctor output, exit status, idempotency, redaction, and regression tests.
- User documentation and cross-spec reconciliation.

## Non-Goals

- Automatically repairing during chat, run, startup, auth, or an ordinary doctor run.
- Retrying the rejected request or silently changing semantics mid-run.
- Changing models, reasoning effort, credentials, model lists, or provider selection.
- Repairing xAI, OpenRouter, Meta, Anthropic, Gemini, or custom gateways.
- Fetching model catalogs or issuing any live/paid provider request.
- Parsing provider messages or legacy string-only session errors.
- Implementing T026's typed LLM error and presentation work.

## Affected Areas

- `src/main.rs`
- `src/doctor.rs`
- `src/config/mod.rs` only if a reusable typed repair helper is required
- `Taskfile.yml` and `docs/tech/task.md`
- `tests/doctor_cli.rs`
- `docs/user/guide.md`
- `docs/specs/README.md`
- T007 and T021 compatibility/evidence notes

The workload/session model, LLM request transport, agent loop, tool authorization, and
provider catalog are not modified.

## Implementation Plan

1. Add failing unit tests for eligible implicit/explicit canonical configurations,
   the explicit-`none` exception, custom gateway exclusion, canonical endpoint
   normalization, field preservation, and idempotency.
2. Add failing CLI tests for diagnosis exit status and `doctor --fix` repair/recheck.
3. Implement the doctor option, pure predicate, locked mutation, reload, and bounded
   redaction-safe output.
4. Update user and owning-spec documentation.
5. Run focused tests, canonical Task gates, diff review, and separate spec-compliance
   and code-quality/security self-reviews.

## Acceptance Criteria

- [x] `nib doctor` identifies canonical active OpenAI Chat Completions with
      provider-default or non-`none` reasoning as a failed readiness check and names
      `nib doctor --fix` without network I/O.
- [x] The diagnosis does not depend on model-name prefixes or provider-controlled
      error text.
- [x] `nib doctor --fix` atomically changes the eligible provider to Responses and
      reruns all doctor checks against the committed configuration.
- [x] The repair preserves model, model list, credentials, reasoning effort, active
      provider, and all unrelated configuration fields.
- [x] An exact canonical full Chat endpoint is normalized safely; custom endpoints and
      unsafe URLs are never rewritten.
- [x] Explicit Chat Completions with `reasoning_effort = "none"` is not changed.
- [x] No eligible issue produces no write; repeated `--fix` is idempotent and does not
      increment the revision.
- [x] Concurrent configuration drift is re-evaluated under the config lock and cannot
      be overwritten by a stale repair decision.
- [x] Output contains no credential, arbitrary URL, query, provider body, prompt, or
      control sequence.
- [x] Existing healthy, invalid-config, MCP, credential, and transport diagnostics
      retain their behavior.
- [x] User documentation explains diagnosis, repair, exclusions, and the absence of
      request retries or live probes.
- [x] `task fmt`, `task docs:check`, `task check:all-targets`, `task check`, `task test`,
      and `git diff --check` pass.

## Validation Gates

- Doctor unit tests for detection, repair, preservation, exclusion, and idempotency.
- Doctor CLI child-process tests for exit codes and persisted TOML behavior.
- Existing config revision/atomicity and endpoint-validation tests.
- `task fmt`
- `task docs:check`
- `task check:all-targets`
- `task check`
- `task test`
- `git diff --check`

## Risks and Mitigations

- **Unexpected semantic migration:** The repair requires an explicit `--fix`, applies
  only to canonical OpenAI, and does not run inside a failed request.
- **Custom gateway corruption:** Eligibility uses exact parsed canonical endpoint
  identity and excludes every custom endpoint.
- **Concurrent overwrite:** The predicate and mutation execute inside the existing
  identity-bound revisioned config update.
- **Credential leakage:** Detection and output use only registered provider, API mode,
  endpoint path, reasoning label, and fixed local guidance; tests use sentinel secrets.
- **Cross-spec policy drift:** T021 is updated to distinguish forbidden automatic
  fallback from this explicit preflight migration, while T026 retains error ownership.

## External Reference

- [OpenAI GPT-5.6 model guidance](https://developers.openai.com/api/docs/guides/latest-model)
  recommends Responses for reasoning, tool-calling, and multi-turn workflows and
  recommends choosing reasoning effort intentionally.

## Validation Evidence (2026-08-17)

- Failing-first `task test:doctor` could not compile before the doctor option, repair
  predicate, and mutation entrypoint existed.
- `task test:doctor`: 14 doctor unit tests and 5 child-process CLI tests passed,
  covering diagnosis, canonical endpoint normalization, preservation, redaction,
  explicit-`none` and custom-gateway exclusions, and revision-stable idempotency.
- `task docs:check`: all 5 documentation integrity tests passed.
- `task check:all-targets`: all targets and features compiled.
- `task check`: installer validation, formatting, Clippy with warnings denied,
  compilation, 733 library tests, 51 binary tests, all integration groups, and doc
  tests passed. The explicitly gated paid live-provider test remained ignored.
- `task test`: the complete test matrix passed independently with the same single
  explicitly gated live-provider test ignored.
- `git diff --check`: passed.

## Final Review

The spec-compliance review found every acceptance criterion implemented without
expanding into runtime fallback or T026 error-presentation work. The separate
quality/security review found no unresolved findings: the allowlist uses parsed
canonical endpoint identity, the mutation evaluates inside the retained config lock,
no-op decisions do not write or advance revisions, custom gateways fail closed, and
all output is fixed or routed through existing bounded redacted diagnostics. No live
provider request was made by the feature or its validation.
