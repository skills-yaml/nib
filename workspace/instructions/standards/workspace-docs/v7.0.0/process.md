# Workspace Process and Governance (`workspace-docs@7.0.0`)

The [SDLC contract](./sdlc.md) defines lifecycle, review, modular quality gates,
acceptance, safe delivery authority, completion, and separate release tracking.
Read it before adopting or generating project workflow guidance.

Keep root policy and tokens in AGENTS.md and DESIGN.md, instructions under
workspace/instructions/, feature-grouped specs under workspace/specs/,
human documentation under workspace/docs/, company context under
workspace/company/, and durable memory under workspace/agents/memory/.

Create a complete development specification before non-trivial implementation.
Use affected modules for quick feedback during coherent changes and review
fixes. Resolve all review findings, freeze the candidate, and run full required
verification. Confirm shared-test integration before test and verified main
merge before done. Record blocked reason, kind, previous state, and resume
condition; do not fabricate transitions or acceptance evidence.

Track deployment and publication separately. Keep applied versions distinct
from published releases. Reconcile the spec catalog, version reservations,
documentation, and memory at handoff and completion.

Concurrent mutating agents use dedicated detached linked worktrees, bounded
possibly overlapping scopes, independent exact-revision review, and serialized
integration of the combined revision. Keep the primary checkout coordination-only
and preserve interrupted work. Ordinary task branches are authorized by the
user's task request, subject to protections. Runtime completion is operational
history, not proof of shared-test integration, main merge, or publication.
