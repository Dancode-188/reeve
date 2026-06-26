# 0011: Root Span Status Determines Trace-Level Failure

**Status:** Accepted
**Date:** 2026-06-26

## Context

When the route stage finalizes a trace it must assign a `TraceStatus`.
For traces that complete normally there are two options: `Completed` or
`Failed`. Something has to decide which one.

An agent trace is a tree. The root span is the top-level agent
execution. Children are tool calls, LLM completions, retrieval steps.
Child spans fail for all kinds of reasons that the agent handles
internally: a tool times out and gets retried, an LLM returns an error
and the agent falls back to a different model. Those are recovered
failures. They don't mean the agent run itself failed.

The root span's status is set by the SDK when the outermost call
returns. If the agent crashes, the SDK marks the root span failed. If
the agent completes normally, the root span is completed. That's the
authoritative signal.

## Decision

A trace is `Failed` if and only if its root span has
`SpanStatus::Failed` at finalization. All other `Completed` traces are
`TraceStatus::Completed`. Traces that finalize via idle timeout (no
root ever arrived) are `TraceStatus::Interrupted` unconditionally.

## Consequences

**What gets easier:**
- Trace status is a reliable coarse signal. A failed tool call that
  the agent handled does not inflate the failure rate.
- Dashboards and alerting can filter on `TraceStatus::Failed` without
  worrying about transient child failures.

**What gets harder:**
- An agent that swallows a child failure and returns a successful
  result anyway gets a `Completed` trace. The child failure is still
  in the spans, but not surfaced at trace level. Finding it requires
  looking at individual spans or evaluation results. That's the right
  place for it.

## Alternatives considered

**Any-span failure (rejected):** Mark a trace `Failed` if any span
in the tree failed. A recovered tool error would count. That turns
every agent that handles exceptions into a "failing" agent, which is
not a useful definition of failure.

**All-span failure (rejected):** Mark a trace `Failed` only if every
leaf span failed. Too conservative. An agent where one critical call
fails and the rest succeed should be `Failed`, not `Completed`.

**Weighted heuristic (rejected):** Compute a failure score from
depth, cost weight, or span count. Rejected as the wrong layer. The
evaluator stage (reeve-engine, post-v1) is where nuanced health
scoring belongs. The route stage sets a simple, stable status that
downstream systems can depend on. One thing at a time.
