"""Basic tests for tools and executor (expand for full T001.7)."""

import pytest

from nib.tools.models import PermissionLevel, ToolCall
from nib.tools.registry import get_permission_level, list_tools


def test_core_tools_registered():
    tools = list_tools()
    names = {t.name for t in tools}
    assert {"read_file", "list_directory", "grep", "apply_patch", "run_terminal"} <= names


def test_permission_levels():
    assert get_permission_level("read_file") == PermissionLevel.READ_ONLY
    assert get_permission_level("run_terminal") == PermissionLevel.DESTRUCTIVE


@pytest.mark.asyncio
async def test_executor_read_only_no_approval():
    from nib.tools.executor import ToolExecutor
    from pathlib import Path

    exec = ToolExecutor(project_root=Path("."))
    call = ToolCall(tool_name="list_directory", arguments={"path": "."})
    res = await exec.execute(call, current_task_id="test-task")
    assert res.success is True
    assert res.approval_granted is True
    assert res.approval_source == "policy"
