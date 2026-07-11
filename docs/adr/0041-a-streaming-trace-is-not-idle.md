# 0041: A Streaming Trace Is Not Idle

**Status:** Accepted
**Date:** 2026-07-11

## Context

The assembler flushes a trace as interrupted after 30 quiet seconds,
where quiet means no spans arrived. That definition assumed spans flow
steadily while an agent works, which held for every SDK agent and mock
the pipeline was built against.

Streaming breaks the assumption. A model can generate one response for
minutes while the proxy emits nothing into the pipeline, because a
span is synthesized only when its round trip finishes. During a real
8-minute Claude Code turn the idle timeout fired repeatedly mid-turn,
and each flush dropped the spans still waiting for their turn root,
which only arrives when the turn ends. Most of the session's spans
were silently lost (#182).

Trace states and their transitions are ADR-0014; this decision changes
what counts as evidence for the idle transition.

## Decision

**Idle means no evidence of life, not no spans.** The proxy marks a
trace while a Messages round trip is in flight, in a shared map read
by the assembler's tick (the same pattern the paused set uses), and a
marked trace is refreshed each tick: it cannot reach the idle timeout
no matter how long the model streams.

- **The proxy owns the marking** because it is the component that
  knows a round trip is in flight. The mark is a per-trace count, so
  concurrent requests on one turn cannot clear each other's exemption.
- **The mark clears on every exit path**, upstream errors, client
  disconnects, and stream timeouts included, and only after the
  finalized span has entered the pipeline. Clearing earlier would
  reopen a small window of the same hole.
- **The companion invariant:** a span that entered the pipeline
  reaches storage. Every flush path persists parent-pending spans;
  they carry a trace id, and the tree renders them as awaiting rows.

## Consequences

**What gets easier:**
- Long streamed turns survive intact: the 8-minute turn that lost
  most of its spans lands as one complete trace.
- The idle timeout keeps its meaning for agents that actually died:
  nothing marks their traces, so silence still flushes them.

**What gets harder:**
- A hung upstream that never times out would hold its trace in flight
  indefinitely; the stream chunk timeout bounds this at 30 seconds of
  wire silence, so the exemption cannot outlive a dead connection.
- SDK agents get no such exemption, because nothing on that path can
  vouch for in-flight work. An SDK agent with a quiet gap longer than
  the idle timeout still flushes early; adapters that stream should
  emit progress heartbeats, which is future SDK work.

## Alternatives considered

- **Lengthen the idle timeout (rejected):** any fixed value loses to
  a longer generation, and a value long enough for the worst case
  makes genuinely dead traces linger for minutes on every screen.
- **Emit heartbeat spans during streams (rejected):** pollutes traces
  with synthetic spans that exist only to feed a timer, and every
  consumer downstream would need to learn to ignore them.
- **Flush pending spans but keep the trace open (rejected):** saves
  the data but completes the same trace twice, which the storage
  layer's completed-never-downgrades rule exists to prevent.
