# 0001: Two-Channel Architecture

**Status:** Accepted
**Date:** 2026-06-23

## Context

Reeve needs to do two fundamentally different things: observe what agents are
doing (passive, one-way, high-volume telemetry) and intervene in what agents
are doing (active, bidirectional, low-latency commands). These have different
requirements.

Observation traffic is continuous, potentially high-volume, and loss-tolerant.
Missing one span in a thousand does not break anything. What matters is
throughput and latency of data delivery.

Intervention traffic is the opposite: low-volume, zero-loss required,
bidirectional, with strict acknowledgment semantics. When Reeve tells an agent
to pause, it needs to know the agent received and acted on that command. A
one-way channel cannot provide this.

The question was whether to use a single protocol for both, or two separate
channels with different characteristics.

## Decision

Use two separate channels:

1. **Observation channel** (OTel/OTLP): standard OpenTelemetry protocol over
   gRPC (port 4317) and HTTP (port 4318). Agents emit telemetry using existing
   OTel SDKs with no Reeve-specific code required.

2. **Intervention channel** (gRPC bidirectional streaming, port 4316): custom
   protocol defined in `proto/reeve.proto`. Agents that want intervention
   capability import the Reeve SDK and maintain a persistent bidirectional
   stream for command/ack exchange.

## Consequences

**What gets easier:**
- Agents can integrate observation-only with zero Reeve-specific dependencies.
  Just configure their existing OTel SDK to point at Reeve's OTLP endpoint.
- The intervention channel can have strict exactly-once delivery semantics
  without constraining the observation channel.
- Ports and protocols are clearly separated, which simplifies firewall rules
  and deployment documentation.
- The observation channel is compatible with any existing OTel infrastructure.
  Agents already instrumented for Datadog, Honeycomb, etc., can point at
  Reeve in parallel.

**What gets harder:**
- Two ports to document and expose in production deployments.
- Two separate protocol definitions to maintain.
- SDK implementation must manage two connections, which adds complexity for
  agents that want full intervention capability.

## Alternatives considered

**Single gRPC channel for both (rejected):** Would require every agent to use
the Reeve SDK even for observation-only use. This breaks compatibility with
existing OTel instrumentation and raises the barrier to a first integration
significantly.

**WebSocket for intervention (rejected):** WebSockets work, but gRPC
bidirectional streaming gives us typed protobuf messages, built-in flow
control, and better tooling for service health and reconnection logic. The
intervention channel is internal (SDK to Reeve, not browser to Reeve), so
HTTP upgrade semantics add nothing.

**HTTP polling for intervention (rejected):** Polling introduces latency that
makes real-time intervention feel sluggish. A pause command that takes 500ms
to be picked up is not a pause command.
