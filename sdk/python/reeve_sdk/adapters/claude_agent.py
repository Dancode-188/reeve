"""
Claude Agent SDK adapter: ReeveClaudeClient.

A drop-in wrapper for `ClaudeSDKClient`: construct it with a connected
`ReeveSdk` and your options, then query and iterate messages exactly as
before. The session lands in the cockpit as one trace per response
cycle, with LLM calls, tools, and subagents as children, and the exact
`total_cost_usd` from the result on the root.

The agent loop runs inside the Claude Code CLI subprocess, so control
works through hooks rather than around a local loop. checkpoint() runs
in PreToolUse, a real hold point: pause keeps the hook from returning
and the CLI waits before its next tool; kill returns a hard stop;
redirect and inject land as additional context the model reads before
acting. The PreToolUse matcher timeout is raised well above the 60s
default, or a pause hold would read as a hook failure.
"""

from __future__ import annotations

import time
from typing import TYPE_CHECKING, Any

from claude_agent_sdk import ClaudeAgentOptions, ClaudeSDKClient, HookMatcher
from opentelemetry import trace as otel_trace

from ..sdk import AgentKilled, CheckpointResult

if TYPE_CHECKING:
    from ..sdk import ReeveSdk

# A pause may hold an agent for as long as the operator thinks; the
# default 60s matcher timeout would fail the hook out from under it.
_HOOK_TIMEOUT_SECS = 6 * 3600

_USAGE_KEYS = (
    ("input_tokens", "gen_ai.usage.input_tokens"),
    ("output_tokens", "gen_ai.usage.output_tokens"),
    ("cache_read_input_tokens", "gen_ai.usage.cache_read.input_tokens"),
    ("cache_creation_input_tokens", "gen_ai.usage.cache_creation.input_tokens"),
)


class ReeveClaudeClient:
    """ClaudeSDKClient with a Reeve tap: same surface, watched session."""

    def __init__(self, sdk: "ReeveSdk", options: ClaudeAgentOptions | None = None) -> None:
        self._sdk = sdk
        self._tracer = otel_trace.get_tracer("reeve-sdk")
        self._root: otel_trace.Span | None = None
        # Open tool spans by tool_use_id: a subagent's messages carry
        # parent_tool_use_id, so its work parents under the Task tool
        # span that spawned it, which stays open until the task ends.
        self._tool_spans: dict[str, otel_trace.Span] = {}
        self._subagent_spans: dict[str, otel_trace.Span] = {}
        self._last_event_at = time.time_ns()
        self._client = ClaudeSDKClient(self._wrap_options(options or ClaudeAgentOptions()))

    # -- options and hooks -------------------------------------------------

    def _wrap_options(self, options: ClaudeAgentOptions) -> ClaudeAgentOptions:
        hooks = dict(options.hooks or {})
        for event, callback in (
            ("PreToolUse", self._on_pre_tool_use),
            ("SubagentStart", self._on_subagent_start),
            ("SubagentStop", self._on_subagent_stop),
            ("PreCompact", self._on_pre_compact),
        ):
            matchers = list(hooks.get(event) or [])
            matchers.append(HookMatcher(hooks=[callback], timeout=_HOOK_TIMEOUT_SECS))
            hooks[event] = matchers
        options.hooks = hooks
        return options

    async def _on_pre_tool_use(
        self, input_data: dict[str, Any], tool_use_id: str | None, context: Any
    ) -> dict[str, Any]:
        # The one place the CLI provably waits on this process: pause
        # holds here, and a command lands before the next tool runs.
        try:
            result = await self._sdk.checkpoint()
        except AgentKilled:
            return {
                "continue_": False,
                "stopReason": "killed via Reeve",
            }
        if isinstance(result, CheckpointResult.Redirect):
            return {
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "additionalContext": (
                        "[Operator redirect via Reeve] A human operator watching "
                        "this session has changed the priorities. Your work so far "
                        "is not in question. From this point, do the following "
                        f"instead: {result.instruction}"
                    ),
                }
            }
        if isinstance(result, CheckpointResult.Context):
            return {
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "additionalContext": (
                        "[Operator context via Reeve] A human operator watching "
                        f"this session shares the following context: {result.context}"
                    ),
                }
            }
        return {}

    async def _on_subagent_start(
        self, input_data: dict[str, Any], tool_use_id: str | None, context: Any
    ) -> dict[str, Any]:
        agent_id = input_data.get("agent_id", "subagent")
        span = self._tracer.start_span(
            f"subagent.{input_data.get('agent_type', 'task')}",
            context=self._under(self._root),
        )
        span.set_attribute("gen_ai.operation.name", "invoke_agent")
        span.set_attribute("gen_ai.agent.name", str(input_data.get("agent_type", "task")))
        self._subagent_spans[agent_id] = span
        return {}

    async def _on_subagent_stop(
        self, input_data: dict[str, Any], tool_use_id: str | None, context: Any
    ) -> dict[str, Any]:
        span = self._subagent_spans.pop(input_data.get("agent_id", "subagent"), None)
        if span is not None:
            span.end()
        return {}

    async def _on_pre_compact(
        self, input_data: dict[str, Any], tool_use_id: str | None, context: Any
    ) -> dict[str, Any]:
        # The same honesty the proxy path has: the context is about to
        # change shape, and the trace should say so rather than leave a
        # mystery.
        if self._root is not None:
            self._root.set_attribute(
                "reeve.context.compacted", str(input_data.get("trigger", "auto"))
            )
        return {}

    # -- message tap -------------------------------------------------------

    def _under(self, parent: otel_trace.Span | None) -> Any:
        if parent is None:
            return None
        return otel_trace.set_span_in_context(parent)

    def _observe(self, message: Any) -> None:
        now = time.time_ns()
        kind = type(message).__name__

        if kind == "AssistantMessage":
            if self._root is None:
                self._root = self._tracer.start_span("claude.session")
                self._root.set_attribute("gen_ai.operation.name", "invoke_agent")
            # A subagent's messages carry the Task tool_use id that
            # spawned it; parent there so the tree shows whose work
            # this is. The CLI reports each message complete, so the
            # span runs from the previous event to now.
            parent = self._root
            parent_tool = getattr(message, "parent_tool_use_id", None)
            if parent_tool and parent_tool in self._tool_spans:
                parent = self._tool_spans[parent_tool]
            span = self._tracer.start_span(
                "llm.call", context=self._under(parent), start_time=self._last_event_at
            )
            span.set_attribute("gen_ai.operation.name", "chat")
            model = getattr(message, "model", None)
            if model:
                span.set_attribute("gen_ai.request.model", str(model))
            usage = getattr(message, "usage", None) or {}
            for key, attr in _USAGE_KEYS:
                value = usage.get(key)
                if value:
                    span.set_attribute(attr, int(value))
            span.end()
            for block in getattr(message, "content", None) or []:
                if type(block).__name__ == "ToolUseBlock":
                    tool = self._tracer.start_span(
                        getattr(block, "name", "tool"), context=self._under(parent)
                    )
                    tool.set_attribute("gen_ai.operation.name", "execute_tool")
                    tool.set_attribute("gen_ai.tool.name", getattr(block, "name", "tool"))
                    self._tool_spans[getattr(block, "id", "")] = tool

        elif kind == "UserMessage":
            # Tool results ride back as user messages; each closes the
            # tool span it answers.
            for block in getattr(message, "content", None) or []:
                if type(block).__name__ == "ToolResultBlock":
                    span = self._tool_spans.pop(getattr(block, "tool_use_id", ""), None)
                    if span is not None:
                        if getattr(block, "is_error", False):
                            span.set_status(otel_trace.StatusCode.ERROR)
                        span.end()

        elif kind == "ResultMessage":
            for span in self._tool_spans.values():
                span.end()
            self._tool_spans.clear()
            for span in self._subagent_spans.values():
                span.end()
            self._subagent_spans.clear()
            if self._root is not None:
                cost = getattr(message, "total_cost_usd", None)
                if cost is not None:
                    # The exact figure, kept off gen_ai.usage.cost so the
                    # accumulator does not double-count it on top of the
                    # per-call estimates the pipeline prices from usage.
                    self._root.set_attribute("reeve.claude.total_cost_usd", float(cost))
                if getattr(message, "is_error", False):
                    self._root.set_status(otel_trace.StatusCode.ERROR)
                self._root.end()
                self._root = None

        self._last_event_at = now

    # -- the ClaudeSDKClient surface ----------------------------------------

    async def connect(self, prompt: Any = None) -> None:
        await self._client.connect(prompt)

    async def disconnect(self) -> None:
        await self._client.disconnect()

    async def __aenter__(self) -> "ReeveClaudeClient":
        await self._client.__aenter__()
        return self

    async def __aexit__(self, *exc: Any) -> Any:
        return await self._client.__aexit__(*exc)

    async def query(self, prompt: Any, session_id: str = "default") -> None:
        await self._client.query(prompt, session_id)

    async def interrupt(self) -> None:
        await self._client.interrupt()

    async def receive_messages(self) -> Any:
        async for message in self._client.receive_messages():
            self._observe(message)
            yield message

    async def receive_response(self) -> Any:
        async for message in self._client.receive_response():
            self._observe(message)
            yield message

    def __getattr__(self, name: str) -> Any:
        # Everything else (set_model, get_server_info, ...) passes through.
        return getattr(self._client, name)
