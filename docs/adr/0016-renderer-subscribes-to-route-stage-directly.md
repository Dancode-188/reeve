# 0016: Renderer Subscribes to the Route Stage Directly

**Status:** Accepted
**Date:** 2026-06-28

## Context

The renderer needs a live feed of what the pipeline is doing. It has
to know when agents connect, when traces complete, and when streaming
tokens arrive. The route stage is where all of these events are
produced.

The question is where the subscription happens and what shape it takes.

Three options came up:

1. The renderer polls the `WarmStore` on a timer and detects changes.
2. The route stage writes to a shared event log; consumers tail it.
3. The route stage emits signals on a `broadcast` channel; consumers
   subscribe.

Option 1 couples the renderer to the database schema. It also
introduces latency proportional to the poll interval. A trace that
completes between two polls will appear late or be missed if the
renderer only fetches the latest N rows.

Option 2 requires a durable event log with its own storage overhead
and read cursor per consumer. This is infrastructure Reeve does not
need at v0.1.0.

Option 3 is already in place because of ADR-0015. The route stage
holds a `broadcast::Sender<EngineSignal>`. Adding the renderer as a
subscriber is a `receiver = sender.subscribe()` call in `main.rs`.

## Decision

The renderer subscribes directly to the route stage's broadcast
channel. `main.rs` creates the channel, hands the `Sender` to the
ingestion pipeline, and clones a `Receiver` for the renderer. There
is no intermediary. There is no polling.

When a signal carries only an ID (e.g. `TraceCompleted { trace_id }`),
the renderer fetches the full record from `WarmStore`. Signals are
notifications, not payloads.

## Consequences

**What gets easier:**
- The renderer always sees events within one frame of when they
  happen. No poll interval to tune.
- Adding a second consumer (a WebSocket feed, the engine) follows
  the same pattern: clone the sender and subscribe.

**What gets harder:**
- The renderer can miss signals if it falls behind the ring buffer.
  This is acceptable for a monitor but means UI state is derived
  from the live signal feed plus point-in-time WarmStore reads, not
  from a complete event replay.
- The renderer cannot reconstruct history from the channel alone.
  On startup, it calls `WarmStore::list_agents()` to populate the
  initial agent list. The broadcast channel only carries events from
  the moment the renderer subscribes.

## Alternatives considered

**Poll WarmStore (rejected):** Adds latency, couples the renderer to
the DB schema, and requires the renderer to implement its own
change-detection logic (timestamps, row counts). The signal channel
already carries this information.

**Durable event log (rejected):** Correct at scale. Not needed at
v0.1.0. The broadcast channel is sufficient and introduces no new
infrastructure.
