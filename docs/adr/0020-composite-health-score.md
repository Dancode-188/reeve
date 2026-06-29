# 0020: Composite Health Score Design

**Status:** Accepted
**Date:** 2026-06-30

## Context

The evaluation engine produces per-metric scores for each completed trace.
Those scores need to be aggregated into a single number the renderer can
display as a live quality gauge. The aggregation method determines how
interpretable the score is, how it behaves when metrics are missing, and
how easy it is for a developer to understand why a score is what it is.

The goal is a score that is fast, auditable, and live. Fast means the
first score update appears in under a millisecond after trace completion,
before any LLM evaluation completes. Auditable means a developer can look
at the weights and understand exactly why the number is what it is. Live
means the gauge updates twice: once with Tier 1 results immediately, and
again when Tier 2 results arrive.

## Decision

The health score is a weighted sum of five evaluation metrics multiplied
to a 0-100 scale:

```
health_score = (
    faithfulness      × 0.30 +
    tool_selection    × 0.25 +
    loop_detection    × 0.20 +
    cost_efficiency   × 0.15 +
    latency_normality × 0.10
) × 100
```

Weights reflect how much each metric predicts overall agent quality.
Faithfulness carries the most weight because a response that introduces
unsupported claims is a quality failure regardless of cost or latency.
Tool selection is the second strongest signal because choosing the wrong
tool wastes all downstream effort. The three Tier 1 metrics carry the
remaining weight in descending order of quality impact.

Two evaluators, `fingerprint_deviation` and `intent_action_divergence`,
produce `EvaluationComplete` events that the policy engine and renderer
can act on, but do not factor into the composite score. They signal
behavioral anomalies rather than quality degradation. The health score is
specifically a quality measure.

When Tier 2 metrics are absent, the weights are renormalized so the score
always uses the full 0-100 range. See ADR-0007 for the renormalization
formula and rationale.

The score updates twice per trace. The first update fires immediately
after Tier 1 evaluation completes (under 1ms after `TraceCompleted`).
`HealthScoreUpdated` carries `tier2_pending: true` and `weight_coverage`
reflecting only the Tier 1 contribution. The second update fires when
Tier 2 results arrive. `tier2_pending` becomes false and `weight_coverage`
reaches 1.0 when all five metrics are present.

The final health score is persisted to the `Trace` entity in the warm
store via `update_trace_health_score`. History queries and the replay view
use this persisted value.

Weights are the primary lever developers will want to tune. A future
configuration path (`~/.config/reeve/config.toml`) will support per-agent
weight overrides. V2 adds Bayesian weight adaptation from developer
feedback. The arithmetic is intentionally simple so that the adaptation
layer, when it arrives, modifies weights without changing the formula.

## Consequences

**What gets easier:**
- The health score is immediately interpretable. A developer can read the
  weight table and understand exactly why the gauge shows 73 instead of 85.
- The two-tier update gives fast feedback (Tier 1 in <1ms) without hiding
  that a more complete evaluation is pending.
- Adding a new metric in a future version requires only adding a weight
  table entry. The renormalization in ADR-0007 handles the rest.
- Persisting the final score to the warm store enables history queries
  and replay without recomputing.

**What gets harder:**
- The score is not stable between the Tier 1 and Tier 2 updates. A score
  that reads 82 may shift to 71 once faithfulness and tool selection are
  included. The renderer must make the pending state visible so the
  developer does not interpret the Tier 1 score as final.
- Weight configuration adds surface area for misconfiguration. Weights
  that do not sum to 1.0 need to be rejected or normalized at load time.

## Alternatives considered

**Equal weights across all metrics (rejected):** Equal weights imply that
latency normality matters as much as faithfulness, which does not reflect
how agent quality degrades in practice. The weight table encodes a
deliberate opinion about what matters most. Hiding that opinion behind
equal weights makes the score harder to reason about, not easier.

**Single-pass scoring after all metrics arrive (rejected):** Waiting for
Tier 2 (1-10 seconds) before showing any score means no feedback until
the evaluation chain completes. Tier 1 results are available in under a
millisecond. The two-tier update is a deliberate tradeoff: accept a
renormalized preliminary score in exchange for immediate live feedback.

**ML-based score aggregation (rejected for v0.2.0):** A model that learns
to weight metrics from historical outcomes is more accurate in theory, but
requires training data that does not exist yet, is harder to audit, and
adds a dependency on a training pipeline. The arithmetic baseline must work
first. Bayesian weight adaptation is the approved V2 addition once enough
intervention outcome data exists to train from.
