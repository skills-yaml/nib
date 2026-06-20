"""Core agent loop for nib.

Drives LLM reasoning over session history, assembles context,
selects tools, executes them via the gated ToolExecutor, and
records everything back into the per-project .nib/sessions/ storage.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from nib.core.workload import SessionStore  # file-based sessions
from nib.llm.base import LLMClient, LLMResponse, ToolCallRequest
from nib.tools.executor import ToolExecutor
from nib.tools.models import ToolCall
from nib.tools.registry import list_tools


async def run_agent_loop(
    session_store: SessionStore,
    session_id: str,
    goal: str,
    llm: LLMClient,
    executor: ToolExecutor,
    max_steps: int = 20,
    mode: str = "execute",  # "plan" | "execute"
    project_root: Path | None = None,
) -> dict[str, Any]:
    """Run the main agent loop for a goal.

    Returns a summary of the run. All turns and tool calls are persisted
    to the session file under .nib/sessions/.
    """
    if project_root is None:
        project_root = Path.cwd()

    session = session_store.load_session(session_id)
    if session is None:
        session = session_store.create_session()
        if session.id != session_id:
            # ensure id matches
            session.id = session_id
            session_store.save_session(session)

    # Record the user goal
    session_store.append_message(session_id, "user", goal)

    tools_schema = _build_tools_schema()

    for step in range(max_steps):
        # Assemble prompt context from current session
        prompt = _build_prompt(
            session_store,
            session_id,
            goal,
            tools_schema,
            mode,
            project_root,
        )

        messages = [{"role": "user", "content": prompt}]

        response: LLMResponse = await llm.complete(
            messages=messages,
            tools=tools_schema if mode == "execute" else None,
        )

        # Record assistant response
        if response.content:
            session_store.append_message(session_id, "assistant", response.content)

        if response.tool_calls:
            for tc in response.tool_calls:
                # Build ToolCall – executor will handle permissions, sandbox, recording
                call = ToolCall(
                    tool_name=tc.name,
                    arguments=tc.arguments,
                    session_id=session_id,
                    project_root=str(project_root),
                )
                result = await executor.execute(call, session_id=session_id)

                # The executor already records the detailed ToolCallRecord.
                # We also append a compact observation for the LLM.
                obs = {
                    "tool": tc.name,
                    "success": result.success,
                    "output": result.output,
                    "error": result.error,
                }
                session_store.append_message(session_id, "tool", json.dumps(obs))

                if not result.success:
                    # Give the LLM a chance to recover
                    continue

        else:
            # No more tool calls – we're done (or in plan mode)
            if mode == "plan" or _is_final_answer(response):
                break

    # Return lightweight summary
    final_session = session_store.load_session(session_id)
    return {
        "session_id": session_id,
        "steps_taken": step + 1,
        "last_message": final_session.messages[-1].content if final_session.messages else None,
        "tool_call_count": len(final_session.tool_calls) if final_session else 0,
    }


def _build_tools_schema() -> list[dict[str, Any]]:
    """Convert registered tools into LLM tool schemas."""
    schemas = []
    for meta in list_tools():
        # Simple schema – real version would use Pydantic model_json_schema()
        schemas.append(
            {
                "type": "function",
                "function": {
                    "name": meta.name,
                    "description": meta.description,
                    "parameters": {
                        "type": "object",
                        "properties": {"path": {"type": "string"}, "command": {"type": "string"}},
                        "required": [],
                    },
                },
            }
        )
    return schemas


def _build_prompt(
    session_store: SessionStore,
    session_id: str,
    goal: str,
    tools_schema: list[dict],
    mode: str,
    project_root: Path,
) -> str:
    """Build the prompt for the current turn using session history + context."""
    sess = session_store.load_session(session_id)

    history_lines = []
    for msg in (sess.messages[-8:] if sess else []):  # recent history
        prefix = "USER" if msg.role == "user" else msg.role.upper()
        history_lines.append(f"{prefix}: {msg.content}")

    tool_list = "\n".join(
        f"- {t['function']['name']}: {t['function'].get('description', '')}"
        for t in tools_schema
    ) if tools_schema else "No tools available."

    system = f"""You are nib, a trustworthy local-first coding agent.

Project root: {project_root}
Current mode: {mode}

Follow any rules from AGENTS.md and active skills (injected by the runtime).

Available tools:
{tool_list}

Current goal: {goal}

Recent conversation history:
{chr(10).join(history_lines) if history_lines else "(no previous messages)"}

Instructions:
- Think step by step.
- If you need to use a tool, call it (the system will execute and give you the result).
- When the goal is complete, output a clear final answer or (in plan mode) a structured plan.
- In "plan" mode, only use read/search tools and end with a plan.
"""

    return system


def _is_final_answer(response: LLMResponse) -> bool:
    content = (response.content or "").lower()
    return "final answer" in content or "done" in content or len(content) > 200
