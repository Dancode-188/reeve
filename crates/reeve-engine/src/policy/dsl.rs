use evalexpr::{ContextWithMutableVariables, HashMapContext, Value, eval_boolean_with_context};
use std::collections::HashMap;

/// evalexpr context built from Tier 1 evaluation results.
///
/// Variables available to policy conditions:
///   health_score                 f64   composite score on [0, 100]
///   cost_usd                     f64   trace cost in US dollars
///   span_count                   f64   number of spans in the trace
///   tier2_pending                bool  true when Tier 2 results have not arrived
///   weight_coverage              f64   sum of active metric weights in [0.0, 1.0]
///   predicted_cost_at_completion f64   extrapolated final cost (mid-trace only)
///   <metric_name>                f64   individual metric score in [0.0, 1.0]
pub struct PolicyContext {
    inner: HashMapContext,
}

impl PolicyContext {
    pub fn build(
        health_score: f64,
        cost_usd: f64,
        span_count: usize,
        tier2_pending: bool,
        weight_coverage: f64,
        predicted_cost_at_completion: f64,
        metric_scores: &HashMap<&str, f64>,
    ) -> Self {
        let mut ctx = HashMapContext::new();
        ctx.set_value("health_score".into(), Value::Float(health_score))
            .ok();
        ctx.set_value("cost_usd".into(), Value::Float(cost_usd))
            .ok();
        ctx.set_value("span_count".into(), Value::Float(span_count as f64))
            .ok();
        ctx.set_value("tier2_pending".into(), Value::Boolean(tier2_pending))
            .ok();
        ctx.set_value("weight_coverage".into(), Value::Float(weight_coverage))
            .ok();
        ctx.set_value(
            "predicted_cost_at_completion".into(),
            Value::Float(predicted_cost_at_completion),
        )
        .ok();
        for (name, &score) in metric_scores {
            ctx.set_value(name.to_string(), Value::Float(score)).ok();
        }
        Self { inner: ctx }
    }

    /// Minimal context for mid-trace span evaluation. Only
    /// `predicted_cost_at_completion` is set; other variables are absent so
    /// rules that reference health_score or cost_usd do not fire.
    pub fn build_mid_trace(predicted_cost_at_completion: f64) -> Self {
        let mut ctx = HashMapContext::new();
        ctx.set_value(
            "predicted_cost_at_completion".into(),
            Value::Float(predicted_cost_at_completion),
        )
        .ok();
        Self { inner: ctx }
    }

    pub fn evaluate(&self, condition: &str) -> bool {
        match eval_boolean_with_context(condition, &self.inner) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(condition, error = %e, "policy condition evaluation failed");
                false
            }
        }
    }
}

/// Validates that `condition` is syntactically parseable by evalexpr.
/// `VariableIdentifierNotFound` is accepted because user-defined rules may
/// reference custom metric names not present in every evaluation context.
pub fn validate_condition(condition: &str) -> Result<(), String> {
    let ctx = PolicyContext::build(0.0, 0.0, 0, false, 0.0, 0.0, &HashMap::new());
    match eval_boolean_with_context(condition, &ctx.inner) {
        Ok(_) => Ok(()),
        Err(evalexpr::EvalexprError::VariableIdentifierNotFound(_)) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(health_score: f64, cost_usd: f64) -> PolicyContext {
        PolicyContext::build(health_score, cost_usd, 5, false, 0.45, 0.0, &HashMap::new())
    }

    #[test]
    fn health_score_below_threshold_matches() {
        assert!(ctx(25.0, 1.0).evaluate("health_score < 30"));
    }

    #[test]
    fn health_score_at_threshold_does_not_match() {
        assert!(!ctx(30.0, 1.0).evaluate("health_score < 30"));
    }

    #[test]
    fn health_score_above_threshold_does_not_match() {
        assert!(!ctx(75.0, 1.0).evaluate("health_score < 30"));
    }

    #[test]
    fn cost_usd_above_threshold_matches() {
        assert!(ctx(80.0, 6.0).evaluate("cost_usd > 5.0"));
    }

    #[test]
    fn cost_usd_below_threshold_does_not_match() {
        assert!(!ctx(80.0, 2.0).evaluate("cost_usd > 5.0"));
    }

    #[test]
    fn metric_variable_is_accessible() {
        let mut metrics = HashMap::new();
        metrics.insert("loop_detection", 0.3_f64);
        let c = PolicyContext::build(80.0, 1.0, 5, false, 0.45, 0.0, &metrics);
        assert!(c.evaluate("loop_detection < 0.5"));
    }

    #[test]
    fn unknown_variable_returns_false_without_panic() {
        assert!(!ctx(25.0, 1.0).evaluate("nonexistent_variable < 30"));
    }

    #[test]
    fn compound_and_condition_evaluated() {
        assert!(ctx(25.0, 6.0).evaluate("health_score < 30 && cost_usd > 5.0"));
        assert!(!ctx(75.0, 6.0).evaluate("health_score < 30 && cost_usd > 5.0"));
    }

    #[test]
    fn boolean_variable_is_accessible() {
        let c = PolicyContext::build(80.0, 1.0, 5, true, 0.45, 0.0, &HashMap::new());
        assert!(c.evaluate("tier2_pending == true"));
    }

    #[test]
    fn span_count_is_accessible() {
        let c = PolicyContext::build(80.0, 1.0, 12, false, 0.45, 0.0, &HashMap::new());
        assert!(c.evaluate("span_count > 10"));
    }

    #[test]
    fn predicted_cost_at_completion_is_accessible() {
        let c = PolicyContext::build(80.0, 1.0, 5, false, 0.45, 9.5, &HashMap::new());
        assert!(c.evaluate("predicted_cost_at_completion > 8.0"));
    }

    #[test]
    fn build_mid_trace_only_sets_predicted_cost() {
        let c = PolicyContext::build_mid_trace(10.0);
        assert!(c.evaluate("predicted_cost_at_completion > 8.0"));
        // health_score is absent — condition fails gracefully without panic
        assert!(!c.evaluate("health_score < 30"));
    }
}
