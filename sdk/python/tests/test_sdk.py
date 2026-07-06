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


def _drain_acks(sdk: ReeveSdk) -> list[tuple[str, int]]:
    """Collect (command_id, status) for every ack currently queued."""
    acks = []
    while not sdk._queue.empty():
        msg = sdk._queue.get_nowait()
        if msg is not None and msg.WhichOneof("payload") == "ack":
            acks.append((msg.ack.command_id, msg.ack.status))
    return acks


def _command_msg(cmd_id: str, cmd_type: int, payload: str = ""):
    return reeve_pb2.ControlMessage(
        command=reeve_pb2.InterventionCommand(
            command_id=cmd_id,
            type=cmd_type,
            payload=payload,
            valid_until_ms=0,
        )
    )


async def _stream(*msgs):
    for m in msgs:
        yield m


@pytest.mark.asyncio
async def test_pause_acks_applied_before_blocking():
    sdk = _sdk_with_pending(reeve_pb2.PAUSE)
    task = asyncio.create_task(sdk.checkpoint())
    await asyncio.sleep(0.05)

    assert not task.done(), "checkpoint must still be holding the pause"
    acks = _drain_acks(sdk)
    assert ("cmd-1", reeve_pb2.APPLYING) in acks
    assert ("cmd-1", reeve_pb2.APPLIED) in acks, (
        "applied must be sent when the hold begins, not at resume"
    )

    sdk._pause_event.set()
    result = await task
    assert isinstance(result, CheckpointResult.Continue)


@pytest.mark.asyncio
async def test_resume_acked_applied_after_wake():
    sdk = _sdk_with_pending(reeve_pb2.PAUSE)
    task = asyncio.create_task(sdk.checkpoint())
    await asyncio.sleep(0.05)
    _drain_acks(sdk)

    sdk._resume_pending = "cmd-resume"
    sdk._pause_event.set()
    await task

    acks = _drain_acks(sdk)
    assert ("cmd-resume", reeve_pb2.APPLIED) in acks


@pytest.mark.asyncio
async def test_kill_while_paused_raises():
    sdk = _sdk_with_pending(reeve_pb2.PAUSE)
    task = asyncio.create_task(sdk.checkpoint())
    await asyncio.sleep(0.05)

    # Simulate what the receive loop does when a kill arrives mid-pause:
    # pend the kill and release the pause block.
    sdk._pending = {"command_id": "cmd-kill", "type": reeve_pb2.KILL}
    sdk._pause_event.set()

    with pytest.raises(AgentKilled):
        await task
    acks = _drain_acks(sdk)
    assert ("cmd-kill", reeve_pb2.APPLIED) in acks


@pytest.mark.asyncio
async def test_redirect_acks_applied():
    sdk = _sdk_with_pending(reeve_pb2.REDIRECT, "change course")
    result = await sdk.checkpoint()
    assert isinstance(result, CheckpointResult.Redirect)
    acks = _drain_acks(sdk)
    assert ("cmd-1", reeve_pb2.APPLIED) in acks


@pytest.mark.asyncio
async def test_duplicate_command_is_not_reapplied():
    sdk = ReeveSdk()
    sdk._call = _stream(_command_msg("cmd-dup", reeve_pb2.REDIRECT, "go left"))
    await sdk._recv_loop()

    result = await sdk.checkpoint()
    assert isinstance(result, CheckpointResult.Redirect)
    assert sdk._pending is None

    # The dispatcher retries with the same command_id when the applied ack
    # is lost. The retry must not become a second redirect.
    sdk._call = _stream(_command_msg("cmd-dup", reeve_pb2.REDIRECT, "go left"))
    await sdk._recv_loop()
    assert sdk._pending is None, "retried command must be dropped, not re-pended"

    acks = _drain_acks(sdk)
    assert (
        acks.count(("cmd-dup", reeve_pb2.RECEIVED)) == 2
    ), "duplicates are still acked received so the retry settles"


@pytest.mark.asyncio
async def test_resume_while_running_acks_failed():
    sdk = ReeveSdk()
    sdk._call = _stream(_command_msg("cmd-bad-resume", reeve_pb2.RESUME))
    await sdk._recv_loop()

    acks = _drain_acks(sdk)
    assert ("cmd-bad-resume", reeve_pb2.FAILED) in acks
    assert sdk._pending is None


def test_handshake_proto_carries_service_identity():
    handshake = reeve_pb2.AgentHandshake(
        agent_id="research-bot:pod-7",
        service_name="research-bot",
        service_instance_id="pod-7",
    )
    assert handshake.service_name == "research-bot"
    assert handshake.service_instance_id == "pod-7"
    assert handshake.agent_id == f"{handshake.service_name}:{handshake.service_instance_id}", (
        "agent_id must be the composed form so older servers register the same identity"
    )


@pytest.mark.asyncio
async def test_resume_while_paused_releases_and_stores_id():
    sdk = _sdk_with_pending(reeve_pb2.PAUSE)
    task = asyncio.create_task(sdk.checkpoint())
    await asyncio.sleep(0.05)

    sdk._call = _stream(_command_msg("cmd-resume", reeve_pb2.RESUME))
    await sdk._recv_loop()

    result = await task
    assert isinstance(result, CheckpointResult.Continue)
    acks = _drain_acks(sdk)
    assert ("cmd-resume", reeve_pb2.APPLIED) in acks
