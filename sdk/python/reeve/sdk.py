from __future__ import annotations

import asyncio
import time
from functools import wraps
from typing import Any, AsyncIterator, Callable

import grpc
import grpc.aio
from opentelemetry import trace as otel_trace
from opentelemetry.exporter.otlp.proto.grpc.trace_exporter import OTLPSpanExporter
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
        self._pause_event: asyncio.Event = asyncio.Event()
        self._queue: asyncio.Queue = asyncio.Queue()
        self._call: Any = None
        self._tracer: otel_trace.Tracer | None = None

    @classmethod
    async def connect(
        cls,
        agent_id: str,
        *,
        framework: str = "custom",
        capabilities: list[str] | None = None,
        host: str = "127.0.0.1",
    ) -> "ReeveSdk":
        sdk = cls()

        exporter = OTLPSpanExporter(endpoint=f"http://{host}:4317", insecure=True)
        provider = TracerProvider()
        provider.add_span_processor(BatchSpanProcessor(exporter))
        otel_trace.set_tracer_provider(provider)
        sdk._tracer = otel_trace.get_tracer("reeve-sdk")

        channel = grpc.aio.insecure_channel(f"{host}:4316")
        stub = reeve_pb2_grpc.ReeveControlStub(channel)

        t1_ms = _now_ms()
        await sdk._queue.put(
            reeve_pb2.AgentMessage(
                handshake=reeve_pb2.AgentHandshake(
                    agent_id=agent_id,
                    framework=framework,
                    sdk_version="0.1.0",
                    capabilities=capabilities
                    or ["pause", "redirect", "inject_context", "kill"],
                    t1_ms=t1_ms,
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
                raise AgentKilled()
            elif cmd_type == reeve_pb2.PAUSE:
                await self._ack(command_id, reeve_pb2.APPLYING)
                self._pause_event.clear()
                await self._pause_event.wait()
                await self._ack(command_id, reeve_pb2.APPLIED)
                # loop back: another command may have arrived while paused
            elif cmd_type == reeve_pb2.REDIRECT:
                await self._ack(command_id, reeve_pb2.APPLYING)
                return CheckpointResult.Redirect(cmd["payload"])
            elif cmd_type == reeve_pb2.INJECT_CONTEXT:
                await self._ack(command_id, reeve_pb2.APPLYING)
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
                if cmd.valid_until_ms > 0 and _now_ms() > cmd.valid_until_ms:
                    await self._ack(cmd.command_id, reeve_pb2.EXPIRED)
                    continue
                await self._ack(cmd.command_id, reeve_pb2.RECEIVED)
                if cmd.type == reeve_pb2.RESUME:
                    self._pending = None
                    self._pause_event.set()
                elif cmd.type in (reeve_pb2.KILL, reeve_pb2.PAUSE):
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
