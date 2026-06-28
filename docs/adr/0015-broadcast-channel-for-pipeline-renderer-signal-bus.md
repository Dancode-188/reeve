# 0015: `broadcast::channel` for the Pipeline-to-Renderer Signal Bus

**Status:** Accepted
**Date:** 2026-06-28

## Context

The ingestion pipeline (route stage) needs to notify the renderer
when things happen: an agent connects for the first time, a trace
completes, a streaming token arrives. The renderer is a separate
async task with no direct access to pipeline internals.

A channel is the natural bridge. The question is which kind.

`mpsc` (multi-producer, single-consumer) allows exactly one consumer.
If a second consumer ever needs the same events (the engine, a
WebSocket feed, a test harness), the pipeline needs to be
re-architected to fan out manually.

`watch` carries only the latest value. It cannot represent a stream
of distinct events. A completed trace replaces the previous value
rather than queuing alongside it.

`oneshot` is single-use. Not applicable for a continuous event stream.

`broadcast` is multi-producer, multi-consumer. Every sender call
delivers the message to all active receivers. When a receiver falls
behind, the channel drops old messages rather than blocking the
sender. Ingestion throughput is never constrained by the renderer's
frame rate.

## Decision

`tokio::sync::broadcast` with a capacity of 256 events carries all
`EngineSignal` variants from the pipeline to the renderer. The
`Sender` lives in the route stage and is passed into it at startup.
The `Receiver` is cloned for each consumer; v0.1.0 has one consumer
(the renderer). `EngineSignal` derives `Clone` because broadcast
clones the message for each receiver.

Dropping old messages when a consumer falls behind is intentional.
The renderer is a monitor. Missing a signal because the render loop
stalled for a frame is acceptable. Blocking ingestion because the
renderer is slow is not.

## Consequences

**What gets easier:**
- Adding a second consumer (engine, test harness, WebSocket feed)
  requires cloning the sender and subscribing a new receiver.
  No restructuring of the pipeline.
- The ingestion pipeline never blocks on the renderer. Slow rendering
  drops signals silently, logged as a warning, and self-corrects.

**What gets harder:**
- `EngineSignal` must implement `Clone`. Variants that carry large
  payloads (streaming content) clone that payload for every receiver.
  At v0.1.0 with one receiver this costs nothing. With multiple
  receivers it becomes a concern if payloads grow large.
- `broadcast` channels have a fixed ring-buffer capacity. Bursting
  past 256 events per frame will cause lag warnings. The right
  response is to increase capacity or add backpressure, not to change
  channel type.

## Alternatives considered

**`mpsc` with manual fan-out (rejected):** The route stage would hold
a `Vec<mpsc::Sender<EngineSignal>>` and clone the message for each
registered receiver. Achieves the same effect as `broadcast` but
requires the pipeline to manage the subscriber list. `broadcast`
handles this for free.

**`watch` (rejected):** Cannot represent a stream of distinct events.
Two `TraceCompleted` signals for different traces would overwrite each
other.

**Shared `AppState` behind a `Mutex` (rejected):** The pipeline
directly mutates renderer state. Eliminates the channel entirely but
couples ingestion to the renderer's data model. Pipeline throughput
depends on the renderer lock.
