use reeve_model::entity::trace::Trace;

/// Data behind the intervention impact view: the agent's actual health and
/// cost around one intervention, plus the trajectory the pre-intervention
/// trend projected. Points are (trace index, value): the x axis counts the
/// agent's traces in chronological order, because that is the granularity
/// outcome measurement works at.
pub struct ImpactState {
    pub command_tag: String,
    /// Actual health per trace before the intervention (inclusive of the
    /// trace the command was issued against).
    pub pre_health: Vec<(f64, f64)>,
    /// Actual health per trace after.
    pub post_health: Vec<(f64, f64)>,
    /// Least-squares extension of the pre trend across the post range.
    pub projected_health: Vec<(f64, f64)>,
    pub pre_cost: Vec<(f64, f64)>,
    pub post_cost: Vec<(f64, f64)>,
    pub projected_cost: Vec<(f64, f64)>,
    /// X position of the intervention, where the divergence marker draws.
    pub intervention_x: f64,
}

/// How many pre-intervention traces feed the trend line. Enough to smooth
/// noise, few enough that an old regime does not drown the recent trend.
const TREND_WINDOW: usize = 5;

impl ImpactState {
    /// Builds the view from the agent's chronological trace history, the
    /// index of the trace the command was issued against, and each trace's
    /// aggregated cost. Returns None when there is nothing on either side
    /// to compare: an impact view needs a before and an after.
    pub fn build(
        history: &[(Trace, f64)],
        intervention_idx: usize,
        command_tag: String,
    ) -> Option<Self> {
        if intervention_idx + 1 >= history.len() || intervention_idx == 0 {
            return None;
        }

        let health_points = |range: std::ops::Range<usize>| -> Vec<(f64, f64)> {
            history[range.clone()]
                .iter()
                .enumerate()
                .filter_map(|(i, (t, _))| {
                    t.final_health_score.map(|s| ((range.start + i) as f64, s))
                })
                .collect()
        };
        let cost_points = |range: std::ops::Range<usize>| -> Vec<(f64, f64)> {
            history[range.clone()]
                .iter()
                .enumerate()
                .map(|(i, (_, c))| ((range.start + i) as f64, *c))
                .collect()
        };

        let pre_start = intervention_idx.saturating_sub(TREND_WINDOW - 1);
        let pre_health = health_points(pre_start..intervention_idx + 1);
        let post_health = health_points(intervention_idx + 1..history.len());
        let pre_cost = cost_points(pre_start..intervention_idx + 1);
        let post_cost = cost_points(intervention_idx + 1..history.len());

        if pre_health.is_empty() || post_health.is_empty() {
            return None;
        }

        let post_xs: Vec<f64> = (intervention_idx..history.len())
            .map(|i| i as f64)
            .collect();
        let projected_health = project(&pre_health, &post_xs, 0.0, 100.0);
        let projected_cost = project(&pre_cost, &post_xs, 0.0, f64::MAX);

        Some(Self {
            command_tag,
            pre_health,
            post_health,
            projected_health,
            pre_cost,
            post_cost,
            projected_cost,
            intervention_x: intervention_idx as f64,
        })
    }
}

/// Least-squares line through `points`, evaluated at `xs`, clamped to
/// [min, max]. A single point projects flat: with no slope information,
/// continuing the level is the only defensible guess.
fn project(points: &[(f64, f64)], xs: &[f64], min: f64, max: f64) -> Vec<(f64, f64)> {
    let n = points.len() as f64;
    let (sum_x, sum_y): (f64, f64) = points
        .iter()
        .fold((0.0, 0.0), |(sx, sy), (x, y)| (sx + x, sy + y));
    let mean_x = sum_x / n;
    let mean_y = sum_y / n;

    let denom: f64 = points.iter().map(|(x, _)| (x - mean_x).powi(2)).sum();
    let slope = if denom.abs() < f64::EPSILON {
        0.0
    } else {
        points
            .iter()
            .map(|(x, y)| (x - mean_x) * (y - mean_y))
            .sum::<f64>()
            / denom
    };

    xs.iter()
        .map(|&x| (x, (mean_y + slope * (x - mean_x)).clamp(min, max)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reeve_model::entity::trace::TraceStatus;

    fn trace(score: Option<f64>) -> (Trace, f64) {
        (
            Trace {
                id: "t".into(),
                agent_id: "a".into(),
                status: TraceStatus::Completed,
                start_time: 0,
                end_time: Some(1),
                root_span_id: None,
                final_health_score: score,
            },
            0.01,
        )
    }

    #[test]
    fn projection_extends_a_declining_trend() {
        // Scores 90, 80, 70: slope -10 per trace.
        let pts = vec![(0.0, 90.0), (1.0, 80.0), (2.0, 70.0)];
        let projected = project(&pts, &[3.0, 4.0], 0.0, 100.0);
        assert!((projected[0].1 - 60.0).abs() < 0.001);
        assert!((projected[1].1 - 50.0).abs() < 0.001);
    }

    #[test]
    fn projection_clamps_to_bounds() {
        let pts = vec![(0.0, 20.0), (1.0, 10.0)];
        let projected = project(&pts, &[4.0], 0.0, 100.0);
        assert_eq!(projected[0].1, 0.0, "a crashing trend bottoms at zero");
    }

    #[test]
    fn single_point_projects_flat() {
        let pts = vec![(2.0, 42.0)];
        let projected = project(&pts, &[3.0, 4.0], 0.0, 100.0);
        assert!((projected[0].1 - 42.0).abs() < 0.001);
        assert!((projected[1].1 - 42.0).abs() < 0.001);
    }

    #[test]
    fn build_needs_traces_on_both_sides() {
        let history = vec![trace(Some(80.0)), trace(Some(60.0)), trace(Some(90.0))];
        assert!(
            ImpactState::build(&history, 2, "redirect".into()).is_none(),
            "intervention on the last trace has no after"
        );
        assert!(
            ImpactState::build(&history, 0, "redirect".into()).is_none(),
            "intervention on the first trace has no before"
        );
        assert!(ImpactState::build(&history, 1, "redirect".into()).is_some());
    }

    #[test]
    fn build_splits_pre_and_post_at_the_intervention() {
        let history = vec![
            trace(Some(90.0)),
            trace(Some(70.0)),
            trace(Some(50.0)), // intervention issued against this trace
            trace(Some(80.0)),
            trace(Some(95.0)),
        ];
        let impact = ImpactState::build(&history, 2, "redirect".into()).unwrap();
        assert_eq!(
            impact.pre_health.len(),
            3,
            "pre includes the command's trace"
        );
        assert_eq!(impact.post_health.len(), 2);
        assert_eq!(impact.intervention_x, 2.0);
        // Declining 90→70→50 projects to 30 at x=3; actual was 80.
        let projected_at_3 = impact
            .projected_health
            .iter()
            .find(|(x, _)| (*x - 3.0).abs() < 0.001)
            .unwrap()
            .1;
        assert!((projected_at_3 - 30.0).abs() < 0.001);
    }

    #[test]
    fn traces_without_scores_are_skipped_not_zeroed() {
        let history = vec![
            trace(Some(90.0)),
            trace(None),
            trace(Some(70.0)),
            trace(Some(80.0)),
        ];
        let impact = ImpactState::build(&history, 2, "pause".into()).unwrap();
        assert_eq!(
            impact.pre_health.len(),
            2,
            "unscored trace contributes no point"
        );
    }
}
