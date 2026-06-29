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

    fn evaluate(&self, ctx: &TraceContext<'_>) -> Option<f64> {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for span in ctx.spans {
            *counts.entry(span.operation.as_str()).or_insert(0) += 1;
        }
        let max = counts.values().copied().max().unwrap_or(0);
        if max < self.threshold {
            Some(1.0)
        } else {
            // Each repeat above the threshold drops the score by 0.15.
            let excess = (max - self.threshold + 1) as f64;
            Some((1.0 - excess * 0.15).max(0.0))
        }
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
