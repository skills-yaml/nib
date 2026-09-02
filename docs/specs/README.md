# Specs

nib follows `workspace-docs@1.2.0` for spec state management.

Canonical state directories:

- `backlog/`: accepted ideas that are not actively being implemented.
- `development/`: active work with scope, acceptance criteria, affected areas, implementation plan, validation gates, and risks.
- `done/`: completed work with final behavior and validation recorded.

Allowed transitions:

- `backlog -> development`
- `development -> done`

Legacy `docs/specs/feature/` and `docs/specs/task/` paths are empty reference
directories. `docs/specs/foundation/product.md` remains the product foundation;
lifecycle-managed work uses the canonical state directories above.

## Implementation Audit Status

The 2026-07-15 audit inspected all 27 specs that had claimed completion. Unsupported
claims moved through `development/`; missing feasible behavior was implemented and
historical proposal text was reconciled. Repeated compliance and quality/security
reviews reopened owning specs whenever completion claims exceeded the implementation.
The current lifecycle is **29 done, 16 development, and 1 backlog**.

T003, T004, and T007 retain their Windows/macOS runtime gates; their implemented Unix
contract proves exact namespace detachment and reports ambiguous residual physical
cleanup within the documented non-malicious-same-UID boundary. T006 remains pending
separate hosted Windows evidence reconciliation. T010 completed its exact-current
development and production release evidence. FT-015 retains Windows worktree
cleanup/runtime validation and the FT-017
platform authority boundary;
managed Git preflight and Unix namespace-detachment criteria are locally complete under
the same threat boundary. T020 and FT-016 have complete local stdio MCP lifecycle,
cancellation, redaction, and metadata coverage; T020 retains Windows Job Object and
macOS runtime evidence, FT-016 retains Windows runtime evidence, and both inherit
FT-015's remaining platform limits. FT-017 owns the stronger abrupt-owner
descendant-process containment contract: Linux production proof is implemented, while
Windows/macOS production delegation fails closed pending protected cleanup authority.
T021 is in development for explicit OpenAI-compatible API-mode and reasoning
configuration plus Responses function-tool support.
T022 is in development for a typed provider-neutral LLM contract, distinct provider
adapters composed from shared wire codecs, native correlated tool continuation, safe
terminal/error normalization, and provider conformance gates.
T023 is in development for credential-gated live qualification across every account-visible
direct-provider model and a reviewed exact-ID OpenRouter allowlist.
T024 completed source-controlled curated provider model defaults and
user-configurable per-provider picker lists.
T025 completed interactive chat/TUI capability parity. T026 is in development for
actionable, redaction-safe LLM failure propagation and native console/TUI presentation.
T027 completed a doctor-diagnosed, explicitly invoked repair of canonical OpenAI
tool/reasoning workloads that still use Chat Completions.
T028 completed a current-session-first TUI, explicit preview-and-confirm session
switching, persisted-session reloading, and slash-command completion.
T029 is in development for explicit, verified switching between production and
development self-update channels from an already managed installation.
T030 completed one canonical interactive launcher that prefers the TUI, retains an
explicit plain-mode fallback, preserves `nib run`, and keeps the existing interactive
commands as compatibility entry points during migration.
FT-019 is in development as the umbrella interaction contract for a Codex-inspired
composer, transcript, live steering and queueing, status and permission visibility,
inspectable activity, and full semantic parity between TUI and plain/chat modes. T031
completed the first child slice (shared model, ledger TUI, queue-only live input,
capability-gated compact/ps/stop/steer).
T032 completed explicit compaction and session background commands. T033 completed
exact-run live steering. T034 remains in development for native terminal qualification.
T035 is in development to separate fast static feedback from one complete local
verification aggregate without weakening the serial test gate.
FT-018 completed the verified self-update command, bounded update notices, Windows
in-use replacement, and four-platform production rollout.

Each audited file has an `Implementation Reconciliation (2026-07-15)` section that
supersedes older proposal text. Later dated remediation sections and their unchecked
criteria supersede that reconciliation snapshot wherever they identify additional
work or narrower guarantees.

### Documentation and task specs

- [D001: Workspace Docs Adoption](done/D001_workspace_docs_adoption_and_foundational_spec_alignment.md)
- [T001: Core Agent Tools](done/T001_implement_core_agent_tools.md)
- [T002: Runtime and Orchestration](done/T002_agent_framework_runtime_and_orchestration_engine.md)
- [T003: Context and Compression](development/T003_context_engine_with_dynamic_compression_and_session_management.md)
- [T004: Profiles, Memory, and Daemons](development/T004_profiles_discrete_memory_store_and_maintenance_daemons.md)
- [T005: Runtime State Machine](done/T005_full_runtime_state_machine_and_lifecycle.md)
- [T006: Skills and MCP Gateway](development/T006_enhanced_skills_framework_and_mcp_gateway_alignment.md)
- [T007: Configuration and Doctor](development/T007_configuration_schema_alignment_and_nib_doctor_validation.md)
- [T008: End-to-End Validation](done/T008_end_to_end_tests_and_sequence_diagram_validation.md)
- [T009: Rust Module Layout and TOML Config](done/T009_rust_module_layout_and_toml_config.md)
- [T010: Release Process](done/T010_release_process.md)
- [T011: End-User Documentation](done/T011_end_user_documentation.md)
- [T012: Toolset Expansion](done/T012_toolset_expansion.md)
- [T018: ratatui Approval Flow](done/T018_ratatui_tui_approval.md)
- [T020: MCP Client Integration](development/T020_mcp_client_integration.md)
- [T021: OpenAI-Compatible Reasoning and Tool Transport Compatibility](development/T021_openai_compatible_reasoning_and_tool_transport_compatibility.md)
- [T022: Provider-Neutral LLM Contract and Adapter Conformance](development/T022_provider_neutral_llm_contract_and_adapter_conformance.md)
- [T023: Live LLM Provider and Model Integration Qualification](development/T023_live_llm_provider_model_integration_qualification.md)
- [T024: Configurable Provider Model Catalog and Curated Defaults](done/T024_configurable_provider_model_catalog_and_curated_defaults.md)
- [T025: Interactive Chat and TUI Capability Parity](done/T025_interactive_chat_tui_capability_parity.md)
- [T026: Actionable, Redaction-Safe LLM Failure Reporting](development/T026_actionable_redaction_safe_llm_failure_reporting.md)
- [T027: Doctor-Guided OpenAI Transport Repair](done/T027_doctor_guided_openai_transport_repair.md)
- [T028: Current-Session-First TUI and Slash-Command Completion](done/T028_current_session_first_tui_and_slash_command_completion.md)
- [T029: Explicit Self-Update Channel Switching](development/T029_explicit_self_update_channel_switching.md)
- [T030: Unified Interactive CLI and Plain-Mode Fallback](done/T030_unified_interactive_cli_and_plain_mode_fallback.md)
- [T031: FT-019 Interaction Model, Ledger TUI, and Queue-Only Live Input](done/T031_ft019_interaction_model_and_ledger_tui.md)
- [T032: FT-019 Explicit Compaction and Session Background Commands](done/T032_ft019_explicit_compaction_and_session_background_commands.md)
- [T033: FT-019 Exact-Run Live Steering](done/T033_ft019_exact_run_live_steering.md)
- [T034: FT-019 Native Terminal Qualification](development/T034_ft019_native_terminal_qualification.md)
- [T035: Fast Incremental Check and Single Full Verification](development/T035_fast_incremental_check_and_single_full_verification.md)

### Feature specs

- [FT-001: Basic Agent Tools](done/ft_001_basic_agent_tools.md)
- [FT-002: Base Architecture](done/ft_002_base_architecture.md)
- [FT-003: Hybrid Sandboxing](done/ft_003_adopt_codex_sandboxing.md)
- [FT-004: LLM Integration and Agent Loop](done/ft_004_llm_integration_and_agent_loop.md)
- [FT-005: Pure Rust Core Migration](done/ft_005_pure_rust_core_migration.md)
- [FT-006: Skills Management](done/ft_006_skills_management.md)
- [FT-011: LLM Streaming and TUI](done/ft_011_llm_streaming_and_tui.md)
- [FT-012: Richer Planner](done/ft_012_richer_planner.md)
- [FT-013: Advanced Session Memory](done/ft_013_advanced_session_memory.md)
- [FT-014: Smart Approval Classifier](done/ft_014_smart_approval_classifier.md)
- [FT-015: Subagent Delegation](development/ft_015_subagent_delegation.md)
- [FT-016: MCP Server Exposure](development/ft_016_mcp_server_exposure.md)
- [FT-017: Managed Process Supervisor](development/ft_017_managed_process_supervisor.md)
- [FT-018: Self-Update Command and Update Availability Notices](done/ft_018_self_update_and_update_notifications.md)
- [FT-019: Codex-Inspired Chat and TUI Interactions](development/ft_019_codex_inspired_chat_and_tui_interactions.md)
- [FT-020: Protected Non-Linux Production Delegation Authority](backlog/ft_020_protected_non_linux_production_delegation_authority.md)

## Current Local Validation (2026-09-02)

- `task verify` passed its fast static phase once and the complete unchanged serial
  suite: 1,061 library tests, 86 CLI tests, and 254 integration tests; only the explicit
  live-provider and release-qualification tests were ignored by the normal suite.
- `task docs:check` passed all five documentation invariants. Host and Windows MSVC
  `task check:all-targets` both reached and checked nib; the Windows test graph emitted
  only existing target-specific dead-code warnings.
- `task coverage` passed at 85.87 percent runtime line coverage
  (101,945/118,726). The locked optimized release build and `git diff --check` passed.
- `task smoke:interactive` passed offline Linux PTY and redirected modes with terminal
  restoration. `task smoke:managed-process` killed the optimized-binary owner and
  proved supervised descendant cleanup.
- `task qualify:llm-release` passed help, embedded version, doctor, structured planning,
  Responses tool continuation, typed failure reconciliation, and executable stability.
  Because the source tree was not yet committed, the evidence correctly records
  `source_worktree_clean = false` and is not acceptance eligible.

These are Linux runtime results plus a Windows cross-target type-check. Native Windows
Job Object/reparse/ConPTY behavior and native macOS runtime/PTY behavior remain explicit
hosted development-spec gates. The CI workflow now qualifies the exact optimized LLM
release binary on all three native jobs before their platform smoke.

## Authoritative Runtime Decisions

- Runtime workload truth is profile-scoped session JSON with structured `PlanStep`
  state, lifecycle events, and audited tool calls, plus profile-scoped durable daemon
  task records. The historical SQLite/global backlog proposal is superseded.
- Shipped v1 MCP client and server transport is stdio. HTTP/SSE and OAuth remain
  future work unless separately specified and implemented.
- Telegram, Slack, and Discord authentication, listeners, and reply delivery live in
  external adapters. nib accepts only their normalized, tool-schema-closed gateway
  payloads.

See [the workspace inventory](../projects/nib/inventory.md) for adoption and validation
details.
