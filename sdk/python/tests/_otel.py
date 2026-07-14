"""Shared OTel test provider.

set_tracer_provider only honors the first call in a process, so every
test file asserting on spans must share this exporter; a second
per-file provider would silently receive nothing.
"""

from opentelemetry import trace as otel_trace
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import SimpleSpanProcessor
from opentelemetry.sdk.trace.export.in_memory_span_exporter import (
    InMemorySpanExporter,
)

EXPORTER = InMemorySpanExporter()
_PROVIDER = TracerProvider()
_PROVIDER.add_span_processor(SimpleSpanProcessor(EXPORTER))
otel_trace.set_tracer_provider(_PROVIDER)
