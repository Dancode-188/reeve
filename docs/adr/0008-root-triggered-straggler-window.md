# 0008: Root-Triggered Straggler Window for Trace Completion

**Status:** Accepted
**Date:** 2026-06-26

## Context

The assemble stage must decide when a trace is complete. A trace is
complete when all of its spans have arrived, but the assemble stage
has no way to know in advance how many spans to expect. It needs a
heuristic.

The naive approach is an idle timer: if no new spans have arrived for
N seconds, declare the trace complete. This works but forces every
trace completion event to wait N seconds after the last span arrives.
For a tool displaying a live trace tree in a terminal, that latency
makes the completion signal feel broken. A trace that finished in 200
milliseconds of real execution time should not take 30 seconds to
finalize in the cockpit.

The question is: can the assemble stage detect completion faster than
the idle timer, without knowing the total span count?

## Decision

The root span is the completion signal. A span with no parent_span_id
is the root of its trace. In OTel, the root span is the outermost
span: the agent framework span that contains all LLM calls and tool
calls as descendants. Its end_time bounds the latest wall-clock
moment at which any descendant span could legitimately have ended.

When the root span arrives, the assemble stage starts a 2-second
straggler window. The window exists to absorb OTel batch export
delays: a span may be buffered at the exporter for up to one batch
interval before being sent, and Reeve may receive it slightly after
the root. Two seconds covers a full batch interval with margin. After
the window closes, the next tick finalizes the trace.

The 30-second idle timer is retained as a fallback for one specific
case: a trace whose root span never arrives. This happens when an
agent crashes mid-execution, when the root span is dropped by a
misconfigured exporter, or when instrumentation is incomplete. The
30-second fallback prevents those orphaned partial traces from
accumulating in memory indefinitely.

## Consequences

**What gets easier:**
- Traces complete in roughly 2 seconds after the root span arrives,
  regardless of how long the trace took to execute. The cockpit
  shows a finalized trace tree while the result of the agent run
  is still fresh.
- The 30-second fallback only fires when something is genuinely
  wrong with the agent or its instrumentation. Normal traces never
  wait 30 seconds.

**What gets harder:**
- Completion detection now depends on the root span arriving. If a
  well-formed agent consistently emits the root span early (before
  all descendants), the 2-second window may be too short. This has
  not been observed in practice with standard OTel SDK exporters,
  which batch spans and send them in arrival order.
- The straggler window duration (2 seconds) is a constant, not
  configurable. If it needs tuning for high-latency exporters, a
  config field will need to be added.

## Alternatives considered

**Idle timer for everything (rejected):** Simple and requires no root
span detection. Rejected because it adds 30 seconds of latency to
every normal trace finalization. An agent that completes in 500
milliseconds produces a trace that stays "in flight" for 30 seconds
in the cockpit. That is not an acceptable user experience.

**Quiescence timer starting from last span arrival (rejected):**
Start the timer on the last span received, not on the root. A shorter
quiescence interval (2-3 seconds) would achieve similar latency.
Rejected because "last span received" is not semantically meaningful.
A slow exporter on a second agent could delay the quiescence timer
for a trace on the first agent. Root arrival is a per-trace event
that is independent of exporter timing on other traces.

**Span count heuristic (rejected):** Some OTel conventions include a
span count in resource attributes. If Reeve could know the expected
span count, it could declare completion exactly when that count was
reached. Rejected because the count is not reliably present, and any
mismatch between the declared count and the actual span count would
either cause premature finalization or cause a trace to never
finalize. The straggler window tolerates the imprecision that a
heuristic approach requires.
