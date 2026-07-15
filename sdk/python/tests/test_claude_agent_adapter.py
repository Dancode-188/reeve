import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import pytest

pytest.importorskip("claude_agent_sdk")

from claude_agent_sdk import (
    AssistantMessage,
    ClaudeAgentOptions,
    HookMatcher,
    ResultMessage,
    TextBlock,
    ToolResultBlock,
    ToolUseBlock,
    UserMessage,
)
from opentelemetry import trace as otel_trace

from reeve_sdk.adapters.claude_agent import ReeveClaudeClient
from reeve_sdk.sdk import AgentKilled, CheckpointResult

from tests._otel import EXPORTER as _EXPORTER


class RecordingSdk:
    """Stands in for ReeveSdk: scripted checkpoint outcomes, no gRPC."""

    def __init__(self, outcome=None):
        self.outcome = outcome or CheckpointResult.Continue()
        self.checkpoints = 0

    async def checkpoint(self):
        self.checkpoints += 1
        if isinstance(self.outcome, Exception):
            raise self.outcome
        return self.outcome


def result_message(**overrides):
    fields = dict(
        subtype="success",
        duration_ms=1200,
        duration_api_ms=900,
        is_error=False,
        num_turns=2,
        session_id="s1",
        total_cost_usd=0.0421,
    )
    fields.update(overrides)
    return ResultMessage(**fields)


def test_a_response_cycle_becomes_one_parented_trace():
    client = ReeveClaudeClient(RecordingSdk())
    client._observe(
        AssistantMessage(
            content=[
                TextBlock(text="checking the file"),
                ToolUseBlock(id="toolu_1", name="Read", input={"path": "a.rs"}),
            ],
            model="claude-fable-5",
            usage={"input_tokens": 900, "output_tokens": 40,
                   "cache_read_input_tokens": 500},
        )
    )
    client._observe(
        UserMessage(content=[
            ToolResultBlock(tool_use_id="toolu_1", content="fn main() {}")
        ])
    )
    client._observe(result_message())

    spans = {s.name: s for s in _EXPORTER.get_finished_spans()}
    assert set(spans) == {"claude.session", "llm.call", "Read"}
    root_ctx = spans["claude.session"].get_span_context()
    assert spans["claude.session"].parent is None
    for child in ("llm.call", "Read"):
        assert spans[child].parent.span_id == root_ctx.span_id, child
        assert spans[child].context.trace_id == root_ctx.trace_id, child
    llm = spans["llm.call"]
    assert llm.attributes["gen_ai.request.model"] == "claude-fable-5"
    assert llm.attributes["gen_ai.usage.cache_read.input_tokens"] == 500
    # The exact figure rides its own attribute; gen_ai.usage.cost stays
    # absent so the pipeline's estimates are not double-counted.
    root = spans["claude.session"]
    assert root.attributes["reeve.claude.total_cost_usd"] == pytest.approx(0.0421)
    assert "gen_ai.usage.cost" not in root.attributes
    # The root ends last: it is what completes the trace.
    assert root.end_time >= max(spans[c].end_time for c in ("llm.call", "Read"))


def test_an_errored_tool_result_fails_its_span():
    client = ReeveClaudeClient(RecordingSdk())
    client._observe(
        AssistantMessage(
            content=[ToolUseBlock(id="toolu_9", name="Bash", input={})],
            model="claude-fable-5",
        )
    )
    client._observe(
        UserMessage(content=[
            ToolResultBlock(tool_use_id="toolu_9", content="boom", is_error=True)
        ])
    )
    client._observe(result_message(is_error=True))
    spans = {s.name: s for s in _EXPORTER.get_finished_spans()}
    assert spans["Bash"].status.status_code == otel_trace.StatusCode.ERROR
    assert spans["claude.session"].status.status_code == otel_trace.StatusCode.ERROR


@pytest.mark.asyncio
async def test_subagent_work_parents_under_its_task_tool():
    client = ReeveClaudeClient(RecordingSdk())
    # The main agent opens a Task tool; the subagent starts, speaks with
    # parent_tool_use_id pointing at that Task, then stops.
    client._observe(
        AssistantMessage(
            content=[ToolUseBlock(id="task_1", name="Task", input={})],
            model="claude-fable-5",
        )
    )
    await client._on_subagent_start(
        {"agent_id": "sub_1", "agent_type": "researcher"}, None, None
    )
    client._observe(
        AssistantMessage(
            content=[TextBlock(text="digging")],
            model="claude-haiku-4-5-20251001",
            parent_tool_use_id="task_1",
        )
    )
    await client._on_subagent_stop({"agent_id": "sub_1"}, None, None)
    client._observe(
        UserMessage(content=[ToolResultBlock(tool_use_id="task_1", content="done")])
    )
    client._observe(result_message())

    spans = _EXPORTER.get_finished_spans()
    by_name = {}
    for s in spans:
        by_name.setdefault(s.name, []).append(s)
    task_span = by_name["Task"][0]
    sub_llm = [s for s in by_name["llm.call"]
               if s.attributes.get("gen_ai.request.model", "").startswith("claude-haiku")][0]
    assert sub_llm.parent.span_id == task_span.get_span_context().span_id, (
        "a subagent's call shows as the Task's work"
    )
    assert by_name["subagent.researcher"][0].attributes["gen_ai.agent.name"] == "researcher"


@pytest.mark.asyncio
async def test_pre_tool_use_maps_checkpoint_outcomes():
    quiet = ReeveClaudeClient(RecordingSdk())
    assert await quiet._on_pre_tool_use({"tool_name": "Read"}, "t1", None) == {}
    assert quiet._sdk.checkpoints == 1

    redirected = ReeveClaudeClient(
        RecordingSdk(CheckpointResult.Redirect("focus on the tests"))
    )
    out = await redirected._on_pre_tool_use({"tool_name": "Read"}, "t1", None)
    ctx = out["hookSpecificOutput"]["additionalContext"]
    assert "focus on the tests" in ctx
    assert "not in question" in ctx, "steering, never blame"

    killed = ReeveClaudeClient(RecordingSdk(AgentKilled()))
    out = await killed._on_pre_tool_use({"tool_name": "Read"}, "t1", None)
    assert out["continue_"] is False


@pytest.mark.asyncio
async def test_pre_compact_marks_the_open_root():
    client = ReeveClaudeClient(RecordingSdk())
    client._observe(
        AssistantMessage(content=[TextBlock(text="hi")], model="claude-fable-5")
    )
    await client._on_pre_compact({"trigger": "auto"}, None, None)
    client._observe(result_message())
    root = {s.name: s for s in _EXPORTER.get_finished_spans()}["claude.session"]
    assert root.attributes["reeve.context.compacted"] == "auto"


def test_user_hooks_survive_and_matchers_get_a_long_timeout():
    mine = HookMatcher(matcher="Bash", hooks=[lambda *a: {}])
    options = ClaudeAgentOptions(hooks={"PreToolUse": [mine]})
    client = ReeveClaudeClient(RecordingSdk(), options=options)
    matchers = client._client.options.hooks["PreToolUse"]
    assert matchers[0] is mine, "the user's own hooks stay first"
    assert len(matchers) == 2
    assert matchers[1].timeout > 60, "a pause hold must outlive the default timeout"
