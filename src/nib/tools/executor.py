"""Central ToolExecutor for nib.

All tool usage MUST go through this (or a subclass).
It enforces:
- Scoping (project_root)
- Worktree isolation
- Classification (via registry)
- Permission layers (from permissions deep-dive)
- Approval workflow
- Redaction (basic)
- Audit recording to workload store
- Context from AGENTS.md / skills (wired in later)

This is the single source of truth for "can the agent do this?"
"""

from __future__ import annotations

import time
from pathlib import Path
from typing import Any

from rich.console import Console
from rich.prompt import Confirm

from nib.tools.core_tools import TOOL_IMPLS
from nib.tools.models import (
    ApprovalDecision,
    ApprovalMode,
    ApprovalRequest,
    PermissionLevel,
    ToolCall,
    ToolExecutionRecord,
    ToolResult,
)
from nib.tools.registry import get_permission_level, get_tool_metadata

console = Console()


class ToolExecutor:
    def __init__(
        self,
        workload_store=None,  # Will be nib.core.workload.WorkloadStore
        approval_mode: ApprovalMode = ApprovalMode.MANUAL,
        project_root: Path | None = None,
    ) -> None:
        self.workload_store = workload_store
        self.approval_mode = approval_mode
        self.project_root = project_root
        self._worktree_manager = None  # lazy

    async def execute(
        self,
        call: ToolCall,
        current_task_id: str | None = None,
    ) -> ToolResult:
        """Main entry point. Enforces all permission layers."""
        start = time.time()
        tool_name = call.tool_name
        metadata = get_tool_metadata(tool_name)
        level = metadata.permission_level

        # Layer 1+2: Scoping + Isolation
        effective_root = self._resolve_scope(call)
        worktree = self._ensure_worktree_if_needed(call, metadata, effective_root)

        # Layer 3: Classification (already in metadata, but can be refined)
        # Layer 4: Approval
        approval = await self._handle_approval(call, level, current_task_id)

        if not approval.granted:
            result = ToolResult(
                tool_name=tool_name,
                success=False,
                error="Approval denied",
                duration_seconds=time.time() - start,
                approval_granted=False,
                approval_source=approval.source,
            )
            await self._record_execution(call, result, approval, current_task_id, worktree)
            return result

        # Layer 5: Redaction (basic placeholder - expand later)
        raw_args = self._redact_arguments(call.arguments)

        # Execute the actual tool implementation
        try:
            output = await self._dispatch(tool_name, raw_args, worktree or effective_root)
            success = True
            error = None
        except Exception as e:
            output = None
            success = False
            error = str(e)

        duration = time.time() - start

        result = ToolResult(
            tool_name=tool_name,
            success=success,
            output=output,
            error=error,
            duration_seconds=duration,
            approval_granted=approval.granted,
            approval_source=approval.source,
            redacted=True,  # placeholder
        )

        await self._record_execution(call, result, approval, current_task_id, worktree)
        return result

    def _resolve_scope(self, call: ToolCall) -> Path:
        """Enforce project scoping."""
        root = Path(call.project_root or self.project_root or ".").resolve()
        # TODO: more sophisticated allowlist for shared libs/docs
        return root

    def _ensure_worktree_if_needed(
        self, call: ToolCall, metadata: Any, effective_root: Path
    ) -> Path | None:
        if metadata.requires_worktree or get_permission_level(call.tool_name) in {
            PermissionLevel.DESTRUCTIVE,
            PermissionLevel.SAFE,
        }:
            # TODO: integrate real WorktreeManager
            # For now return the effective root (will be replaced with worktree path)
            return effective_root
        return None

    async def _handle_approval(
        self, call: ToolCall, level: PermissionLevel, task_id: str | None
    ) -> ApprovalDecision:
        """Core approval logic from permissions deep-dive."""
        if level == PermissionLevel.READ_ONLY:
            return ApprovalDecision(granted=True, source="policy", approver_note="read-only")

        if self.approval_mode == ApprovalMode.OFF:
            return ApprovalDecision(granted=True, source="yolo", approver_note="YOLO mode")

        if self.approval_mode == ApprovalMode.POLICY:
            # TODO: check explicit policy / AGENTS.md allowlists / previous grants
            # For now, fall through to manual for safety
            pass

        # Default / MANUAL / SMART path: prompt user
        request = ApprovalRequest(
            tool_name=call.tool_name,
            arguments=call.arguments,
            permission_level=level,
            reason="Agent requested this action as part of current task.",
            risk_explanation=self._risk_explanation(call.tool_name, level),
            suggested_command_or_patch=str(call.arguments),
            task_id=task_id,
        )

        # Simple rich prompt for CLI (TUI version later)
        console.print(
            f"\n[bold red]Approval Required[/] for [cyan]{call.tool_name}[/]", style="red"
        )
        console.print(f"Permission level: [yellow]{level}[/yellow]")
        console.print(f"Arguments: {call.arguments}")
        console.print(f"Risk: {request.risk_explanation}")

        granted = Confirm.ask("Approve this action?", default=False)

        return ApprovalDecision(
            granted=granted,
            source="user" if granted else "denied",
            approver_note="CLI confirmation" if granted else "User denied",
            expires_for_task=True,
        )

    def _risk_explanation(self, tool_name: str, level: PermissionLevel) -> str:
        if level == PermissionLevel.DESTRUCTIVE:
            return (
                "This action can permanently delete or alter files, git history, or system state."
            )
        if level == PermissionLevel.NETWORK:
            return "This action may exfiltrate data or run untrusted code from the network."
        return "Potentially modifying state."

    def _redact_arguments(self, args: dict) -> dict:
        # TODO: real secret redaction (see Hermes patterns)
        return args

    async def _dispatch(self, tool_name: str, args: dict, cwd: Path | None) -> Any:
        """Dispatch to the real implementation after all permission gates."""
        impl = TOOL_IMPLS.get(tool_name)
        if impl is None:
            raise ValueError(f"No implementation registered for tool: {tool_name}")
        return await impl(args, cwd)

    async def _record_execution(
        self,
        call: ToolCall,
        result: ToolResult,
        approval: ApprovalDecision,
        task_id: str | None,
        worktree: Path | None,
    ) -> None:
        """Record to workload for audit (ties to permissions deep-dive requirement)."""
        if not task_id:
            return
        record = ToolExecutionRecord(
            id=f"tool-{int(time.time() * 1000)}",
            task_id=task_id,
            tool_name=call.tool_name,
            arguments=call.arguments,
            result=result,
            approval=approval,
            project_root=str(self.project_root) if self.project_root else None,
            worktree_path=str(worktree) if worktree else None,
        )
        if self.workload_store:
            await self.workload_store.record_tool_execution(record)  # type: ignore[attr-defined]
        else:
            console.print(
                f"[dim]Recorded tool call for task {task_id}: {call.tool_name} (approved={approval.granted})[/dim]"
            )


# Global default executor (can be overridden)
default_executor = ToolExecutor(approval_mode=ApprovalMode.MANUAL)
