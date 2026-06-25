# 0004: Connection-Time Clock Offset Estimation

**Status:** Accepted
**Date:** 2026-06-25

## Context

Spans arrive carrying timestamps set by the agent's own clock
(`start_time`/`end_time`). When Reeve and the agent run on different
machines, those clocks can disagree, which would otherwise show a span's
reported time slightly ahead of or behind Reeve's own clock.

The original design called for correcting this with Cristian's algorithm.
That algorithm requires a round trip: send a request, receive a response
carrying the other side's clock reading, and use half the round-trip time
to estimate one-way latency. OTLP export is one-directional. The agent
sends spans, Reeve acknowledges receipt, and no exchange in normal
operation carries a clock reading back to Reeve in a way that supports
that calculation. The one round trip that does exist, the gRPC health
check, only returns a status enum with no timestamp field. There is no
real round trip available yet to apply the named algorithm to.

## Decision

For the first slice of the ingestion pipeline, Reeve estimates a per-agent
clock offset by taking the minimum of `arrived_at - reported end_time`
across the first 10 spans from a newly connected agent, then applies that
fixed offset to all subsequent spans from that connection. This is called
"connection-time clock offset estimation" in code and documentation, not
Cristian's algorithm, since it is not that algorithm.

The minimum, not the mean or the first sample alone, is the deliberate
choice. Across several samples, the one with the smallest observed delay
has the least queueing and transmission noise mixed in, so it is the
closest approximation to pure clock skew available without a real round
trip.

The offset is supplementary display information only. It is never used to
determine trace assembly order, span parent-child relationships, or
completion detection. Those all rely on causal ordering (which span
arrived after which) and `arrived_at` (Reeve's own clock), neither of
which depends on cross-machine clock agreement at all.

Once the intervention control channel exists (v0.3.0), the
`AgentHandshake` exchange on that channel gives a real bidirectional
round trip with clock readings on both sides. That makes genuine
NTP-style four-timestamp synchronization possible:
`offset = ((T2 - T1) + (T3 - T4)) / 2`, where T1/T4 are agent timestamps
and T2/T3 are Reeve timestamps around the handshake. That replacement is
tracked as a separate issue rather than built now, since the control
channel and SDK adapters it depends on do not exist yet.

## Consequences

**What gets easier:**
- No new infrastructure needed. Works with the OTLP receiver alone.
- Honest naming avoids a misleading comment that claims an algorithm the
  code does not actually implement.

**What gets harder:**
- The estimate conflates clock skew with network latency, since there is
  no way to separate them without a real round trip. For Reeve's common
  case (agent and Reeve on the same machine or local network), this
  barely matters: latency is small enough that the conflation is
  negligible.
- The offset is necessarily wrong by some amount for genuinely remote
  agents (a cloud VM, an SSH session) until the v0.3.0 replacement lands.

**Acceptable tradeoff:**
- Nothing depends on this offset being exactly right. It only affects the
  timestamp displayed next to a span in the cockpit, not how spans are
  ordered, assembled into traces, or evaluated.

## Alternatives considered

**Do nothing, skip clock correction entirely (rejected for now):** Correct
in spirit for the common same-machine case, where skew is zero by
definition, but it silently drops the multi-machine case described above,
which is the actual reason clock alignment exists at all.

**Single first-span sample (rejected):** Simpler than the minimum filter
over 10 spans, but a single sample has no protection against an unlucky
first measurement landing on a slow span.

**Causal ordering only, no clock offset at all (partially adopted as a
principle, not as the full answer):** Correctly observes that duration
calculations, replay ordering, and trace tree structure never need
cross-clock alignment in the first place. Adopted as the governing
principle for how the offset gets used. Not adopted as a reason to skip
computing an offset entirely, since the cockpit still displays an
absolute timestamp next to each span, and that display benefits from
correction even though nothing else depends on it.

**NTP four-timestamp sync via the control channel now (rejected for
now):** The technically correct fix, and the eventual replacement. Not
possible yet because it depends on the control channel and SDK adapters,
which are v0.3.0 work, not yet built.
