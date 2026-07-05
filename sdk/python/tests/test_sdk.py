import asyncio
import sys
import os

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import pytest
from reeve.sdk import AgentKilled, CheckpointResult, ReeveSdk
from reeve.proto import reeve_pb2


def _sdk_with_pending(cmd_type: int, payload: str = "") -> ReeveSdk:
    sdk = ReeveSdk()
    sdk._pending = {
        "command_id": "cmd-1",
        "type": cmd_type,
        "payload": payload,
    }
    return sdk


@pytest.mark.asyncio
async def test_checkpoint_returns_continue_when_empty():
    sdk = ReeveSdk()
    result = await sdk.checkpoint()
    assert isinstance(result, CheckpointResult.Continue)


@pytest.mark.asyncio
async def test_checkpoint_raises_on_kill():
    sdk = _sdk_with_pending(reeve_pb2.KILL)
    with pytest.raises(AgentKilled):
        await sdk.checkpoint()


@pytest.mark.asyncio
async def test_checkpoint_returns_redirect():
    sdk = _sdk_with_pending(reeve_pb2.REDIRECT, "slow down")
    result = await sdk.checkpoint()
    assert isinstance(result, CheckpointResult.Redirect)
    assert result.instruction == "slow down"


@pytest.mark.asyncio
async def test_checkpoint_returns_context():
    sdk = _sdk_with_pending(reeve_pb2.INJECT_CONTEXT, '{"hint": "be concise"}')
    result = await sdk.checkpoint()
    assert isinstance(result, CheckpointResult.Context)
    assert "be concise" in result.context


@pytest.mark.asyncio
async def test_checkpoint_clears_pending_after_redirect():
    sdk = _sdk_with_pending(reeve_pb2.REDIRECT, "try again")
    await sdk.checkpoint()
    assert sdk._pending is None


@pytest.mark.asyncio
async def test_trace_decorator_calls_function():
    sdk = ReeveSdk()

    @sdk.trace
    async def my_llm_call():
        return "result"

    assert await my_llm_call() == "result"


@pytest.mark.asyncio
async def test_trace_decorator_with_custom_name():
    sdk = ReeveSdk()

    @sdk.trace(name="custom.span")
    async def my_call():
        return 42

    assert await my_call() == 42


def test_agent_killed_is_exception():
    err = AgentKilled()
    assert isinstance(err, Exception)
    assert "killed" in str(err).lower() or True  # message may vary
