use super::TraceContext;
use std::collections::HashMap;

pub trait Evaluator: Send + Sync {
    fn name(&self) -> &str;
    fn evaluate(&self, ctx: &TraceContext<'_>) -> Option<f64>;
}

pub struct LoopDetector {
    threshold: usize,
}

impl LoopDetector {
    pub fn new(threshold: usize) -> Self {
        Self { threshold }
    }
}

impl Evaluator for LoopDetector {
    fn name(&self) -> &str {
        "loop_detection"
    }

    /// A loop is one action DOMINATING the trace, not the trace being
    /// long. The original absolute count scored a real 20-minute Claude
    /// Code turn (46 chat spans, a healthy mix of tools) as critical,
    /// because it assumed one trace holds a handful of spans; on the
    /// proxy path one trace is a whole turn. Carrier spans (the chat
    /// per round trip, the turn root) are structural and excluded;
    /// among the remaining actions, the threshold is the minimum
    /// evidence before dominance is judged, and the score falls as one
    /// action's share climbs past half of everything the agent did.
    fn evaluate(&self, ctx: &TraceContext<'_>) -> Option<f64> {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        let mut total = 0usize;
        for span in ctx.spans {
            let op = span.operation.as_str();
            if op == "gen_ai.chat" || op.starts_with("agent.turn") {
                continue;
            }
            *counts.entry(op).or_insert(0) += 1;
            total += 1;
        }
        let max = counts.values().copied().max().unwrap_or(0);
        if total == 0 || max < self.threshold {
            return Some(1.0);
        }
        let share = max as f64 / total as f64;
        if share < 0.5 {
            return Some(1.0);
        }
        // Share 0.5 scores 1.0, share 0.9+ scores 0.0.
        Some((1.0 - (share - 0.5) / 0.4).clamp(0.0, 1.0))
    }
}

pub struct CostEfficiencyEvaluator;

impl Evaluator for CostEfficiencyEvaluator {
    fn name(&self) -> &str {
        "cost_efficiency"
    }

    fn evaluate(&self, ctx: &TraceContext<'_>) -> Option<f64> {
        let fp = ctx.fingerprint?;
        if !fp.is_warmed() || fp.avg_cost_per_trace == 0.0 {
            return None;
        }
        let ratio = ctx.cost / fp.avg_cost_per_trace;
        // Score 1.0 at or below the average, 0.0 at double the average.
        Some((2.0 - ratio).clamp(0.0, 1.0))
    }
}

pub struct LatencyNormalityEvaluator;

impl Evaluator for LatencyNormalityEvaluator {
    fn name(&self) -> &str {
        "latency_normality"
    }

    fn evaluate(&self, ctx: &TraceContext<'_>) -> Option<f64> {
        let fp = ctx.fingerprint?;
        if !fp.is_warmed() || fp.avg_duration_secs == 0.0 {
            return None;
        }
        let min_start = ctx.spans.iter().map(|s| s.start_time).min()?;
        let max_end = ctx.spans.iter().filter_map(|s| s.end_time).max()?;
        // OTel timestamps are nanoseconds.
        let duration_secs = max_end.saturating_sub(min_start).max(0) as f64 / 1e9;
        let ratio = duration_secs / fp.avg_duration_secs;
        // Score 1.0 at or below average, 0.0 at 3× average.
        Some((1.0 - (ratio - 1.0).max(0.0) / 2.0).clamp(0.0, 1.0))
    }
}

pub struct IntentActionDivergenceEvaluator;

impl Evaluator for IntentActionDivergenceEvaluator {
    fn name(&self) -> &str {
        "intent_action_divergence"
    }

    fn evaluate(&self, _ctx: &TraceContext<'_>) -> Option<f64> {
        // Requires gen_ai.assistant.message span events with content.
        // Privacy tier 1 (the default) does not capture content, so this
        // returns None until content capture is enabled and that data is
        // surfaced through the evaluation context.
        None
    }
}

pub struct FingerprintDeviationEvaluator;

impl Evaluator for FingerprintDeviationEvaluator {
    fn name(&self) -> &str {
        "fingerprint_deviation"
    }

    fn evaluate(&self, ctx: &TraceContext<'_>) -> Option<f64> {
        let fp = ctx.fingerprint?;
        if !fp.is_warmed() {
            return None;
        }
        let z_cost = if fp.avg_cost_per_trace > 0.0 {
            (ctx.cost - fp.avg_cost_per_trace).abs() / fp.avg_cost_per_trace
        } else {
            0.0
        };
        let z_spans = if fp.avg_spans_per_trace > 0.0 {
            (ctx.span_count as f64 - fp.avg_spans_per_trace).abs() / fp.avg_spans_per_trace
        } else {
            0.0
        };
        let composite = ((z_cost * z_cost + z_spans * z_spans) / 2.0).sqrt();
        Some((1.0 - composite).clamp(0.0, 1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluation::fingerprint::AgentFingerprint;
    use reeve_model::entity::span::{InternalSpan, SpanStatus};
    use reeve_model::ids::{AgentId, TraceId};
    use std::collections::HashMap;

    fn make_span(op: &str, start: i64, end: i64) -> InternalSpan {
        InternalSpan {
            id: op.into(),
            trace_id: "t1".into(),
            parent_id: None,
            operation: op.to_string(),
            status: SpanStatus::Completed,
            start_time: start,
            end_time: Some(end),
            arrived_at: start,
            attributes: serde_json::Value::Null,
            raw_attributes: HashMap::new(),
        }
    }

    fn warmed_fp(avg_cost: f64, avg_spans: f64, avg_duration: f64) -> AgentFingerprint {
        let mut fp = AgentFingerprint::new();
        for _ in 0..10 {
            fp.update(avg_spans as usize, avg_cost, avg_duration);
        }
        fp
    }

    fn ctx<'a>(
        spans: &'a [InternalSpan],
        cost: f64,
        fp: Option<&'a AgentFingerprint>,
    ) -> TraceContext<'a> {
        TraceContext {
            trace_id: TraceId::from("t1"),
            agent_id: AgentId::from("a1"),
            span_count: spans.len(),
            cost,
            spans,
            fingerprint: fp,
        }
    }

    #[test]
    fn loop_detector_no_repeats_scores_one() {
        let spans = vec![make_span("llm.call", 0, 1), make_span("tool.search", 1, 2)];
        let score = LoopDetector::new(3).evaluate(&ctx(&spans, 0.01, None));
        assert_eq!(score, Some(1.0));
    }

    #[test]
    fn loop_detector_at_threshold_penalizes() {
        let spans = vec![
            make_span("tool.bash", 0, 1),
            make_span("tool.bash", 1, 2),
            make_span("tool.bash", 2, 3),
        ];
        let score = LoopDetector::new(3)
            .evaluate(&ctx(&spans, 0.01, None))
            .unwrap();
        assert!(score < 1.0);
    }

    #[test]
    fn loop_detector_scores_a_real_turn_shape_healthy() {
        // The #191 shape from a live 20-minute Claude Code build: many
        // chat carriers plus a healthy MIX of tools. Volume is work,
        // not a loop; the old absolute count scored this critical.
        let mut spans: Vec<InternalSpan> = Vec::new();
        for i in 0..46 {
            spans.push(make_span("gen_ai.chat", i, i + 1));
        }
        for (op, n) in [
            ("gen_ai.tool:Write", 16),
            ("gen_ai.tool:TaskUpdate", 16),
            ("gen_ai.tool:Bash", 14),
            ("gen_ai.tool:TaskCreate", 8),
            ("gen_ai.tool:Read", 4),
            ("gen_ai.tool:WebSearch", 3),
        ] {
            for i in 0..n {
                spans.push(make_span(op, 100 + i, 101 + i));
            }
        }
        spans.push(make_span("agent.turn.1", 0, 200));
        let score = LoopDetector::new(3)
            .evaluate(&ctx(&spans, 1.9, None))
            .unwrap();
        assert_eq!(score, 1.0, "a mixed 100-span turn is work, not a loop");
    }

    #[test]
    fn loop_detector_scores_a_dominated_trace_critical() {
        // An actual runaway: one tool hammered over and over with
        // almost nothing else. Dominance, not volume, is the signal.
        let mut spans: Vec<InternalSpan> = Vec::new();
        for i in 0..20 {
            spans.push(make_span("gen_ai.tool:Bash", i, i + 1));
        }
        spans.push(make_span("gen_ai.tool:Read", 50, 51));
        for i in 0..21 {
            spans.push(make_span("gen_ai.chat", 100 + i, 101 + i));
        }
        let score = LoopDetector::new(3)
            .evaluate(&ctx(&spans, 0.5, None))
            .unwrap();
        assert!(score < 0.2, "one tool at 95% share is a loop: {score}");
    }

    #[test]
    fn cost_efficiency_cold_start_returns_none() {
        let fp = AgentFingerprint::new();
        let score = CostEfficiencyEvaluator.evaluate(&ctx(&[], 0.05, Some(&fp)));
        assert_eq!(score, None);
    }

    #[test]
    fn cost_efficiency_at_baseline_scores_one() {
        let fp = warmed_fp(1.0, 10.0, 2.0);
        let score = CostEfficiencyEvaluator
            .evaluate(&ctx(&[], 1.0, Some(&fp)))
            .unwrap();
        assert!((score - 1.0).abs() < 0.01);
    }

    #[test]
    fn cost_efficiency_double_baseline_scores_zero() {
        let fp = warmed_fp(1.0, 10.0, 2.0);
        let score = CostEfficiencyEvaluator
            .evaluate(&ctx(&[], 2.0, Some(&fp)))
            .unwrap();
        assert!(score < 0.01);
    }

    #[test]
    fn latency_normality_within_baseline_scores_one() {
        // avg_duration_secs = 2.0; trace duration = 2s = 2_000_000_000 ns
        let fp = warmed_fp(1.0, 10.0, 2.0);
        let spans = vec![make_span("llm.call", 0, 2_000_000_000_i64)];
        let score = LatencyNormalityEvaluator
            .evaluate(&ctx(&spans, 0.01, Some(&fp)))
            .unwrap();
        assert!((score - 1.0).abs() < 0.01);
    }

    #[test]
    fn latency_normality_three_x_baseline_scores_zero() {
        // avg_duration_secs = 2.0; trace duration = 6s
        let fp = warmed_fp(1.0, 10.0, 2.0);
        let spans = vec![make_span("llm.call", 0, 6_000_000_000_i64)];
        let score = LatencyNormalityEvaluator
            .evaluate(&ctx(&spans, 0.01, Some(&fp)))
            .unwrap();
        assert!(score < 0.01);
    }

    #[test]
    fn fingerprint_deviation_cold_start_returns_none() {
        let fp = AgentFingerprint::new();
        let score = FingerprintDeviationEvaluator.evaluate(&ctx(&[], 0.01, Some(&fp)));
        assert_eq!(score, None);
    }

    #[test]
    fn fingerprint_deviation_on_baseline_scores_one() {
        let fp = warmed_fp(1.0, 10.0, 2.0);
        let spans: Vec<InternalSpan> = (0..10).map(|i| make_span("op", i, i + 1)).collect();
        let score = FingerprintDeviationEvaluator
            .evaluate(&ctx(&spans, 1.0, Some(&fp)))
            .unwrap();
        assert!((score - 1.0).abs() < 0.01);
    }

    #[test]
    fn intent_action_divergence_always_none() {
        let score = IntentActionDivergenceEvaluator.evaluate(&ctx(&[], 0.0, None));
        assert_eq!(score, None);
    }
}
