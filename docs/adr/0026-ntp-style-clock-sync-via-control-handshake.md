# 0026: NTP-Style Clock Synchronization via the Control Handshake

**Status:** Accepted
**Date:** 2026-07-05

## Context

Reeve timestamps every span with the agent's clock and then applies a
correction so spans sort correctly against Reeve's own wall clock. Before
the control channel existed, the only way to estimate that correction was
to observe the minimum of `arrived_at - span_end_time` across the first
ten spans from a newly connected agent (ADR-0004). That approach conflates
clock skew with one-way network latency. There is no round trip, so the
two cannot be separated.

The `AgentHandshake` exchange on the gRPC control channel provides a
genuine bidirectional round trip with clock readings from both sides. This
is the same setup that NTP uses, which means the four-timestamp formula
gives a real offset rather than an approximation inflated by network
latency.

## Decision

When an agent connects to the control channel, both sides record
timestamps at each of the four events in the exchange:

- T1: agent-side timestamp when the handshake request is sent
- T2: Reeve-side timestamp when the handshake request is received
- T3: Reeve-side timestamp when the `HandshakeAck` is sent
- T4: agent-side timestamp when the `HandshakeAck` is received

The agent sends T4 back to Reeve in a `NtpFollowup` message. Reeve
then computes:

```
offset_ms = ((T2 - T1) + (T3 - T4)) / 2
```

The computed offset is written into a shared `Arc<Mutex<HashMap<String, i64>>>`
keyed by `agent_id`. The OTLP receiver reads from that map when
processing spans. If an NTP offset is present for the agent's
`service.instance.id`, it is used in place of the sample-based estimate.
If the `NtpFollowup` has not arrived yet, the receiver falls back to the
ADR-0004 approximation so spans are not held up waiting for the
four-timestamp exchange to complete.

The shared map is created in `main.rs` and passed to both
`reeve-intervention` (writer) and `reeve-ingestion` (reader). No new
event type is needed on the signal bus.

## Consequences

**What gets easier:**
- Clock offsets for agents that complete the handshake exchange are
  accurate regardless of network latency between agent and Reeve.
- The OTLP receiver requires no protocol changes. It reads the offset
  from a map it already holds a reference to.
- The fallback to sample-based estimation means existing integrations
  that do not yet send `NtpFollowup` continue to work without changes.

**What gets harder:**
- The four-timestamp exchange assumes the network is symmetric: that
  the latency from agent to Reeve is roughly equal to the latency from
  Reeve to agent. On asymmetric networks the computed offset will be
  off by half the latency difference. NTP has the same assumption and
  the same limitation.
- Agents that connect to the control channel but never send `NtpFollowup`
  will fall back to the ADR-0004 approximation indefinitely. The Rust SDK
  sends the followup; adapters in other languages must implement it.

## Alternatives considered

**Keeping the ADR-0004 approximation permanently (rejected):** The minimum
sample approach is simple but wrong. It adds at least one-way network
latency to the offset estimate, so spans from remote agents will appear
slightly earlier than they actually occurred relative to Reeve's clock.
The error grows with physical distance between agent and Reeve.

**Emitting a new `ClockOffsetReady` engine event (rejected):** Routing
the offset through the engine event bus would require `reeve-ingestion`
to subscribe to engine events, which it currently does not do. The shared
map achieves the same result with less architectural change and no new
event type.
