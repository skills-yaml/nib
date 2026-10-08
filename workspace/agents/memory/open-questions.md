# Open Questions

## 2026-07-15 - Future remote MCP scope

- Type: open-question
- Source: implementation audit
- Confidence: high
- Review: none
- Supersedes: none

Content:

- If remote MCP transport is required, should HTTP/SSE and OAuth be introduced under
  a separate versioned spec rather than expanding the stdio v1 contract?

## 2026-07-16 - Platform ownership gates

- Type: open-question
- Source: FT-015 final ownership review
- Confidence: high
- Review: required before FT-015 completion
- Supersedes: none

Content:

- Where will Windows Job Object, reparse-point, and handle-deletion runtime gates be
  executed, and where will the macOS runtime gates run? Linux validation and
  cross-compilation cannot close those gates.
- What OS-protected broker, ACL boundary, or inherited capability will hold cleanup
  proof state outside an untrusted Windows/macOS worker, and what independent macOS
  owner will recover a crashed supervisor? Production delegation remains disabled on
  those platforms until both questions have verified answers.

## 2026-09-02 - Live LLM qualification authority

- Type: open-question
- Source: T023 closure review
- Confidence: high
- Review: required before any paid or credentialed live run
- Supersedes: none

Content:

- Which owner-approved, current exact OpenRouter model IDs should form the initial
  allowlist, with rationale, owner, review/expiry dates, and cost ceilings?
- Which dedicated provider credentials, per-provider budgets, and protected GitHub
  environment approvals may be used for catalog, canary, selected, and full runs?
- T023 remains in development until those authorities exist and one exact revision has
  complete, privacy-reviewed evidence across all six provider groups.

## 2026-09-02 - Native platform execution location resolved

- Type: open-question resolution
- Source: PR #25 exact hosted CI
- Confidence: high
- Review: 2026-09-02
- Supersedes: 2026-07-16 platform ownership gates, first question only

Content:

GitHub-hosted Ubuntu 24.04, macOS 15, and Windows runners executed the native runtime,
filesystem, process, release-binary, and terminal gates in run `33683995100`; that
location question is resolved. The protected cleanup-authority *design* is selected
in done spec FT-020 (Windows protected Job DACL + non-inheritable owner handle;
macOS LaunchDaemon reaper preflight). Production delegation remains Linux+bwrap
only. Windows and macOS `production()` stay fail-closed until a later native
qualification record enables them.

## 2026-09-25 - T023 live credentials still owner-gated

- Type: open-question
- Source: T023 2026-09-25 external authority audit
- Confidence: high
- Review: 2026-09-25 live Task and GitHub environment inventory

Content:

Dedicated low-privilege keys for OpenAI, Anthropic, Gemini, xAI, Meta, and
OpenRouter, plus `NIB_LIVE_TESTS`/`NIB_LIVE_ACK_COSTS`, Meta catalog root,
OpenRouter exact-ID approval, and `llm-live-*` GitHub environment secrets, are
still missing. T023 cannot move to done until one exact revision has a complete
privacy-reviewed catalog/canary/selected/full pass.

## 2026-10-01 - T023 authority snapshots superseded by live evidence

- Type: open-question resolution
- Source: T023 2026-09-25 catalog and canary follow-up; user spec review
- Confidence: high
- Review: repository evidence review on 2026-10-01; hosted reports not revalidated
- Supersedes: 2026-09-25 T023 live credentials still owner-gated; missing-authority assertions in 2026-09-02 Live LLM qualification authority

Content:

T023 records a protected six-provider catalog pass, reviewed OpenRouter entries,
and retained paid canary reports. Earlier statements that all credentials and
approvals are absent describe historical snapshots. Current protected approval
and spend limits still apply to each new run. Canary continuation failures,
including Anthropic's final refusal, remain unresolved; selected and full
exact-revision qualification remain open. Retain the current Anthropic default
and investigate the continuation failure under the user-approved direction.

## 2026-10-08 - Catalog and sandbox release scope

- Type: open-question
- Source: user merge request, latest atomic shared reservation and independent review
- Confidence: high
- Review: 2026-10-08 exact delivery and shared-version review
- Supersedes: none

Content:

The catalog/Anthropic delivery candidate in [PR #51](https://github.com/skills-yaml/nib/pull/51)
uses the previously applied 0.3.2 patch. T080's in-progress sandbox work subsequently
joined `nib-catalog-refresh` and raised its atomic shared reservation to minor 0.4.0.
Resolve whether delivery combines T080 or separates the catalog/Anthropic changes,
then reconcile ownership, release membership and native versions before integration.
The current catalog candidate has independent approval, complete clean local gates
and Linux/macOS CI; these do not override the changed reservation or establish a merge.
T080 is not yet handed off, and T023 live qualification remains unresolved.
