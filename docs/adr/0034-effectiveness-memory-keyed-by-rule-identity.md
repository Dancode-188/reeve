# 0034: Effectiveness Memory Keyed by Rule Identity, Not Metric

**Status:** Accepted
**Date:** 2026-07-08

## Context

Effectiveness memory answers "when this alert fires, which intervention
has historically helped?" by aggregating measured `InterventionOutcome`
deltas (ADR-0032) and attaching the best-performing command's track record
to future alerts as a hint.

The aggregation needs a scoping key: which past outcomes count as
evidence for this alert. The natural-seeming key is the evaluation
metric, joining outcomes to traces where the same metric was evaluated,
so that "loop detected" alerts learn only from traces that exhibited
loops. The problem is that Tier 1 metrics evaluate on every trace by
design: loop detection, cost efficiency, and latency normality all score
every completed trace. A metric join therefore selects nearly every
outcome for the agent, and a promise of per-failure-mode memory quietly
becomes per-agent memory while still looking scoped.

## Decision

Outcomes are scoped by the identity of the rule that issued the command:
`WHERE policy_id = <the alerting rule>`, aggregated per agent, falling
back to same-framework agents when the agent itself has fewer than three
measured outcomes. A minimum of three samples gates the hint in either
scope, because an average of one delta is an anecdote wearing statistics.

Rule identity is what the alert actually is. Two rules watching the same
metric with different thresholds are different situations, and a rule's
identity survives evaluator internals changing underneath it.

## Consequences

**What gets easier:**
- Hints are honest: the track record shown for a firing rule is built
  exclusively from that rule's own past commands.
- The aggregation is one indexed SQL query per alert, with no join
  against evaluation rows and no ambiguity about what qualified.
- A rule's memory survives evaluator changes, because the key is the
  rule's identity rather than anything about how its metric is computed.

**What gets harder:**
- Human-issued interventions do not feed effectiveness memory. Only
  policy-issued commands carry a `policy_id` (confirmed, auto-confirmed,
  or auto-dispatched). If effectiveness memory looks sparse, that is why:
  the redirect a developer types after seeing an alert is currently
  invisible to it.
- Growing the data means solving temporal association, attributing a
  human command to a rule when it was issued while that rule's alert was
  recent for the same agent. Deferred until real usage shows the
  policy-only data is too thin, because fuzzy attribution contaminates
  exactly the track record the feature exists to keep honest.

## Alternatives considered

- **Metric-keyed aggregation.** Selects nearly everything once
  always-on metrics are involved; the scoping is cosmetic.
- **Temporal association of human commands from day one.** More data,
  lower quality: a human command issued near an alert is not necessarily
  a response to it. Honest data first, cleverness later.
- **Global (cross-agent, cross-rule) aggregation.** Maximizes sample
  count and destroys the premise; what worked for a research agent's
  loop is weak evidence about a coding agent's cost overrun.
