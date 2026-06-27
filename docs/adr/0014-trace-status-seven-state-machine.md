# 0014: Seven-State Trace Status Machine

**Status:** Accepted
**Date:** 2026-06-27

## Context

A trace needs a status. The naive options are running and done. That
covers the happy path: spans arrive, root span arrives, trace closes.

But agents run in the real world. The gRPC connection drops mid-run.
Reeve restarts while a trace is active. An operator pauses the agent to
review what it is doing. A run times out because the root span never
arrives. These are not edge cases; they are the situations where
observability actually matters.

A two-state machine cannot distinguish between a trace that completed
normally and one that was cut short by a network drop. A three-state
machine adds an error bucket but still cannot represent the intermediate
states needed to resume a trace after a Reeve restart or return a
paused agent to running.

## Decision

`TraceStatus` has seven states:

```
Running      Spans arriving normally.
Disconnected gRPC connection dropped; within the 60-second grace period.
Paused       Intervention-driven; awaiting Resume command.
Interrupted  Grace period expired or idle timeout; flushed to warm tier.
Resuming     Reeve restarted; trace being reloaded from warm tier.
Completed    Root span arrived cleanly; flushed to warm tier.
Failed       Root span arrived with error status.
```

Valid transitions:

```
Running      -> Disconnected
Disconnected -> Running      (reconnected within grace period)
Disconnected -> Interrupted  (grace period expired)
Running      -> Paused
Paused       -> Running      (Resume command received)
Interrupted  -> Resuming     (Reeve restarted, trace found in warm tier)
Resuming     -> Running
Running      -> Completed
Running      -> Failed
```

`TraceStatus::transition_to` validates every transition and returns
`Err(InvalidTraceTransition)` for anything not in that list. The
terminal states (`Completed`, `Failed`, `Interrupted`) have no outgoing
transitions. A completed trace stays completed.

## Consequences

**What gets easier:**
- The status column in the warm tier is meaningful. Querying for
  `Interrupted` traces surfaces agent runs that died without a clean
  exit. Querying for `Disconnected` surfaces runs currently in the
  grace period.
- Reeve can restart without losing in-progress traces. The
  `Interrupted -> Resuming -> Running` path is what makes recovery
  possible without treating every restart as data loss.
- The `transition_to` method makes invalid state transitions a compile-
  checked runtime error, not a silent data corruption.

**What gets harder:**
- Seven states means seven variants to handle wherever `TraceStatus`
  is matched. The renderer, the route stage, and any future query
  layer all need to account for states they may not care about.
- The 60-second `Disconnected` grace period is a constant, not
  configurable. If it needs tuning it becomes a config field.

## Alternatives considered

**Two states: Running and Completed (rejected):** Simple, but cannot
represent a dropped connection, a paused run, or a recovery. Any
unexpected end to a trace gets silently bucketed as completed, which
is the wrong data for a tool whose job is to surface what went wrong.

**Three states: Running, Completed, Failed (rejected):** Adds the
error case but still cannot represent the intermediate states. A
network drop mid-run would be indistinguishable from a completed run
until the grace period expired and it became a failed run. That
delay is confusing in a live cockpit.

**Treat Disconnected as Interrupted immediately (rejected):** Skip
the grace period entirely. OTel SDK exporters buffer spans in memory
and retry on reconnect. A dropped connection that recovers within
seconds would lose all buffered spans if the trace was immediately
flushed to warm tier and closed. The 60-second window is sized to
cover typical OTel retry backoff schedules.
