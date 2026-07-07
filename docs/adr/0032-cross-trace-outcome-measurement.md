# 0032: Cross-Trace Window for Intervention Outcome Measurement

**Status:** Accepted
**Date:** 2026-07-07

## Context

Reeve records an `InterventionOutcome` for each applied command: the health
score before the intervention, the score after, and the delta. The value of
the whole feature rests on one question: what counts as "after"?

The intuitive answer is the rest of the command's own trace. It is also
wrong in practice. The live intervention runs during v0.3.0 showed that a
command usually reaches the agent moments before its trace completes: the
agent applies a redirect at its next checkpoint, finishes the current task,
and the trace closes. An in-trace measurement window would observe almost
nothing, and most outcomes would be empty.

There is also a wiring question. Outcome measurement needs to know when a
command was actually applied, but applied acks arrive at the dispatcher in
`reeve-intervention`, and the engine deliberately does not depend on that
crate (ADR-0029 keeps tonic and prost out of every engine build).

## Decision

**Measurement spans traces, per agent.** When the dispatcher processes an
applied ack, it records the command into a shared applied-commands feed.
The engine drains the feed on every completed trace. At pickup it captures
the agent's most recent health score as the before-picture; the scores of
that agent's next three completed traces become the after-picture, averaged.
The delta is after minus before, so positive means the intervention helped.

The command's own trace counts as the first post-intervention sample: its
later spans ran under the intervention, and by the time the engine picks
the command up, that trace's score is the next one to arrive.

**The feed reuses the established shared-state pattern.** An
`Arc<Mutex<Vec<AppliedCommand>>>` created in `main.rs`, written by the
dispatcher, drained by the engine: the third use of the pattern after the
NTP offset map and the paused-agents set. The `AppliedCommand` record lives
in `reeve-model` so both crates can name it without depending on each other.

**Honest edges.** Kill is not measured: a killed agent produces no
post-intervention behavior to score. A command applied before the agent has
any score history records a null before-picture and a null delta rather
than fabricating a baseline. Scores from other agents never count toward a
measurement, and overlapping measurements on one agent each keep their own
window.

Three post-intervention scores is the window. Health scores land once per
completed trace, so this is three traces of evidence: enough to smooth a
single lucky or unlucky trace, short enough that the outcome annotation
appears while the intervention is still fresh in the developer's mind.

## Consequences

**What gets easier:**
- Outcomes exist. Every applied Pause, Resume, Redirect, and InjectContext
  gets a measured delta, which is what the effectiveness memory aggregates
  and what the intervention impact view renders.
- The engine's evaluation loop needed no restructuring. Measurement hooks
  into the one place per-agent health scores already flow.
- The dispatcher stays ignorant of measurement policy. It records applied
  facts; the engine decides what to do with them.

**What gets harder:**
- The measurement is per agent, not per trace, so quality movement caused
  by something other than the intervention (a task mix change across those
  three traces) is attributed to the intervention. This is inherent to
  observational measurement without a control group; the projection in the
  intervention impact view exists to give the developer the trend context.
- An agent that stops producing traces after a command leaves the
  measurement pending forever. The state is a few entries per agent and
  clears when the agent resumes or the process restarts; no eviction is
  needed at this scale.
- The command's trace counting as post-intervention sample one slightly
  dilutes the after-picture when the intervention landed very late in that
  trace. Accepted: excluding it would routinely waste the trace where the
  redirect visibly changed behavior.

## Alternatives considered

**In-trace measurement window (rejected):** Measure only spans of the
command's own trace that completed after the apply. Matches the intuition
that an intervention targets a trace, but the live runs showed the window
would usually contain zero or near-zero spans. A measurement that is
almost always empty is worse than none.

**Engine subscribes to ack events over a channel (rejected):** A dedicated
mpsc from dispatcher to engine would deliver applied commands without
shared state. But the engine's loop wakes on ingestion events, and scores
only move when traces complete, so nothing is gained by waking it earlier;
the drain-on-trace-completion model has the same latency with less wiring.
The shared-state pattern is also already established twice.

**Wall-clock measurement window (rejected):** "Average all scores in the N
minutes after apply" decouples measurement from trace cadence, but agents
differ wildly in trace rate: a fast agent would cram twenty traces into the
window while a slow one contributes none. Counting traces normalizes the
evidence per agent.
