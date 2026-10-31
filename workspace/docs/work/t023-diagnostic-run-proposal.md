# T023 bounded diagnostic run proposal

Status: prepared; no live dispatch or spending authorized by this document.
Spec: [T023](../../specs/development/llm-providers/T023_live_llm_provider_model_integration_qualification.md).

## Candidate and order

Pin the reviewed implementation commit after offline verification. Record its exact
40-character SHA, selected matrix fingerprint and fresh catalog hash before any paid
run. Recheck that the protected workflow actually checks out that SHA. Do not run
against an old pending main revision. Inspect pending runs and establish ownership;
never approve stale jobs simply to clear the queue.

1. Read-only readiness: confirm protected reviewers, credential validity/account
   ownership, hard account caps and all nine explicit paid ceilings. The October 26
   audit found Meta's scenario timeout missing. Resolve through the repository's
   approved configuration workflow before Meta qualification; no local infrastructure
   mutation is included in this preparation.
2. Fresh authenticated Anthropic catalog with network acknowledgement. Check exact
   membership for both IDs below. Catalog access makes no generation requests.
3. First paid run: provider `anthropic`, mode `selected`, exact model
   `claude-opus-5`, using the unchanged `nib-llm-core-v1` matrix. This addresses the
   historical failure; do not substitute the newer registry default.
4. Inspect and preserve the result before another run. A refusal is still a failure;
   no alternate prompt retry or silent model substitution is permitted.
5. Only if the first result supports proceeding and remaining authority/budget allows:
   provider `anthropic`, mode `canary`, registry default `claude-opus-5-5`. Recheck the
   default at the pinned revision; drift requires a revised proposal.

## Proposed ceilings (require explicit user spending approval)

Each of these two separate runs is limited to one model and one Anthropic Messages
transport. Complete and streamed text consume one logical request each; single-tool
continuation consumes two; advertised parallel-tool continuation adds two. Therefore
allow at most six logical requests and eighteen actual attempts per run. The planner
must reject a larger denominator before generation, not silently omit scenarios.

| Setting | Per-run ceiling |
| --- | --- |
| Logical requests | 6 |
| Actual attempts | 18 |
| Output tokens per request | 4096 |
| Total output tokens | 24576 |
| Concurrency | 1 |
| Scenario duration | 120 seconds |
| Provider duration | 360 seconds |
| Whole-run duration | 600 seconds |
| Harness cost ceiling | USD 1 |

Proposed combined spending authorization: **USD 2 maximum across both runs**, with
at most USD 1 allocated to either. This is a proposed limit, not a price estimate or
approval. The run ledger does not enforce a combined cross-run account cap. Confirm
provider-side hard caps and reconcile actual charges/remaining balance between runs;
unknown cost is not zero. If the provider cannot enforce the approved hard cap for
unpriced models, do not run. Do not raise ceilings automatically if planning rejects
them. A fresh catalog/budget review may require a revised proposal.

The existing protected workflow exposes provider and mode, not budget overrides or
a dry-run CLI. These exact reviewed limits must be supplied by its approved protected
configuration process; current USD 50 settings are unsuitable for this proposal.
Do not dispatch a paid job using those old limits. No scheduled paid mode is enabled.

## Evidence preservation and diagnosis

Hosted retention remains seven days under the current CI policy. Before expiry,
inspect JSON and Markdown reports and record a durable sanitized summary in T023:
exact source revision, run ID, time, mode, approved public model IDs, catalog/matrix
fingerprints, per-scenario outcome, tool failure phase, fixed error class, numeric
HTTP status if present, attempts/usage/cost completeness and budget blockers. Include
artifact digests for identity, but never mistake digests for recoverable artifacts.
Keep raw provider bodies, prompts/nonces, credentials, private native content and
private model identifiers out of tracked records. Use the existing privacy-scanned
reports; download only to an approved local results location for inspection.

Classify failures before editing: initial request rejection, tool-result correlation,
continuation rejection/refusal, or harness mismatch. Preserve refusal evidence.
Diagnose with local synthetic fixtures; production fixes need a fresh patch reservation
and independent review. A successful run is evidence only for its exact revision and
model/catalog snapshot; do not carry it across later adapter changes.

## Delivery gates

Run `task test:llm-live:offline` for diagnostic/report changes. Production adapter
changes, if later justified, additionally select `task test:llm-conformance` before
consumer tests. Reconcile records, obtain independent exact-candidate review, resolve
findings, freeze, then run `task verify`, `task docs:check` and `task versions:check`.
Use a reviewed PR into development and verify the actual combined revision. This
preparation does not satisfy live acceptance or move T023 out of development.
