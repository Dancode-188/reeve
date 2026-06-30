# 0021: Tier 2 LLM Judge via Ollama with Self-Consistency Scoring

**Status:** Accepted
**Date:** 2026-06-30

## Context

Tier 1 heuristic evaluators cover structure: loops, cost relative to baseline,
latency relative to baseline, behavioral fingerprint deviation. They are
deterministic, sub-millisecond, and require no model. What they cannot assess
is whether the agent's output was semantically correct. A trace can have
normal cost, normal latency, and no loops, while still producing a response
that contradicts the retrieved context, selects the wrong tool, or fabricates
claims the context does not support.

The two highest-weight metrics in the health score, `faithfulness` (0.30) and
`tool_selection` (0.25), require semantic judgment. An LLM judge is the standard
mechanism for evaluating these dimensions at development time.

The problem with LLM judges is that they are themselves unreliable. A single
call to a small local model can produce an inconsistent score depending on
prompt phrasing, token sampling, and model temperature. Reporting an
inconsistent score as fact would mislead developers rather than inform them.

## Decision

Use a local Ollama phi4-mini model as the Tier 2 judge. The local-first
rationale is covered in ADR-0002. This ADR covers the evaluation protocol
and self-consistency mechanism.

**Self-consistency scoring:** Each metric is evaluated twice using differently
phrased rubric prompts. The two scores are compared before any result is
reported. Divergence below 0.10 is High confidence. Divergence between 0.10
and 0.30 is Medium confidence. Divergence above 0.30 means the evaluation
is genuinely unreliable and the metric is excluded from the health score.
`EvaluationComplete` carries a `confidence: Option<EvaluationConfidence>`
field so the renderer can surface the confidence state without computing it
from raw scores. `None` means a Tier 1 evaluator, where determinism makes
a second pass unnecessary.

Low-confidence results are still emitted as `EvaluationComplete` events so
the policy engine can act on them. They are not included in the
`HealthScoreUpdated` recomputation. A Low confidence result that contributes
to the health score would shift the number in ways the developer cannot
trust or reproduce.

**Three Tier 2 evaluators:**

`faithfulness` (weight 0.30) assesses whether the agent's response uses
only information from the retrieved context. `tool_selection` (weight 0.25)
assesses whether the right tools were called in the right order, derived
from span operation names and the `gen_ai.tool.name` attribute. These two
feed into the health score.

`hallucination_detection` follows the same pattern as `fingerprint_deviation`
in Tier 1: it emits `EvaluationComplete` for the policy engine to act on
but is absent from the weight table. It signals an anomaly rather than a
quality dimension with an established weight.

**Privacy tier 1 behavior:** Under the default privacy tier, span event
content is null. `faithfulness` and `hallucination_detection` require LLM
response text to evaluate and return `None` when content is absent.
`tool_selection` operates on span operation names and metadata which are
always available. A default installation therefore contributes one Tier 2
metric to the health score, not three. This is correct behavior.

**Backend discovery:** The engine probes Ollama at startup and emits
`EngineEvent::EvaluationBackendReady` once with a human-readable backend
description and, when disabled, a `reason` field that distinguishes between
Ollama not found and phi4-mini not pulled. The two cases require different
recovery actions and the renderer shows them separately.

**Retry behavior:** Each Ollama call retries up to three times with
exponential backoff. On exhaustion the metric is skipped and the health
score renormalizes per ADR-0007.

## Consequences

**What gets easier:**
- The health score reflects semantic quality when Ollama is available, not
  just structural properties.
- Self-consistency scoring surfaces evaluation uncertainty directly in the
  event stream. The renderer can show a distinct indicator for Low confidence
  results rather than presenting an unreliable score as fact.
- Graceful degradation means the engine works without Ollama and without
  configuration changes. The gap in weight coverage is honest and visible.

**What gets harder:**
- Two inference calls per metric means Tier 2 evaluation takes roughly twice
  as long as a single-pass approach. With three metrics and two calls each,
  a trace may take several seconds to fully evaluate under Tier 2. The
  two-tier update in ADR-0020 exists specifically to absorb this: Tier 1
  results appear in under a millisecond and Tier 2 arrives when it finishes.
- The health score can shift between the Tier 1 and Tier 2 updates. The
  renderer must make `tier2_pending` visible.
- Low confidence exclusion means a degraded agent whose scores are wildly
  inconsistent between rubric phrasings will show a Tier 1-only health score
  even when Ollama is running. This is the correct outcome.

## Alternatives considered

**Single-pass evaluation (rejected):** A single rubric call per metric
is faster and simpler but does not address the unreliability problem.
An inconsistent model will produce inconsistent scores across traces for
the same agent behavior. Developers have no signal that a score is
questionable. Self-consistency scoring doubles the inference cost but pays
for it with honest uncertainty quantification.

**Cloud API judge (rejected):** See ADR-0002. Developer traces contain
internal reasoning, tool call patterns, and potentially sensitive retrieved
context. Sending them to a cloud API for evaluation violates the local-first
principle that Reeve is built on.

**Ensemble of multiple local models (rejected for v0.2.0):** Averaging
across multiple models would give more reliable confidence estimates but
requires users to have multiple models pulled locally and adds dependency
on model availability. phi4-mini is a single well-scoped dependency. Ensemble
evaluation is a plausible v0.3.0 addition once the single-model path is
established and instrumented.
