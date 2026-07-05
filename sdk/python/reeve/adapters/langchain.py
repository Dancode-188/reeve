from __future__ import annotations

from typing import TYPE_CHECKING, Any
from uuid import UUID

from langchain_core.callbacks.base import AsyncCallbackHandler
from langchain_core.outputs import LLMResult
from opentelemetry import trace as otel_trace

if TYPE_CHECKING:
    from ..sdk import ReeveSdk


class ReeveCallbacks(AsyncCallbackHandler):
    """LangChain callback handler that wires checkpoint() and OTel spans into a chain."""

    def __init__(self, sdk: "ReeveSdk") -> None:
        super().__init__()
        self._sdk = sdk
        self._tracer = otel_trace.get_tracer("reeve-sdk")
        self._llm_spans: dict[str, otel_trace.Span] = {}
        self._tool_spans: dict[str, otel_trace.Span] = {}

    async def on_llm_start(
        self,
        serialized: dict[str, Any],
        prompts: list[str],
        *,
        run_id: UUID,
        **kwargs: Any,
    ) -> None:
        await self._sdk.checkpoint()
        span = self._tracer.start_span("llm.call")
        span.set_attribute("gen_ai.operation.name", "chat")
        model = (serialized.get("kwargs") or {}).get("model_name")
        if model:
            span.set_attribute("gen_ai.request.model", model)
        self._llm_spans[str(run_id)] = span

    async def on_llm_end(
        self,
        response: LLMResult,
        *,
        run_id: UUID,
        **kwargs: Any,
    ) -> None:
        span = self._llm_spans.pop(str(run_id), None)
        if span is None:
            return
        if response.llm_output:
            usage = response.llm_output.get("token_usage") or {}
            total = usage.get("total_tokens")
            if total is not None:
                span.set_attribute("gen_ai.usage.total_tokens", int(total))
        span.end()

    async def on_tool_start(
        self,
        serialized: dict[str, Any],
        input_str: str,
        *,
        run_id: UUID,
        **kwargs: Any,
    ) -> None:
        name = serialized.get("name") or "tool"
        span = self._tracer.start_span(name)
        span.set_attribute("gen_ai.operation.name", "tool_call")
        self._tool_spans[str(run_id)] = span
        await self._sdk.checkpoint()

    async def on_tool_end(
        self,
        output: Any,
        *,
        run_id: UUID,
        **kwargs: Any,
    ) -> None:
        span = self._tool_spans.pop(str(run_id), None)
        if span is not None:
            span.end()
        await self._sdk.checkpoint()
