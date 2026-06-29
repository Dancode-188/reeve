use std::collections::HashMap;

// The composite health score uses five metrics. Two other evaluators
// (fingerprint_deviation, intent_action_divergence) produce
// EvaluationComplete events used by the policy engine and renderer
// but do not factor into this score. They signal anomalies rather
// than quality degradation, which the health score is designed to
// measure. See ADR-0007 and ADR-0020.
const WEIGHTS: &[(&str, f64)] = &[
    ("faithfulness", 0.30),
    ("tool_selection", 0.25),
    ("loop_detection", 0.20),
    ("cost_efficiency", 0.15),
    ("latency_normality", 0.10),
];

pub struct HealthScore {
    /// Composite score on a 0.0 to 100.0 scale.
    pub value: f64,
    /// True when Tier 2 metrics are absent and the score is renormalized.
    pub tier2_pending: bool,
    /// Sum of active metric weights before renormalization. 1.0 means all
    /// five metrics contributed; 0.45 means only Tier 1 contributed.
    pub weight_coverage: f64,
}

/// Compute the composite health score from whatever metrics are available.
///
/// `scores` maps metric name to a score in [0.0, 1.0]. Missing metrics are
/// skipped and their weight is redistributed proportionally among the rest
/// so the result always uses the full 0-100 range. Returns `None` if no
/// scored metrics are present at all.
pub fn compute(scores: &HashMap<&str, f64>) -> Option<HealthScore> {
    let mut weighted_sum = 0.0_f64;
    let mut weight_coverage = 0.0_f64;

    for (metric, weight) in WEIGHTS {
        if let Some(&score) = scores.get(metric) {
            weighted_sum += score * weight;
            weight_coverage += weight;
        }
    }

    if weight_coverage == 0.0 {
        return None;
    }

    // Renormalize: divide by active weight sum so the score uses full range.
    let value = (weighted_sum / weight_coverage) * 100.0;

    // All five Tier 2 metrics ("faithfulness", "tool_selection") being present
    // would push weight_coverage to 1.0. Anything below that means Tier 2 is
    // pending.
    let tier2_pending = weight_coverage < 1.0 - f64::EPSILON;

    Some(HealthScore {
        value,
        tier2_pending,
        weight_coverage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scores(pairs: &[(&'static str, f64)]) -> HashMap<&'static str, f64> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn no_scores_returns_none() {
        assert!(compute(&scores(&[])).is_none());
    }

    #[test]
    fn all_perfect_scores_give_100() {
        let s = scores(&[
            ("faithfulness", 1.0),
            ("tool_selection", 1.0),
            ("loop_detection", 1.0),
            ("cost_efficiency", 1.0),
            ("latency_normality", 1.0),
        ]);
        let hs = compute(&s).unwrap();
        assert!((hs.value - 100.0).abs() < 0.001);
        assert!(!hs.tier2_pending);
        assert!((hs.weight_coverage - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn all_zero_scores_give_zero() {
        let s = scores(&[
            ("faithfulness", 0.0),
            ("tool_selection", 0.0),
            ("loop_detection", 0.0),
            ("cost_efficiency", 0.0),
            ("latency_normality", 0.0),
        ]);
        let hs = compute(&s).unwrap();
        assert!(hs.value.abs() < 0.001);
    }

    #[test]
    fn tier1_only_renormalizes_to_full_range() {
        // Only Tier 1 metrics available (sum = 0.45).
        // All perfect scores should still give 100.
        let s = scores(&[
            ("loop_detection", 1.0),
            ("cost_efficiency", 1.0),
            ("latency_normality", 1.0),
        ]);
        let hs = compute(&s).unwrap();
        assert!((hs.value - 100.0).abs() < 0.001);
        assert!(hs.tier2_pending);
        assert!((hs.weight_coverage - 0.45).abs() < 0.001);
    }

    #[test]
    fn tier1_only_zero_scores_give_zero() {
        let s = scores(&[
            ("loop_detection", 0.0),
            ("cost_efficiency", 0.0),
            ("latency_normality", 0.0),
        ]);
        let hs = compute(&s).unwrap();
        assert!(hs.value.abs() < 0.001);
        assert!(hs.tier2_pending);
    }

    #[test]
    fn single_metric_carries_full_weight() {
        // Only loop_detection at 0.5 — should give score of 50.
        let s = scores(&[("loop_detection", 0.5)]);
        let hs = compute(&s).unwrap();
        assert!((hs.value - 50.0).abs() < 0.001);
        assert!(hs.tier2_pending);
        assert!((hs.weight_coverage - 0.20).abs() < 0.001);
    }

    #[test]
    fn unknown_metrics_are_ignored() {
        // fingerprint_deviation and intent_action_divergence are not in the
        // weight table and must not affect the score.
        let s = scores(&[
            ("loop_detection", 1.0),
            ("fingerprint_deviation", 0.0),
            ("intent_action_divergence", 0.0),
        ]);
        let hs = compute(&s).unwrap();
        assert!((hs.value - 100.0).abs() < 0.001);
        assert!((hs.weight_coverage - 0.20).abs() < 0.001);
    }
}
