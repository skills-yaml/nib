# Development Specifications

nib follows workspace-docs@7.0.0. Every current spec has one primary feature,
state and catalog row. Specs live at workspace/specs/<state>/<primary-feature>/.
Plan companions belong to their owning spec and have no separate catalog row.

## Lifecycle

backlog -> development -> test -> done. Shared test integration uses development;
completion uses main. Blocked work records previous state, kind, reason and resume
condition. Publication is separate. Historical done contracts are preserved;
new work requires verified main merge. See [SDLC](../instructions/tech/sdlc.md).

## Primary Features

- `agent-runtime`
- `architecture`
- `build-quality`
- `context-memory`
- `delegation`
- `documentation`
- `interactive`
- `llm-providers`
- `model-routing`
- `prompt-caching`
- `release-delivery`
- `skills-mcp`
- `tools-sandbox`
- `workspace-governance`

## Status Catalog

<!-- SPEC-CATALOG:START -->
| Spec | Primary feature | State | Status rationale |
| --- | --- | --- | --- |
| [FT-021](backlog/model-routing/ft_021_cost_controlled_model_escalation.md) | model-routing | backlog | Inactive proposal awaiting scoped development decisions. |
| [FT-022](backlog/prompt-caching/ft_022_provider_prompt_caching.md) | prompt-caching | backlog | Inactive proposal awaiting scoped development decisions. |
| [T061](development/interactive/T061_question_form.md) | interactive | development | Accepted question-form contract; implementation pending. |
| [T023](development/llm-providers/T023_live_llm_provider_model_integration_qualification.md) | llm-providers | development | Live provider qualification incomplete; no paid calls made by this migration. |
| [T062](development/workspace-governance/T062_workspace_docs_v7_upgrade.md) | workspace-governance | development | Local Workspace upgrade and verification; shared integration and main merge pending. |
| [T001](done/agent-runtime/T001_implement_core_agent_tools.md) | agent-runtime | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T002](done/agent-runtime/T002_agent_framework_runtime_and_orchestration_engine.md) | agent-runtime | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T005](done/agent-runtime/T005_full_runtime_state_machine_and_lifecycle.md) | agent-runtime | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T012](done/agent-runtime/T012_toolset_expansion.md) | agent-runtime | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T040](done/agent-runtime/T040_resourceful_agent_loop_and_context.md) | agent-runtime | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T041](done/agent-runtime/T041_task_aware_context_and_verified_completion.md) | agent-runtime | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T057](done/agent-runtime/T057_contextual_answers_and_plan_progress.md) | agent-runtime | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [FT-001](done/agent-runtime/ft_001_basic_agent_tools.md) | agent-runtime | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [FT-004](done/agent-runtime/ft_004_llm_integration_and_agent_loop.md) | agent-runtime | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [FT-012](done/agent-runtime/ft_012_richer_planner.md) | agent-runtime | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T009](done/architecture/T009_rust_module_layout_and_toml_config.md) | architecture | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [FT-002](done/architecture/ft_002_base_architecture.md) | architecture | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [FT-005](done/architecture/ft_005_pure_rust_core_migration.md) | architecture | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T008](done/build-quality/T008_end_to_end_tests_and_sequence_diagram_validation.md) | build-quality | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T035](done/build-quality/T035_fast_incremental_check_and_single_full_verification.md) | build-quality | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T043](done/build-quality/T043_oversized_modules_and_coverage_artifact_hygiene.md) | build-quality | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T044](done/build-quality/T044_strict_clippy_quality_gate.md) | build-quality | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T003](done/context-memory/T003_context_engine_with_dynamic_compression_and_session_management.md) | context-memory | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T004](done/context-memory/T004_profiles_discrete_memory_store_and_maintenance_daemons.md) | context-memory | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T042](done/context-memory/T042_context_budgeting_and_live_visibility.md) | context-memory | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T055](done/context-memory/T055_conversational_repository_aware_help.md) | context-memory | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [FT-013](done/context-memory/ft_013_advanced_session_memory.md) | context-memory | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [FT-015](done/delegation/ft_015_subagent_delegation.md) | delegation | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [FT-020](done/delegation/ft_020_protected_non_linux_production_delegation_authority.md) | delegation | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T011](done/documentation/T011_end_user_documentation.md) | documentation | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T018](done/interactive/T018_ratatui_tui_approval.md) | interactive | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T025](done/interactive/T025_interactive_chat_tui_capability_parity.md) | interactive | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T028](done/interactive/T028_current_session_first_tui_and_slash_command_completion.md) | interactive | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T030](done/interactive/T030_unified_interactive_cli_and_plain_mode_fallback.md) | interactive | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T031](done/interactive/T031_ft019_interaction_model_and_ledger_tui.md) | interactive | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T032](done/interactive/T032_ft019_explicit_compaction_and_session_background_commands.md) | interactive | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T033](done/interactive/T033_ft019_exact_run_live_steering.md) | interactive | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T034](done/interactive/T034_ft019_native_terminal_qualification.md) | interactive | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T036](done/interactive/T036_conversational_tui_visual_hierarchy.md) | interactive | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T037](done/interactive/T037_tui_cancellation_modal_cleanup.md) | interactive | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T038](done/interactive/T038_tui_block_transcript_and_key_contract.md) | interactive | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T039](done/interactive/T039_visible_tool_blocks_and_explicit_approval.md) | interactive | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T045](done/interactive/T045_codex_style_thought_and_tool_rows.md) | interactive | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T047](done/interactive/T047_user_interaction_harmonization.md) | interactive | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T048](done/interactive/T048_active_tui_render_responsiveness.md) | interactive | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T049](done/interactive/T049_interaction_card.md) | interactive | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T050](done/interactive/T050_plan_free_answers_decision_prompts_and_run_outcomes.md) | interactive | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T052](done/interactive/T052_windows_tui_session_cache_refresh.md) | interactive | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T053](done/interactive/T053_visible_stop_reasons.md) | interactive | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [FT-011](done/interactive/ft_011_llm_streaming_and_tui.md) | interactive | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [FT-019](done/interactive/ft_019_codex_inspired_chat_and_tui_interactions.md) | interactive | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T007](done/llm-providers/T007_configuration_schema_alignment_and_nib_doctor_validation.md) | llm-providers | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T021](done/llm-providers/T021_openai_compatible_reasoning_and_tool_transport_compatibility.md) | llm-providers | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T022](done/llm-providers/T022_provider_neutral_llm_contract_and_adapter_conformance.md) | llm-providers | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T024](done/llm-providers/T024_configurable_provider_model_catalog_and_curated_defaults.md) | llm-providers | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T026](done/llm-providers/T026_actionable_redaction_safe_llm_failure_reporting.md) | llm-providers | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T027](done/llm-providers/T027_doctor_guided_openai_transport_repair.md) | llm-providers | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T051](done/llm-providers/T051_windows_mcp_provider_failure_stack.md) | llm-providers | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T010](done/release-delivery/T010_release_process.md) | release-delivery | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T029](done/release-delivery/T029_explicit_self_update_channel_switching.md) | release-delivery | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T046](done/release-delivery/T046_cross_platform_ci_repairs.md) | release-delivery | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T059](done/release-delivery/T059_development_ci_without_windows_tests.md) | release-delivery | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [FT-018](done/release-delivery/ft_018_self_update_and_update_notifications.md) | release-delivery | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T006](done/skills-mcp/T006_enhanced_skills_framework_and_mcp_gateway_alignment.md) | skills-mcp | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T020](done/skills-mcp/T020_mcp_client_integration.md) | skills-mcp | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [FT-006](done/skills-mcp/ft_006_skills_management.md) | skills-mcp | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [FT-016](done/skills-mcp/ft_016_mcp_server_exposure.md) | skills-mcp | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T054](done/tools-sandbox/T054_reconcile_local_preflight_failures.md) | tools-sandbox | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T056](done/tools-sandbox/T056_session_worktree_preflight_recovery.md) | tools-sandbox | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T058](done/tools-sandbox/T058_preflight_diagnostics_and_independent_read_progress.md) | tools-sandbox | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [T060](done/tools-sandbox/T060_terminal_listing_instruction_preflight.md) | tools-sandbox | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [FT-003](done/tools-sandbox/ft_003_adopt_codex_sandboxing.md) | tools-sandbox | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [FT-014](done/tools-sandbox/ft_014_smart_approval_classifier.md) | tools-sandbox | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [FT-017](done/tools-sandbox/ft_017_managed_process_supervisor.md) | tools-sandbox | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
| [D001](done/workspace-governance/D001_workspace_docs_adoption_and_foundational_spec_alignment.md) | workspace-governance | done | Preserved historical done under the prior 1.2.0 completion contract; no v7 main-merge event inferred. |
<!-- SPEC-CATALOG:END -->

## Version and Memory Impact

Follow [versioning](versioning.md) and [release reservations](../releases.json).
Historical done specs retain their prior evidence and use historical reservations.
Completed tasks classify memory as updated or none; unresolved active work may
remain pending. No integration, main merge or publication is inferred by migration.

## Preserved Audit History

The following pre-v7 audit records retain their original completion semantics.

## Implementation Audit Status

T060 corrects Task listing instruction preflight, terminal scope recovery, and
home-installed Task availability under bwrap. See [T060](done/tools-sandbox/T060_terminal_listing_instruction_preflight.md).
T061 is in development for the question form. The contract is accepted and
implementation has not started. The 2026-10-01 revision uses conversational
recovery, Esc interruption, and proposal chat, and removes `/plan` and `/questions`
from the planned interactive command surface. See [T061](development/interactive/T061_question_form.md).

The 2026-07-15 audit inspected all 27 specs that had claimed completion. Unsupported
claims moved through `development/`; missing feasible behavior was implemented and
historical proposal text was reconciled. Repeated compliance and quality/security
reviews reopened owning specs whenever completion claims exceeded the implementation.
The current lifecycle is **70 done, 2 development, and 2 backlog**.

Exact implementation run
[33683995100](https://github.com/skills-yaml/nib/actions/runs/33683995100)
closed the remaining ordinary implementation and native-platform gates for T003, T004,
T006, T007, T020, T021, T022, T026, T029, T034, T035, FT-015, FT-016, FT-017, and
FT-019. The clean Linux, macOS, and Windows jobs passed their complete serial suites,
native all-target checks, exact release-binary qualification, and platform smokes.
The final Linux coverage result was 85.87 percent (102,061/118,862).

T042 ships complete-request `/context` snapshots, input-allowance admission,
chunked compression coverage, continuation accounting, `ctx ~Nk/Mk` occupancy,
and session `--json` inspection. Live-model quality and billed usage remain T023.
T023 remains in development. Its offline harness is implemented, and the protected
six-provider catalog passed on 2026-09-25. Reviewed OpenRouter IDs and retained
paid canary reports are recorded in the spec. Canary tool-continuation failures
remain open, including Anthropic's final-response refusal; selected and full
exact-revision qualification have not passed. The 2026-10-01 decision is to retain
the current Anthropic default and fix the continuation failure rather than replace
the selected model. Further live runs still require the protected approvals,
budgets, and privacy-reviewed evidence described in T023. T036 completed the
conversation-first TUI hierarchy refinement with local verification and native PTY smoke.
T037 closes a cancellation modal-cleanup race identified while qualifying the revised
interaction on hosted macOS, with deterministic regression and native local evidence.
T038 and T039 complete the block transcript, safe key contract, visible tool lifecycle,
under-composer decisions, compact chrome, markdown speech, and startup workspace
consent. Live tool blocks use exact invocation identity, so repeated same-name calls
remain distinct through streaming and completion. T045 restyles thought and tool
rows into a scan list (`▸ Thought for Ns`, quiet `●` tool hints, nested running
spinner) without changing T038 keys.
FT-020 is done with a fail-closed Windows/macOS production contract: Windows Job
creation uses a protected cleanup owner, macOS production stays fail-closed without
a bootstrap reaper, and Linux FT-015/FT-017 production is unchanged. Enabling
Windows or macOS `production()` remains a later independent native qualification.
Remote MCP transport remains separate future scope; shipped MCP v1 is stdio-only.

Each audited file has an `Implementation Reconciliation (2026-07-15)` section that
supersedes older proposal text. Later dated remediation sections and their unchecked
criteria supersede that reconciliation snapshot wherever they identify additional
work or narrower guarantees.

### Documentation and task specs

- [T060: Terminal Listing Instruction Preflight](done/tools-sandbox/T060_terminal_listing_instruction_preflight.md)

- [T059: Development CI Without Windows Tests](done/release-delivery/T059_development_ci_without_windows_tests.md)
- [D001: Workspace Docs Adoption](done/workspace-governance/D001_workspace_docs_adoption_and_foundational_spec_alignment.md)
- [T001: Core Agent Tools](done/agent-runtime/T001_implement_core_agent_tools.md)
- [T002: Runtime and Orchestration](done/agent-runtime/T002_agent_framework_runtime_and_orchestration_engine.md)
- [T003: Context and Compression](done/context-memory/T003_context_engine_with_dynamic_compression_and_session_management.md)
- [T004: Profiles, Memory, and Daemons](done/context-memory/T004_profiles_discrete_memory_store_and_maintenance_daemons.md)
- [T005: Runtime State Machine](done/agent-runtime/T005_full_runtime_state_machine_and_lifecycle.md)
- [T006: Skills and MCP Gateway](done/skills-mcp/T006_enhanced_skills_framework_and_mcp_gateway_alignment.md)
- [T007: Configuration and Doctor](done/llm-providers/T007_configuration_schema_alignment_and_nib_doctor_validation.md)
- [T008: End-to-End Validation](done/build-quality/T008_end_to_end_tests_and_sequence_diagram_validation.md)
- [T009: Rust Module Layout and TOML Config](done/architecture/T009_rust_module_layout_and_toml_config.md)
- [T010: Release Process](done/release-delivery/T010_release_process.md)
- [T011: End-User Documentation](done/documentation/T011_end_user_documentation.md)
- [T012: Toolset Expansion](done/agent-runtime/T012_toolset_expansion.md)
- [T018: ratatui Approval Flow](done/interactive/T018_ratatui_tui_approval.md)
- [T020: MCP Client Integration](done/skills-mcp/T020_mcp_client_integration.md)
- [T021: OpenAI-Compatible Reasoning and Tool Transport Compatibility](done/llm-providers/T021_openai_compatible_reasoning_and_tool_transport_compatibility.md)
- [T022: Provider-Neutral LLM Contract and Adapter Conformance](done/llm-providers/T022_provider_neutral_llm_contract_and_adapter_conformance.md)
- [T023: Live LLM Provider and Model Integration Qualification](development/llm-providers/T023_live_llm_provider_model_integration_qualification.md)
- [T057: Contextual Answers and Plan Progress](done/agent-runtime/T057_contextual_answers_and_plan_progress.md) — direct answers and saved multi-step checklist progress.
- [T024: Configurable Provider Model Catalog and Curated Defaults](done/llm-providers/T024_configurable_provider_model_catalog_and_curated_defaults.md)
- [T025: Interactive Chat and TUI Capability Parity](done/interactive/T025_interactive_chat_tui_capability_parity.md)
- [T026: Actionable, Redaction-Safe LLM Failure Reporting](done/llm-providers/T026_actionable_redaction_safe_llm_failure_reporting.md)
- [T027: Doctor-Guided OpenAI Transport Repair](done/llm-providers/T027_doctor_guided_openai_transport_repair.md)
- [T028: Current-Session-First TUI and Slash-Command Completion](done/interactive/T028_current_session_first_tui_and_slash_command_completion.md)
- [T029: Explicit Self-Update Channel Switching](done/release-delivery/T029_explicit_self_update_channel_switching.md)
- [T030: Unified Interactive CLI and Plain-Mode Fallback](done/interactive/T030_unified_interactive_cli_and_plain_mode_fallback.md)
- [T031: FT-019 Interaction Model, Ledger TUI, and Queue-Only Live Input](done/interactive/T031_ft019_interaction_model_and_ledger_tui.md)
- [T032: FT-019 Explicit Compaction and Session Background Commands](done/interactive/T032_ft019_explicit_compaction_and_session_background_commands.md)
- [T033: FT-019 Exact-Run Live Steering](done/interactive/T033_ft019_exact_run_live_steering.md)
- [T034: FT-019 Native Terminal Qualification](done/interactive/T034_ft019_native_terminal_qualification.md)
- [T035: Fast Incremental Check and Single Full Verification](done/build-quality/T035_fast_incremental_check_and_single_full_verification.md)
- [T036: Conversational TUI Visual Hierarchy](done/interactive/T036_conversational_tui_visual_hierarchy.md)
- [T037: TUI Cancellation Modal Cleanup](done/interactive/T037_tui_cancellation_modal_cleanup.md)
- [T038: TUI Block Transcript and Key Contract](done/interactive/T038_tui_block_transcript_and_key_contract.md)
- [T039: Visible Tool Blocks and Explicit Approval Card](done/interactive/T039_visible_tool_blocks_and_explicit_approval.md)
- [T040: Resourceful Agent Loop and Context](done/agent-runtime/T040_resourceful_agent_loop_and_context.md)
- [T041: Task-Aware Context and Verified Completion](done/agent-runtime/T041_task_aware_context_and_verified_completion.md)
- [T042: Context Budgeting and Live Visibility](done/context-memory/T042_context_budgeting_and_live_visibility.md)
- [T043: Oversized Modules and Coverage-Artifact Hygiene](done/build-quality/T043_oversized_modules_and_coverage_artifact_hygiene.md)
- [T044: Strict Clippy Quality Gate](done/build-quality/T044_strict_clippy_quality_gate.md)
- [T045: Codex-Style Thought and Tool Rows](done/interactive/T045_codex_style_thought_and_tool_rows.md)
- [T046: Cross-platform CI repairs](done/release-delivery/T046_cross_platform_ci_repairs.md)
- [T047: User Interaction Harmonization](done/interactive/T047_user_interaction_harmonization.md)
  ([implementation plan](done/interactive/T047_user_interaction_harmonization.plan.md))
- [T048: Active TUI Render Responsiveness](done/interactive/T048_active_tui_render_responsiveness.md)
- [T049: Command Approval Card](done/interactive/T049_interaction_card.md)
- [T050: Plan-Free Answers, Decision Prompts, and Run Outcomes](done/interactive/T050_plan_free_answers_decision_prompts_and_run_outcomes.md)
- [T051: Windows MCP Provider-Failure Test Stack](done/llm-providers/T051_windows_mcp_provider_failure_stack.md)
- [T052: Windows TUI Session Cache Refresh](done/interactive/T052_windows_tui_session_cache_refresh.md)
- [T053: Visible Stop Reasons](done/interactive/T053_visible_stop_reasons.md)
- [T054: Reconcile Local Preflight Failures](done/tools-sandbox/T054_reconcile_local_preflight_failures.md)
- [T055: Conversational Repository-Aware Help](done/context-memory/T055_conversational_repository_aware_help.md)
- [T056: Session Worktree Preflight Recovery](done/tools-sandbox/T056_session_worktree_preflight_recovery.md)
- [T058: Preflight Diagnostics and Independent Read Progress](done/tools-sandbox/T058_preflight_diagnostics_and_independent_read_progress.md)
- [T061: Question Form](development/interactive/T061_question_form.md) — one `ask_question` call may carry one question or a set; implementation has not started.

### Feature specs

- [FT-022: Provider Prompt Caching](backlog/prompt-caching/ft_022_provider_prompt_caching.md)
  — backlog proposal for stable request prefixes, opt-in provider cache controls,
  and cache-read/write evidence; coverage, retention, and evaluation targets remain
  open before development.
- [FT-021: Cost-Controlled Model Escalation](backlog/model-routing/ft_021_cost_controlled_model_escalation.md)
  — backlog proposal for mostly cheaper/open-weight execution and bounded costly
  assistance; hosting, budget, and quality targets remain open before development.
- [FT-001: Basic Agent Tools](done/agent-runtime/ft_001_basic_agent_tools.md)
- [FT-002: Base Architecture](done/architecture/ft_002_base_architecture.md)
- [FT-003: Hybrid Sandboxing](done/tools-sandbox/ft_003_adopt_codex_sandboxing.md)
- [FT-004: LLM Integration and Agent Loop](done/agent-runtime/ft_004_llm_integration_and_agent_loop.md)
- [FT-005: Pure Rust Core Migration](done/architecture/ft_005_pure_rust_core_migration.md)
- [FT-006: Skills Management](done/skills-mcp/ft_006_skills_management.md)
- [FT-011: LLM Streaming and TUI](done/interactive/ft_011_llm_streaming_and_tui.md)
- [FT-012: Richer Planner](done/agent-runtime/ft_012_richer_planner.md)
- [FT-013: Advanced Session Memory](done/context-memory/ft_013_advanced_session_memory.md)
- [FT-014: Smart Approval Classifier](done/tools-sandbox/ft_014_smart_approval_classifier.md)
- [FT-015: Subagent Delegation](done/delegation/ft_015_subagent_delegation.md)
- [FT-016: MCP Server Exposure](done/skills-mcp/ft_016_mcp_server_exposure.md)
- [FT-017: Managed Process Supervisor](done/tools-sandbox/ft_017_managed_process_supervisor.md)
- [FT-018: Self-Update Command and Update Availability Notices](done/release-delivery/ft_018_self_update_and_update_notifications.md)
- [FT-019: Codex-Inspired Chat and TUI Interactions](done/interactive/ft_019_codex_inspired_chat_and_tui_interactions.md)
- [FT-020: Protected Non-Linux Production Delegation Authority](done/delegation/ft_020_protected_non_linux_production_delegation_authority.md)

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

See [the workspace inventory](../docs/work/inventory.md) for adoption and validation
details.
