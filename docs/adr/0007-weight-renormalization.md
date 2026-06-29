# 0007: Weight Renormalization for Missing Evaluation Metrics

**Status:** Accepted
**Date:** 2026-06-30

## Context

The composite health score is a weighted sum of five evaluation metrics.
Tier 1 heuristic evaluators produce three of those metrics synchronously
on every completed trace. The two Tier 2 metrics come from a local LLM
judge running asynchronously and are unavailable during cold start, when
Ollama is not running, or while the evaluation is in flight.

A score computed with only three of five metrics has a gap. There are two
honest ways to fill it: deflate the score by treating missing metrics as
zero, or redistribute the missing weight proportionally among the metrics
that did produce a result.

Deflation is the naive default and it is wrong for the use case. A healthy
agent running normally with no Tier 2 available would score 45 out of 100
(the sum of the three Tier 1 weights). That reads as a failing agent to
any developer watching the gauge. The score loses interpretive value the
moment it drops below the theoretical maximum for the available tier.

ADR-0002 cross-references this decision by number because weight
renormalization is the specific mechanism that keeps the health score
useful even when the local LLM judge is disabled or unavailable.

## Decision

When one or more metrics are absent, redistribute the missing weight
proportionally among the metrics that did produce a result. The formula
divides each available metric's raw weight by the sum of all available
weights. The renormalized weights sum to 1.0 and the score uses the full
0-100 range regardless of which metrics are present.

Example with only Tier 1 available (raw weights sum to 0.45):

```
loop_detection:    0.20 / 0.45 = 0.444
cost_efficiency:   0.15 / 0.45 = 0.333
latency_normality: 0.10 / 0.45 = 0.222
```

A perfect Tier 1 score still reads as 100. A degraded agent still reads
as degraded. The score is always interpretable.

`EngineEvent::HealthScoreUpdated` carries two fields that let the renderer
communicate the renormalization state to the developer: `tier2_pending: bool`
(true when the score is renormalized because Tier 2 has not contributed yet)
and `weight_coverage: f64` (the sum of active metric weights before
renormalization, so 0.45 when Tier 1 only and 1.0 when all five metrics
are present). The renderer uses these to show a pulsing animation and a
coverage indicator while the score is renormalized.

## Consequences

**What gets easier:**
- The health score is interpretable at all times: during cold start, when
  Ollama is not installed, and while Tier 2 evaluation is in flight.
- Developers see a real signal from the first completed trace, not a
  degraded placeholder.
- Adding a new metric in a future version does not break existing scores.
  The renormalization handles any subset of the weight table automatically.

**What gets harder:**
- A score of 87 with `weight_coverage = 0.45` is not directly comparable
  to a score of 87 with `weight_coverage = 1.0`. The renderer must surface
  coverage to avoid misleading the developer. `tier2_pending` and
  `weight_coverage` on the event exist specifically for this.
- Two traces from the same agent can have different effective weight tables
  if Ollama availability changes between them. Historical score comparison
  requires checking whether Tier 2 was contributing at the time.

## Alternatives considered

**Deflation: treat missing weights as zero (rejected):** A three-metric
score caps at 45 out of 100. A healthy agent with no Tier 2 available
looks like a failing one. The gauge loses meaning as a live quality signal
and developers learn to ignore it. This is the wrong outcome for the
primary UI element of the cockpit.

**Suppress the score until all metrics are present (rejected):** This
makes the gauge useless during cold start and whenever Ollama is not
installed. Tier 1 completes in under a millisecond; waiting for Tier 2
(1-10 seconds) to show any score is a poor trade. The gauge should always
show something real.

**Show "N/A" instead of a score when metrics are missing (rejected):**
Same problem as suppression. The developer sees no live feedback until the
full evaluation chain is complete. The point of Tier 1 is fast feedback.
