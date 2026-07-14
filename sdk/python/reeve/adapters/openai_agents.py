"""
OpenAI Agents SDK adapter: ReeveHooks.

Pass an instance as `hooks=` to `Runner.run()` and the run shows up in
the cockpit: agent spans per agent, LLM and tool calls as children,
handoffs closing one agent's span and opening the next. checkpoint()
runs before each LLM call and around tool execution, so pause,
redirect, and kill land at the same safe points the LangChain adapter
uses.

The hooks carry no run id, so spans are keyed by the run's Usage
object: the SDK creates one per run wrapper and shares it across every
hook of that run, which makes `id(context.usage)` a stable run key
even when one ReeveHooks instance serves concurrent runs.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from agents.lifecycle import RunHooks
from opentelemetry import trace as otel_trace

if TYPE_CHECKING:
    from ..sdk import ReeveSdk


def _run_key(context: Any) -> int:
    return id(context.usage)


def _agent_name(agent: Any) -> str:
    return getattr(agent, "name", None) or "agent"


class ReeveHooks(RunHooks):
    """Run hooks that wire checkpoint() and OTel spans into an Agents SDK run."""

    def __init__(self, sdk: "ReeveSdk") -> None:
        super().__init__()
        self._sdk = sdk
        self._tracer = otel_trace.get_tracer("reeve-sdk")
        self._agent_spans: dict[tuple[int, str], otel_trace.Span] = {}
        # An umbrella agent.run span is each run's trace root: agent
        # spans parent under it and everything an agent does parents
        # under that agent. Hooks run outside any OTel context, so
        # parenting is explicit or every span becomes its own trace.
        # The umbrella closes when the run's last agent span does, so
        # the root is emitted last, which is what tells the pipeline
        # the trace is complete (the proxy's turn root works the same
        # way). A first-agent root would end at its handoff and
        # complete the trace while the run was still going.
        self._run_roots: dict[int, otel_trace.Span] = {}
        self._current_agent: dict[int, otel_trace.Span] = {}
        self._llm_spans: dict[int, otel_trace.Span] = {}
        # Tool spans stack per (run, tool name): parallel tool calls of
        # the same tool close newest-first, which keeps durations sane
        # without a call id the hooks do not provide.
        self._tool_spans: dict[tuple[int, str], list[otel_trace.Span]] = {}

    def _child_context(self, parent: otel_trace.Span | None) -> Any:
        if parent is None:
            return None
        return otel_trace.set_span_in_context(parent)

    async def on_agent_start(self, context: Any, agent: Any) -> None:
        key = _run_key(context)
        name = _agent_name(agent)
        root = self._run_roots.get(key)
        if root is None:
            root = self._tracer.start_span("agent.run")
            root.set_attribute("gen_ai.operation.name", "invoke_agent")
            root.set_attribute("gen_ai.agent.name", name)
            self._run_roots[key] = root
        span = self._tracer.start_span(
            f"agent.{name}", context=self._child_context(root)
        )
        span.set_attribute("gen_ai.operation.name", "invoke_agent")
        span.set_attribute("gen_ai.agent.name", name)
        self._agent_spans[(key, name)] = span
        self._current_agent[key] = span
        await self._sdk.checkpoint()

    def _maybe_forget_run(self, key: int) -> None:
        # Once a run's last agent span closes, end the umbrella root and
        # drop every per-run entry: id(usage) can be recycled after the
        # run is garbage collected, and a stale root would graft a
        # future run onto a dead trace.
        if any(k == key for (k, _) in self._agent_spans):
            return
        root = self._run_roots.pop(key, None)
        if root is not None:
            root.end()
        self._current_agent.pop(key, None)
        self._llm_spans.pop(key, None)
        for tool_key in [tk for tk in self._tool_spans if tk[0] == key]:
            del self._tool_spans[tool_key]

    async def on_agent_end(self, context: Any, agent: Any, output: Any) -> None:
        key = _run_key(context)
        span = self._agent_spans.pop((key, _agent_name(agent)), None)
        if span is not None:
            span.end()
        self._maybe_forget_run(key)

    async def on_handoff(self, context: Any, from_agent: Any, to_agent: Any) -> None:
        # A handoff ends one agent's work: close its span so the tree
        # reads as a baton pass, not one long undifferentiated run. The
        # receiving agent's span opens in its own on_agent_start.
        span = self._agent_spans.pop((_run_key(context), _agent_name(from_agent)), None)
        if span is not None:
            span.set_attribute("gen_ai.handoff.to", _agent_name(to_agent))
            span.end()
        await self._sdk.checkpoint()

    async def on_llm_start(
        self,
        context: Any,
        agent: Any,
        system_prompt: Any = None,
        input_items: Any = None,
    ) -> None:
        await self._sdk.checkpoint()
        key = _run_key(context)
        span = self._tracer.start_span(
            "llm.call", context=self._child_context(self._current_agent.get(key))
        )
        span.set_attribute("gen_ai.operation.name", "chat")
        model = getattr(agent, "model", None)
        if isinstance(model, str) and model:
            span.set_attribute("gen_ai.request.model", model)
        self._llm_spans[key] = span

    async def on_llm_end(self, context: Any, agent: Any, response: Any) -> None:
        span = self._llm_spans.pop(_run_key(context), None)
        if span is None:
            return
        usage = getattr(response, "usage", None)
        if usage is not None:
            for field, attr in (
                ("input_tokens", "gen_ai.usage.input_tokens"),
                ("output_tokens", "gen_ai.usage.output_tokens"),
                ("total_tokens", "gen_ai.usage.total_tokens"),
            ):
                value = getattr(usage, field, None)
                if value:
                    span.set_attribute(attr, int(value))
        span.end()

    async def on_tool_start(self, context: Any, agent: Any, tool: Any) -> None:
        key = _run_key(context)
        name = getattr(tool, "name", None) or "tool"
        span = self._tracer.start_span(
            name, context=self._child_context(self._current_agent.get(key))
        )
        span.set_attribute("gen_ai.operation.name", "tool_call")
        span.set_attribute("gen_ai.tool.name", name)
        self._tool_spans.setdefault((key, name), []).append(span)
        await self._sdk.checkpoint()

    async def on_tool_end(self, context: Any, agent: Any, tool: Any, result: Any) -> None:
        name = getattr(tool, "name", None) or "tool"
        stack = self._tool_spans.get((_run_key(context), name))
        if stack:
            stack.pop().end()
        await self._sdk.checkpoint()
