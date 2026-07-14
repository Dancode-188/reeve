from __future__ import annotations

import asyncio
import time
from functools import wraps
from typing import Any, AsyncIterator, Callable

import grpc
import grpc.aio
from opentelemetry import trace as otel_trace
from opentelemetry.exporter.otlp.proto.grpc.trace_exporter import OTLPSpanExporter
from opentelemetry.sdk.resources import Resource
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import BatchSpanProcessor

from .proto import reeve_pb2, reeve_pb2_grpc


class AgentKilled(Exception):
    """Raised by checkpoint() when Reeve sends a Kill command."""


class CheckpointResult:
    class Continue:
        pass

    class Redirect:
        def __init__(self, instruction: str) -> None:
            self.instruction = instruction

    class Context:
        def __init__(self, context: str) -> None:
            self.context = context


class ReeveSdk:
    def __init__(self) -> None:
        self._pending: dict[str, Any] | None = None
        # Set means running, cleared means holding at a paused checkpoint.
        # Starts set so is_set() doubles as the pause-state check.
        self._pause_event: asyncio.Event = asyncio.Event()
        self._pause_event.set()
        self._resume_pending: str | None = None
        self._seen_commands: set[str] = set()
        self._queue: asyncio.Queue = asyncio.Queue()
        self._call: Any = None
        self._tracer: otel_trace.Tracer | None = None

    @classmethod
    async def connect(
        cls,
        agent_name: str,
        *,
        instance_id: str | None = None,
        framework: str = "custom",
        capabilities: list[str] | None = None,
        host: str = "127.0.0.1",
    ) -> "ReeveSdk":
        sdk = cls()

        # The OTel Resource and the handshake must carry the same identity.
        # Reeve derives the agent id from these two values on both channels;
        # if they differ, commands can never route back to this connection.
        if instance_id is None:
            instance_id = format(time.time_ns(), "x")

        exporter = OTLPSpanExporter(endpoint=f"http://{host}:4317", insecure=True)
        provider = TracerProvider(
            resource=Resource.create(
                {
                    "service.name": agent_name,
                    "service.instance.id": instance_id,
                }
            )
        )
        # A 1s flush instead of the OTel default 5s: an agent appears in
        # the cockpit on its first exported span, and five invisible
        # seconds is most of a short run, long enough that there is no
        # agent to intervene on yet.
        provider.add_span_processor(
            BatchSpanProcessor(exporter, schedule_delay_millis=1000)
        )
        otel_trace.set_tracer_provider(provider)
        sdk._tracer = otel_trace.get_tracer("reeve-sdk")

        channel = grpc.aio.insecure_channel(f"{host}:4316")
        stub = reeve_pb2_grpc.ReeveControlStub(channel)

        t1_ms = _now_ms()
        await sdk._queue.put(
            reeve_pb2.AgentMessage(
                handshake=reeve_pb2.AgentHandshake(
                    agent_id=f"{agent_name}:{instance_id}",
                    framework=framework,
                    sdk_version="0.1.0",
                    capabilities=capabilities
                    or ["pause", "redirect", "inject_context", "kill"],
                    t1_ms=t1_ms,
                    service_name=agent_name,
                    service_instance_id=instance_id,
                )
            )
        )

        sdk._call = stub.ControlStream(_queue_stream(sdk._queue))
        asyncio.create_task(sdk._recv_loop())
        asyncio.create_task(sdk._heartbeat_loop())

        return sdk

    async def checkpoint(
        self,
    ) -> CheckpointResult.Continue | CheckpointResult.Redirect | CheckpointResult.Context:
        while True:
            cmd = self._pending
            if cmd is None:
                return CheckpointResult.Continue()
            self._pending = None

            cmd_type = cmd["type"]
            command_id = cmd["command_id"]

            if cmd_type == reeve_pb2.KILL:
                await self._ack(command_id, reeve_pb2.APPLIED)
                # The ack sits in the send queue; raising immediately tears
                # down the event loop before the sender task can put it on
                # the wire, and the kill then shows as expired in the audit
                # trail instead of applied. One short yield lets it flush.
                await asyncio.sleep(0.1)
                raise AgentKilled()
            elif cmd_type == reeve_pb2.PAUSE:
                await self._ack(command_id, reeve_pb2.APPLYING)
                self._pause_event.clear()
                # Applied means the agent has confirmed it is holding at a
                # yield point, which is true from this moment. Reeve's pause
                # tracking flips on this ack; sending it after resume instead
                # makes every pause look like a command that timed out.
                await self._ack(command_id, reeve_pb2.APPLIED)
                await self._pause_event.wait()
                if self._resume_pending is not None:
                    resume_id = self._resume_pending
                    self._resume_pending = None
                    await self._ack(resume_id, reeve_pb2.APPLIED)
                # loop back: another command may have arrived while paused
            elif cmd_type == reeve_pb2.REDIRECT:
                await self._ack(command_id, reeve_pb2.APPLYING)
                await self._ack(command_id, reeve_pb2.APPLIED)
                return CheckpointResult.Redirect(cmd["payload"])
            elif cmd_type == reeve_pb2.INJECT_CONTEXT:
                await self._ack(command_id, reeve_pb2.APPLYING)
                await self._ack(command_id, reeve_pb2.APPLIED)
                return CheckpointResult.Context(cmd["payload"])

    def trace(self, func: Callable | None = None, *, name: str | None = None) -> Any:
        """Decorator that wraps a coroutine in a gen_ai.chat span."""

        def decorator(f: Callable) -> Callable:
            @wraps(f)
            async def wrapper(*args: Any, **kwargs: Any) -> Any:
                span_name = name or f.__name__
                tracer = self._tracer or otel_trace.get_tracer("reeve-sdk")
                with tracer.start_as_current_span(span_name) as span:
                    span.set_attribute("gen_ai.operation.name", "chat")
                    return await f(*args, **kwargs)

            return wrapper

        if func is not None:
            return decorator(func)
        return decorator

    async def _recv_loop(self) -> None:
        async for msg in self._call:
            kind = msg.WhichOneof("payload")
            if kind == "handshake_ack":
                t4_ms = _now_ms()
                await self._send(
                    reeve_pb2.AgentMessage(
                        ntp_followup=reeve_pb2.NtpFollowup(t4_ms=t4_ms)
                    )
                )
            elif kind == "command":
                cmd = msg.command
                if cmd.command_id in self._seen_commands:
                    # A network retry of a command already handled. Discard
                    # silently with a RECEIVED ack; re-applying it is worse
                    # than the retry (a redirect would land twice).
                    await self._ack(cmd.command_id, reeve_pb2.RECEIVED)
                    continue
                if cmd.valid_until_ms > 0 and _now_ms() > cmd.valid_until_ms:
                    await self._ack(cmd.command_id, reeve_pb2.EXPIRED)
                    continue
                self._seen_commands.add(cmd.command_id)
                await self._ack(cmd.command_id, reeve_pb2.RECEIVED)
                if cmd.type == reeve_pb2.RESUME:
                    if self._pause_event.is_set():
                        # Not paused; refuse the invalid transition.
                        await self._ack(cmd.command_id, reeve_pb2.FAILED)
                    else:
                        # The woken checkpoint acks this applied once the
                        # agent is actually running again.
                        self._resume_pending = cmd.command_id
                        self._pause_event.set()
                elif cmd.type == reeve_pb2.KILL:
                    self._pending = {"command_id": cmd.command_id, "type": cmd.type}
                    # A paused checkpoint is blocked on the pause event and
                    # only processes commands when awake. Kill must release
                    # the block or a paused agent can never be killed.
                    self._pause_event.set()
                elif cmd.type == reeve_pb2.PAUSE:
                    self._pending = {"command_id": cmd.command_id, "type": cmd.type}
                elif cmd.type in (reeve_pb2.REDIRECT, reeve_pb2.INJECT_CONTEXT):
                    self._pending = {
                        "command_id": cmd.command_id,
                        "type": cmd.type,
                        "payload": cmd.payload,
                    }
            elif kind == "heartbeat":
                await self._send(
                    reeve_pb2.AgentMessage(
                        heartbeat=reeve_pb2.Heartbeat(timestamp_ms=_now_ms())
                    )
                )

    async def _heartbeat_loop(self) -> None:
        while True:
            await asyncio.sleep(30)
            await self._send(
                reeve_pb2.AgentMessage(
                    heartbeat=reeve_pb2.Heartbeat(timestamp_ms=_now_ms())
                )
            )

    async def _send(self, msg: Any) -> None:
        await self._queue.put(msg)

    async def _ack(self, command_id: str, status: int) -> None:
        await self._send(
            reeve_pb2.AgentMessage(
                ack=reeve_pb2.CommandAck(
                    command_id=command_id,
                    status=status,
                    message="",
                )
            )
        )


async def _queue_stream(queue: asyncio.Queue) -> AsyncIterator:
    while True:
        msg = await queue.get()
        if msg is None:
            return
        yield msg


def _now_ms() -> int:
    return int(time.time() * 1000)
