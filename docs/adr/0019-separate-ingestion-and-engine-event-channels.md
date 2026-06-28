# 0019: Separate IngestionEvent and EngineEvent Channels

**Status:** Accepted
**Date:** 2026-06-28

## Context

v0.1.0 shipped with a single `EngineSignal` broadcast channel carrying all
runtime events from the ingestion pipeline to its consumers. The renderer
subscribed to this channel and drove its display from it.

v0.2.0 adds the evaluation engine. The engine needs to consume ingestion
events to evaluate spans as they arrive, and emit evaluation results back
to the renderer: health scores, policy alerts, tier-2 pending state. A
single channel cannot serve both directions cleanly. More fundamentally,
`EngineSignal` was misleading from the start. The ingestion pipeline
produces it, not the engine. A name that implies the wrong producer is a
liability that grows as the codebase does.

Two channels, each named for its producer, is the clean answer.

## Decision

Rename `EngineSignal` to `IngestionEvent` in `reeve-model`. The name now
matches the producer: the route stage in `reeve-ingestion`.

Add `EngineEvent` to `reeve-model` with three variants: `EvaluationComplete`
carrying a metric name and score for a span or trace, `HealthScoreUpdated`
carrying the computed composite score and a `tier2_pending` flag so the
renderer knows whether to show a settled number or the pulsing scoring
animation, and `PolicyAlert` carrying the rule ID, command type, and
confirmation requirement.

Both enums carry doc comments stating the producer and consumers explicitly
so the signal topology is readable without tracing call sites.

Both channels are constructed in `main.rs` with explicit, named subscriptions.
Every consumer is visible in one place without opening another file. Adding
a consumer is one `subscribe()` call.

`reeve-model` takes no tokio dependency. This is a standing principle: the
model crate defines shared data types. Runtime primitives belong in the crates
that use them. Once a runtime dependency enters the model crate, the boundary
does not come back.

## Consequences

**What gets easier:**
- The signal topology is readable in `main.rs` without tracing imports.
  Every sender, every receiver, every capacity is explicit.
- Adding the intervention channel in v0.3.0 is one broadcast pair and
  one or two named subscriptions. No restructuring required.
- The renderer processes ingestion and evaluation events independently.
  A streaming update does not block on a pending health score evaluation.
- `reeve-model` stays free of runtime dependencies. Compile time stays
  fast, dependency surface stays minimal.

**What gets harder:**
- Two channel types to subscribe to in the renderer instead of one.
  The main loop must drain both.
- `EngineSignal` is renamed, touching the route stage, `main.rs`, and
  the renderer. Small scope, one-time cost, worth it for the clarity.

## Alternatives considered

**Single channel with all variants (rejected):** Adding engine-produced
variants to `IngestionEvent` means one channel with two producers and no
clear ownership. Receivers pattern-match on variants they do not care about.
The name becomes actively wrong: produced by both ingestion and the engine.

**`EventBus` struct in `reeve-model` (rejected):** A struct wrapping both
channels and handing out typed receivers is a clean API but it requires
tokio in `reeve-model`. Wiring is not model. The explicit construction in
`main.rs` is not boilerplate to be hidden; it is architectural documentation.
