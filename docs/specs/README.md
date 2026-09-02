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
The current lifecycle is **44 done, 1 development, and 1 backlog**.

Exact implementation run
[33683995100](https://github.com/skills-yaml/nib/actions/runs/33683995100)
closed the remaining ordinary implementation and native-platform gates for T003, T004,
T006, T007, T020, T021, T022, T026, T029, T034, T035, FT-015, FT-016, FT-017, and
FT-019. The clean Linux, macOS, and Windows jobs passed their complete serial suites,
native all-target checks, exact release-binary qualification, and platform smokes.
The final Linux coverage result was 85.87 percent (102,061/118,862).

T023 is the only development spec. Its bounded credential-free implementation and
offline native matrix are green, but no paid or credentialed live run was authorized.
It still requires owner-approved exact OpenRouter IDs, provider accounts and
credentials, hard spend and execution ceilings, protected environment approvals, and
privacy-reviewed catalog/canary/selected/full evidence for all six provider groups.
FT-020 remains backlog for any future protected Windows/macOS production delegation
authority. The completed FT-015/FT-017 v1 production boundary remains Linux with usable
bwrap containment; native non-Linux mechanism tests are complete and production use
continues to fail closed. Remote MCP transport remains separate future scope; shipped
MCP v1 is stdio-only.

Each audited file has an `Implementation Reconciliation (2026-07-15)` section that
supersedes older proposal text. Later dated remediation sections and their unchecked
criteria supersede that reconciliation snapshot wherever they identify additional
work or narrower guarantees.

### Documentation and task specs

- [D001: Workspace Docs Adoption](done/D001_workspace_docs_adoption_and_foundational_spec_alignment.md)
- [T001: Core Agent Tools](done/T001_implement_core_agent_tools.md)
- [T002: Runtime and Orchestration](done/T002_agent_framework_runtime_and_orchestration_engine.md)
- [T003: Context and Compression](done/T003_context_engine_with_dynamic_compression_and_session_management.md)
- [T004: Profiles, Memory, and Daemons](done/T004_profiles_discrete_memory_store_and_maintenance_daemons.md)
- [T005: Runtime State Machine](done/T005_full_runtime_state_machine_and_lifecycle.md)
- [T006: Skills and MCP Gateway](done/T006_enhanced_skills_framework_and_mcp_gateway_alignment.md)
- [T007: Configuration and Doctor](done/T007_configuration_schema_alignment_and_nib_doctor_validation.md)
- [T008: End-to-End Validation](done/T008_end_to_end_tests_and_sequence_diagram_validation.md)
- [T009: Rust Module Layout and TOML Config](done/T009_rust_module_layout_and_toml_config.md)
- [T010: Release Process](done/T010_release_process.md)
- [T011: End-User Documentation](done/T011_end_user_documentation.md)
- [T012: Toolset Expansion](done/T012_toolset_expansion.md)
- [T018: ratatui Approval Flow](done/T018_ratatui_tui_approval.md)
- [T020: MCP Client Integration](done/T020_mcp_client_integration.md)
- [T021: OpenAI-Compatible Reasoning and Tool Transport Compatibility](done/T021_openai_compatible_reasoning_and_tool_transport_compatibility.md)
- [T022: Provider-Neutral LLM Contract and Adapter Conformance](done/T022_provider_neutral_llm_contract_and_adapter_conformance.md)
- [T023: Live LLM Provider and Model Integration Qualification](development/T023_live_llm_provider_model_integration_qualification.md)
- [T024: Configurable Provider Model Catalog and Curated Defaults](done/T024_configurable_provider_model_catalog_and_curated_defaults.md)
- [T025: Interactive Chat and TUI Capability Parity](done/T025_interactive_chat_tui_capability_parity.md)
- [T026: Actionable, Redaction-Safe LLM Failure Reporting](done/T026_actionable_redaction_safe_llm_failure_reporting.md)
- [T027: Doctor-Guided OpenAI Transport Repair](done/T027_doctor_guided_openai_transport_repair.md)
- [T028: Current-Session-First TUI and Slash-Command Completion](done/T028_current_session_first_tui_and_slash_command_completion.md)
- [T029: Explicit Self-Update Channel Switching](done/T029_explicit_self_update_channel_switching.md)
- [T030: Unified Interactive CLI and Plain-Mode Fallback](done/T030_unified_interactive_cli_and_plain_mode_fallback.md)
- [T031: FT-019 Interaction Model, Ledger TUI, and Queue-Only Live Input](done/T031_ft019_interaction_model_and_ledger_tui.md)
- [T032: FT-019 Explicit Compaction and Session Background Commands](done/T032_ft019_explicit_compaction_and_session_background_commands.md)
- [T033: FT-019 Exact-Run Live Steering](done/T033_ft019_exact_run_live_steering.md)
- [T034: FT-019 Native Terminal Qualification](done/T034_ft019_native_terminal_qualification.md)
- [T035: Fast Incremental Check and Single Full Verification](done/T035_fast_incremental_check_and_single_full_verification.md)

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
- [FT-015: Subagent Delegation](done/ft_015_subagent_delegation.md)
- [FT-016: MCP Server Exposure](done/ft_016_mcp_server_exposure.md)
- [FT-017: Managed Process Supervisor](done/ft_017_managed_process_supervisor.md)
- [FT-018: Self-Update Command and Update Availability Notices](done/ft_018_self_update_and_update_notifications.md)
- [FT-019: Codex-Inspired Chat and TUI Interactions](done/ft_019_codex_inspired_chat_and_tui_interactions.md)
- [FT-020: Protected Non-Linux Production Delegation Authority](backlog/ft_020_protected_non_linux_production_delegation_authority.md)

## Current Validation (2026-09-02)

- Local `task verify` passed 1,062 library tests, 86 CLI tests, every integration suite,
  and doctests; the paid live-provider and exact release qualification entrypoints
  remained explicitly gated from the ordinary suite.
- Exact hosted run `33683995100` passed Validate, macOS Tests, and Windows Tests for
  head `c3b88564da4f6f654a8618e4fa544b353ece86f5` at clean merge checkout
  `0479b72ad3d11fd7221632f042736b8489b6443b`.
- The hosted Linux coverage gate passed at 85.87 percent (102,061/118,862). Linux and
  macOS native PTY/redirected smokes and the Windows ConPTY/`TERM=dumb`/redirected smoke
  passed with terminal restoration and bounded execution.
- Exact release qualification passed the credential-free structured planning,
  Responses tool continuation, failure reconciliation, doctor, identity, and stability
  matrix. Binary SHA-256 values were
  `e9b56b4c2b527ab04bd4e40932c83a632ae5bd5931010dee6152012b421e4276`
  (Linux), `e7bbf6ea23d87a3e00b1447fc7880f2c93e6c67a27239f0068bcb599d18fb739`
  (macOS), and
  `e9250200aa0b06188e3e05d062ccd39115eb98311d0dc9b691cfdc5e9a324423`
  (Windows); every report recorded `source_worktree_clean = true`.

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
