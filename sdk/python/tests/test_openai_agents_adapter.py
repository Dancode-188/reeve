import os
import sys
from types import SimpleNamespace

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import pytest

pytest.importorskip("agents")

from opentelemetry import trace as otel_trace

from agents.run_context import RunContextWrapper
from reeve_sdk.adapters.openai_agents import ReeveHooks

from tests._otel import EXPORTER as _EXPORTER


class RecordingSdk:
    """Stands in for ReeveSdk: counts checkpoints instead of talking gRPC."""

    def __init__(self):
        self.checkpoints = 0

    async def checkpoint(self):
        self.checkpoints += 1


def make_run():
    return RunContextWrapper(context=None)


@pytest.mark.asyncio
async def test_full_lifecycle_produces_spans_and_checkpoints():
    sdk = RecordingSdk()
    hooks = ReeveHooks(sdk)
    run = make_run()
    agent = SimpleNamespace(name="researcher", model="gpt-4.1")
    tool = SimpleNamespace(name="web_search")

    await hooks.on_agent_start(run, agent)
    await hooks.on_llm_start(run, agent, None, [])
    await hooks.on_llm_end(
        run,
        agent,
        SimpleNamespace(
            usage=SimpleNamespace(input_tokens=120, output_tokens=30, total_tokens=150)
        ),
    )
    await hooks.on_tool_start(run, agent, tool)
    await hooks.on_tool_end(run, agent, tool, "ok")
    await hooks.on_agent_end(run, agent, "done")

    spans = {s.name: s for s in _EXPORTER.get_finished_spans()}
    assert set(spans) == {"agent.run", "agent.researcher", "llm.call", "web_search"}
    llm = spans["llm.call"]
    assert llm.attributes["gen_ai.request.model"] == "gpt-4.1"
    assert llm.attributes["gen_ai.usage.total_tokens"] == 150
    assert spans["web_search"].attributes["gen_ai.tool.name"] == "web_search"
    assert spans["agent.researcher"].attributes["gen_ai.agent.name"] == "researcher"
    # One trace under the umbrella root: hooks run outside any OTel
    # context, so without explicit parenting every span becomes its own
    # trace. The root ends last, which is what completes the trace.
    root_ctx = spans["agent.run"].get_span_context()
    agent_ctx = spans["agent.researcher"].get_span_context()
    assert spans["agent.run"].parent is None
    assert spans["agent.researcher"].parent.span_id == root_ctx.span_id
    for child in ("llm.call", "web_search"):
        assert spans[child].parent.span_id == agent_ctx.span_id, child
        assert spans[child].context.trace_id == root_ctx.trace_id, child
    assert spans["agent.run"].end_time >= spans["agent.researcher"].end_time
    # agent start, llm start, tool start, tool end: the safe yield points.
    assert sdk.checkpoints == 4


@pytest.mark.asyncio
async def test_handoff_closes_the_outgoing_agent_span():
    hooks = ReeveHooks(RecordingSdk())
    run = make_run()
    triage = SimpleNamespace(name="triage", model=None)
    expert = SimpleNamespace(name="expert", model=None)

    await hooks.on_agent_start(run, triage)
    await hooks.on_handoff(run, triage, expert)
    await hooks.on_agent_start(run, expert)
    await hooks.on_agent_end(run, expert, "answer")

    spans = {s.name: s for s in _EXPORTER.get_finished_spans()}
    assert set(spans) == {"agent.run", "agent.triage", "agent.expert"}
    assert spans["agent.triage"].attributes["gen_ai.handoff.to"] == "expert"
    # The baton pass stays one trace: both agents parent under the
    # umbrella root, and the root outlives the handoff so the trace
    # cannot complete while the receiving agent still works.
    root_ctx = spans["agent.run"].get_span_context()
    for name in ("agent.triage", "agent.expert"):
        assert spans[name].parent.span_id == root_ctx.span_id, name
        assert spans[name].context.trace_id == root_ctx.trace_id, name
    assert spans["agent.run"].end_time >= spans["agent.expert"].end_time


@pytest.mark.asyncio
async def test_concurrent_runs_do_not_cross_spans():
    hooks = ReeveHooks(RecordingSdk())
    run_a, run_b = make_run(), make_run()
    agent = SimpleNamespace(name="worker", model="gpt-4.1")

    # Both runs have an LLM call in flight at once; each end must close
    # its own run's span, which the per-run usage keying guarantees.
    await hooks.on_llm_start(run_a, agent, None, [])
    await hooks.on_llm_start(run_b, agent, None, [])
    await hooks.on_llm_end(
        run_a,
        agent,
        SimpleNamespace(usage=SimpleNamespace(input_tokens=1, output_tokens=1, total_tokens=2)),
    )
    await hooks.on_llm_end(
        run_b,
        agent,
        SimpleNamespace(usage=SimpleNamespace(input_tokens=1, output_tokens=1, total_tokens=99)),
    )

    totals = sorted(
        s.attributes["gen_ai.usage.total_tokens"]
        for s in _EXPORTER.get_finished_spans()
    )
    assert totals == [2, 99], "each run closed its own span with its own usage"


@pytest.mark.asyncio
async def test_parallel_calls_of_one_tool_all_close():
    hooks = ReeveHooks(RecordingSdk())
    run = make_run()
    agent = SimpleNamespace(name="worker", model=None)
    tool = SimpleNamespace(name="web_search")

    await hooks.on_tool_start(run, agent, tool)
    await hooks.on_tool_start(run, agent, tool)
    await hooks.on_tool_end(run, agent, tool, "first")
    await hooks.on_tool_end(run, agent, tool, "second")

    spans = _EXPORTER.get_finished_spans()
    assert len(spans) == 2
    assert all(s.end_time is not None for s in spans)


@pytest.mark.asyncio
async def test_unpaired_end_hooks_are_harmless():
    hooks = ReeveHooks(RecordingSdk())
    run = make_run()
    agent = SimpleNamespace(name="ghost", model=None)
    await hooks.on_agent_end(run, agent, None)
    await hooks.on_llm_end(run, agent, SimpleNamespace(usage=None))
    await hooks.on_tool_end(run, agent, SimpleNamespace(name="t"), None)
    assert _EXPORTER.get_finished_spans() == ()
