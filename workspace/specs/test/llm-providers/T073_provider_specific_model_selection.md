# T073: Provider-Specific Model Selection

State: test
Primary Feature: llm-providers

Status rationale: Confirmed shared development integration at `81c37a1908ec1032809e97cdf0b0b39d5253603b`
was re-read after the successful synchronization push on 2026-10-08T17:02:18Z.
Independent exact-candidate review and task verify/docs:check/check:all-targets
passed. Shared release is minor 0.4.0; T073 supersedes earlier retention
contracts. Main delivery, publication and T023 live qualification remain
separate.

Related: [T072](T072_latest_generation_model_catalog.md),
[T069](T069_current_model_catalog_refresh.md) and
[T023](../../development/llm-providers/T023_live_llm_provider_model_integration_qualification.md).

## Problem and Scope

The user wants all OpenAI 5.6-and-newer models, the latest three Grok models,
and only `gemini-3.8-flash`, `gemini-3.7-flash`, `gemini-3.6-flash` and
`gemini-3.5-flash`, checking for newer ordinary Flash releases. Apply the same
selection rules to the corresponding OpenRouter vendors. This supersedes
T072's retention contract for those providers; other curated families keep
their latest-major-generation selections.

## Proposed Behavior

Keep the four existing GPT 6 IDs and add direct `gpt-5.6-sol`,
`gpt-5.6-terra` and `gpt-5.6-luna`. These seven canonical IDs are the documented
general direct text/tool models at 5.6 or newer. The user explicitly selected
**general models only**: exclude restricted Cyber and all Pro variants, including
router Pro IDs. Do not invent direct IDs or add duplicate aliases, snapshots or
batch entries.

OpenRouter retains the same seven canonical base OpenAI routes, adding its three
GPT 5.6 base IDs. Route presence must be checked independently from direct
OpenAI availability. Keep every default and retained relative order, appending
new suggestions.

Use only `grok-4.7`, `grok-4.6` and `grok-4.5`, with the same three `x-ai/`
routes. They are the newest three general model releases; Build predates 4.5
and is excluded. Keep only the user's four Gemini IDs and matching `google/`
routes. Public model and release-note inspection on 2026-10-08 finds no newer
ordinary text/tool Flash release than 3.8. Image, Live, TTS, Flash-Lite and Pro
are outside the requested Gemini list.

The user explicitly answered both clarifications: three newest general Grok
releases, and general OpenAI models only.

Preserve explicit selections and configured replacement/empty lists. No adapter,
workload, persistence, delegation, endpoint, governed instruction or protected
qualification change. Catalog presence is not authenticated qualification.

## Affected Areas

Bundled TOML, registry/configuration tests, test-only offline matrix accounting,
guide, spec catalog, shared release membership and durable preference memory.

## Implementation Plan

1. Recheck the applied shared 0.3.2 reservation and register this contract before
   implementation; preserve earlier candidate and source history.
2. Update exact provider lists and route-specific source dates, retaining every
   default and unrelated family. Add observable selection/override regressions.
3. Adjust only synthetic offline accounting; keep approved qualification IDs
   and production budgets unchanged.
4. Reconcile guide and superseding memory, run focused Task checks, obtain
   independent two-stage exact-candidate review, freeze and run all final gates.

## Acceptance Criteria

- [x] AC-1: Exactly 57 catalog entries: OpenAI 7, Anthropic 6, Google 4, Grok 3,
  OpenRouter 33, Meta 3 and Mock 1. Six suggestions are added and thirteen
  removed relative to T072. Every default and retained relative order survives.
- [x] AC-2: OpenAI and router OpenAI include exactly the seven verified general
  5.6+ canonical models. Cyber and Pro suggestions are excluded. Gemini and
  Grok exact ordered lists match the requested/current selection. No older,
  invented direct Pro, duplicate alias or batch suggestions appear.
- [x] AC-3: Explicit older selections, custom selections and replacement/empty
  lists remain authoritative. The synthetic offline fixture accounts for 56
  network entries, 40 provider transport profiles, 160 requests, 480 maximum
  attempts and 10,240 output tokens; its four approved routes remain unchanged.
- [x] AC-4: Guide documents provider-specific selection, general-only scope
  and the latest-release check. Version/memory/catalog records are reconciled;
  independent exact review and required local gates pass.

## Validation Gates

Focused registry/configuration tests through Task, `task test:llm-live:offline`,
`task docs:check`, `task versions:check` and `task check` during iteration.
Final reviewed/frozen candidate: `task verify`, `task docs:check`,
`task check:all-targets` and `git diff --check`. Cargo access stays serial with
bounded build settings. No paid calls or credential reads.

## Source Evidence

Inspected 2026-10-08:
[OpenAI complete model catalog](https://developers.openai.com/api/docs/models.md),
the seven general canonical model pages,
[Google models](https://ai.google.dev/gemini-api/docs/models) and
[release notes](https://ai.google.dev/gemini-api/docs/changelog),
[xAI models](https://docs.x.ai/developers/models),
[Grok 4.6](https://docs.x.ai/developers/models/grok-4.6),
[Grok 4.5](https://docs.x.ai/developers/models/grok-4.5) and
[OpenRouter public metadata](https://openrouter.ai/api/v1/models).
Direct documentation lists no newer numbered ordinary Flash or Grok release.
Router metadata confirms canonical exact IDs, text output, tool parameters and
release order. Cyber and router Pro variants were inspected but excluded by explicit user
selection. Direct Pro documentation pages return 404. The official
[Pro mode guide](https://developers.openai.com/api/docs/guides/latest-model#pro-mode)
uses base model IDs with an execution mode; this catalog change does not add such
a direct configuration setting. Use Responses for GPT-6.1 Sol tool calling;
GPT-6 Sol/Luna Chat Completions tool use requires reasoning_effort=none.
Availability remains route and account specific.
Other providers preserve T072's 2026-10-07 inspection dates.

## Risks and Rollback

Do not reintroduce OpenAI Cyber/Pro, Grok Build or Gemini Pro/Lite suggestions contrary
to the clarified scope. Direct transport support remains model-specific; document
the Responses requirements for the retained models without implying qualification.
Keep the advisory picker separate from protected qualification allowlists and
budgets. Restore only the T072 selection-policy inputs to roll back; preserve
the Anthropic repair, user changes, historical records and shared version.

## Version Impact

| Component | Impact | Release | Rationale |
| --- | --- | --- | --- |
| nib | patch | nib-catalog-refresh | Compatible advisory catalog-policy revision sharing the unreleased applied 0.3.2 patch from 0.3.1; atomic reservation rechecked before implementation, no additional bump. Superseded at combined synchronization by the shared minor 0.4.0 target with T080; this compatible member remains patch impact and applies no additional bump. |

## Memory Impact

Status: updated
Rationale: Appended the revised preference to
[workspace/agents/memory/preferences.md](../../../agents/memory/preferences.md)
and its corresponding
[workspace/agents/memory/changelog.md](../../../agents/memory/changelog.md) record.

## Local Evidence

Native catalog comparison confirms six additions, thirteen removals, 57
entries, unchanged defaults and retained order, and byte-identical unrelated
provider entries. All thirty-three retained router IDs have text output and tools in
fresh public metadata. This supports AC-1.

Renewed checks for the confirmed general-only scope pass: eight registry tests,
48 configuration tests, 71 offline qualification tests, five documentation
tests, and `task check` (including version/workspace checks and strict Clippy).
These support AC-2 and AC-3. Prior adapter/version/user edits,
historical spec inputs and protected qualification fixtures remain unchanged.

Independent spec-compliance and correctness review approved the exact 29-input
candidate without findings. The frozen manifest SHA-256 was
`d731d20d4cc5875d42a34385cc0c428c2ac05e5491b841b565abbfc58772eb9e`;
all input hashes matched before and after serial `task verify`,
`task docs:check`, `task check:all-targets` and `git diff --check`, which passed.
The full suite includes 1,350 library tests, 106 CLI tests and 71 offline
qualification tests, plus the remaining integration targets. These support
AC-4. The shared applied 0.3.2 reservation was rechecked at handoff; no second
bump is needed. Final evidence-record edits are limited to this spec and the
status catalog, with renewed independent review and affected documentation,
workspace and static checks; the other 27 reviewed inputs retain their hashes.

No shared integration, main delivery or publication is observed; state remains
development.

## Shared Release Reconciliation (2026-10-08)

T080 phase 1 is already integrated into remote development at `045e0e2`.
The user-requested synchronization combines that work with the catalog and
Anthropic changes, retaining shared release `nib-catalog-refresh` at minor
0.4.0 from 0.3.1. The release ledger includes all five catalog specs and T080;
Cargo.toml, Cargo.lock and skills.yaml retain the same applied 0.4.0 target.
Earlier 0.3.2 statements record the superseded pre-integration reservation.
Combined exact review and native gates are pending; no catalog shared
integration, main delivery, publication or live-provider qualification is
inferred from this local reconciliation.

## Shared Integration Evidence (2026-10-08)

Outcome: passed

The synchronization push advanced remote `development` from `045e0e2` to
`81c37a1908ec1032809e97cdf0b0b39d5253603b`. The remote ref was re-read at
2026-10-08T17:02:18Z and matched that exact reviewed combined revision.
Independent spec-compliance and correctness review approved the candidate
after the two documentation findings were fixed. Fresh serial `task verify`,
`task docs:check`, `task check:all-targets` and `git diff --check` passed
before pushing. Verification used debug-symbol-free build profiles after the
earlier filesystem-capacity failure; test assertions and coverage of the
complete non-ignored suite were preserved.

This observed shared integration establishes test state under minor 0.4.0.
T073 governs the final catalog selection; earlier retention proposals remain
preserved as superseded history. This record does not establish main delivery,
publication, paid qualification or completion of later T080 phases. The
subsequent lifecycle-record change renews independent review and affected
documentation, governance and static checks; runtime inputs remain unchanged.
