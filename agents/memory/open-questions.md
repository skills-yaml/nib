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
