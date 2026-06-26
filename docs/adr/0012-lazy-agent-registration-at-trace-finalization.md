# 0012: Lazy Agent Registration at Trace Finalization

**Status:** Accepted
**Date:** 2026-06-26

## Context

`traces.agent_id` has a foreign key constraint to `agents`. A trace
cannot be persisted until a matching agent row exists. Something in
the pipeline has to create that row.

The normalize stage is where agent identity is first established: it
reads `service.name`, `service.instance.id`, and framework metadata
from OTel resource attributes and assembles a full `Agent` struct.
But normalize has no storage access. Neither does receive. The first
stage that writes to the warm tier is route.

The assemble stage previously held only `agent_id: AgentId`, enough
to group spans by trace. The route stage had the ID but not the name,
framework, or integration fields needed to create an agent row.

## Decision

The route stage calls `upsert_agent` before `save_trace` on every
finalized trace. `InFlightTrace` now carries a full `Agent` struct
rather than a bare `AgentId`. The value is set when the first span
for a trace arrives in the assemble stage and travels through to
finalization.

The `Router` is stateless. It keeps no in-memory record of which
agents it has already registered. Every finalized trace triggers an
upsert, which is idempotent and naturally updates `last_seen_at` and
`status` on repeated calls.

## Consequences

**What gets easier:**
- No separate agent registration step or connection handshake. Agents
  appear in the warm store the moment their first trace completes.
- Agent liveness (`last_seen_at`) stays current without a heartbeat
  mechanism. Each trace completion updates it.
- The Router has no state to serialize, migrate, or reason about on
  restart.

**What gets harder:**
- No agent row exists until the first trace completes. Any query
  against `agents` before that returns nothing. This is fine for
  v0.1.0 because nothing reads the agents table during active
  ingestion. It becomes relevant when the renderer shows a live
  agents view.
- `InFlightTrace` is larger. It carries the full `Agent` struct
  (name, framework, integration, timestamps) instead of a string ID.
  Not a real concern at current scale.

## Alternatives considered

**Register in the receive stage (rejected):** The receive stage
establishes the gRPC connection and sees the agent first. But receive
has no warm storage access, and adding it there would violate the
pipeline's layering. Receive validates and deduplicates OTLP data.
That's all it does.

**Register in the normalize stage (rejected):** Normalize is where
`Agent` is first constructed, so it's a natural-looking registration
point. Same problem: normalize has no storage access, and the stage
is designed to be a pure transformation with no side effects.

**Stateful router with an in-memory agent cache (rejected):** The
Router tracks which agents it has registered and skips the upsert for
known agents. Adds state to a struct that has no other state, complicates
testing, and the SQLite upsert costs less than the lookup to check
whether one is needed. There is no meaningful performance problem to
solve here.
