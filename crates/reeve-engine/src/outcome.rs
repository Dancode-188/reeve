use reeve_model::entity::intervention::{AppliedCommand, CommandType};
use reeve_model::entity::outcome::InterventionOutcome;
use reeve_model::ids::AgentId;
use std::collections::HashMap;

/// How many post-intervention health scores to collect before writing the
/// outcome. Health scores land once per completed trace, so this is three
/// traces of evidence: enough to smooth a single lucky or unlucky trace,
/// short enough that the outcome line appears while the intervention is
/// still fresh in the developer's mind.
const POST_INTERVENTION_SCORES: usize = 3;

struct PendingMeasurement {
    command: AppliedCommand,
    pre_score: Option<f64>,
    post_scores: Vec<f64>,
    spans_measured: u32,
}

/// Measures whether interventions worked. When a command is confirmed
/// applied, the agent's most recent health score is captured as the
/// before-picture; the scores of the next few completed traces become the
/// after-picture; the delta is written as an `InterventionOutcome`.
///
/// Measurement deliberately spans traces: the live intervention runs showed
/// that a command usually applies moments before its own trace completes,
/// so an in-trace window would almost always measure nothing.
#[derive(Default)]
pub struct OutcomeTracker {
    pending: HashMap<AgentId, Vec<PendingMeasurement>>,
}

impl OutcomeTracker {
    /// Registers an applied command for measurement. `pre_score` is the
    /// agent's last known health score at pickup time, before any trace
    /// that completed after the command applied. Kill is not measured:
    /// a killed agent produces no post-intervention behavior to score.
    pub fn command_applied(&mut self, command: AppliedCommand, pre_score: Option<f64>) {
        if command.command_type == CommandType::Kill {
            return;
        }
        self.pending
            .entry(command.agent_id.clone())
            .or_default()
            .push(PendingMeasurement {
                command,
                pre_score,
                post_scores: Vec::new(),
                spans_measured: 0,
            });
    }

    /// Feeds one completed trace's health score to every measurement pending
    /// for that agent. Returns the outcomes that completed on this score.
    pub fn trace_scored(
        &mut self,
        agent_id: &AgentId,
        score: f64,
        span_count: u32,
        now_ms: i64,
    ) -> Vec<InterventionOutcome> {
        let Some(pending) = self.pending.get_mut(agent_id) else {
            return Vec::new();
        };

        let mut finished = Vec::new();
        pending.retain_mut(|m| {
            m.post_scores.push(score);
            m.spans_measured += span_count;
            if m.post_scores.len() < POST_INTERVENTION_SCORES {
                return true;
            }
            let post = m.post_scores.iter().sum::<f64>() / m.post_scores.len() as f64;
            finished.push(InterventionOutcome {
                id: format!("out-{:x}-{}", now_ms, m.command.command_id),
                command_id: m.command.command_id.clone(),
                trace_id: m.command.trace_id.clone(),
                pre_intervention_score: m.pre_score,
                post_intervention_score: Some(post),
                delta: m.pre_score.map(|pre| post - pre),
                spans_measured: Some(m.spans_measured),
                measured_at: now_ms,
            });
            false
        });
        if pending.is_empty() {
            self.pending.remove(agent_id);
        }
        finished
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reeve_model::ids::{CommandId, TraceId};

    fn applied(agent: &str, cmd: &str, command_type: CommandType) -> AppliedCommand {
        AppliedCommand {
            command_id: CommandId::from(cmd),
            trace_id: TraceId::from("trace-1"),
            agent_id: AgentId::from(agent),
            command_type,
            applied_at_ms: 1_000,
        }
    }

    fn redirect() -> CommandType {
        CommandType::Redirect {
            instruction: "steer".to_string(),
        }
    }

    #[test]
    fn outcome_written_after_three_post_scores() {
        let mut tracker = OutcomeTracker::default();
        let agent = AgentId::from("agent-1");
        tracker.command_applied(applied("agent-1", "cmd-1", redirect()), Some(40.0));

        assert!(tracker.trace_scored(&agent, 70.0, 5, 2_000).is_empty());
        assert!(tracker.trace_scored(&agent, 80.0, 4, 3_000).is_empty());
        let outcomes = tracker.trace_scored(&agent, 90.0, 3, 4_000);

        assert_eq!(outcomes.len(), 1);
        let o = &outcomes[0];
        assert_eq!(o.pre_intervention_score, Some(40.0));
        assert_eq!(o.post_intervention_score, Some(80.0), "mean of 70/80/90");
        assert_eq!(o.delta, Some(40.0), "positive delta means improvement");
        assert_eq!(o.spans_measured, Some(12));
        assert_eq!(o.command_id.as_str(), "cmd-1");
    }

    #[test]
    fn negative_delta_when_quality_dropped() {
        let mut tracker = OutcomeTracker::default();
        let agent = AgentId::from("agent-1");
        tracker.command_applied(applied("agent-1", "cmd-1", redirect()), Some(90.0));

        tracker.trace_scored(&agent, 50.0, 1, 0);
        tracker.trace_scored(&agent, 50.0, 1, 0);
        let outcomes = tracker.trace_scored(&agent, 50.0, 1, 0);
        assert_eq!(outcomes[0].delta, Some(-40.0));
    }

    #[test]
    fn kill_is_not_measured() {
        let mut tracker = OutcomeTracker::default();
        let agent = AgentId::from("agent-1");
        tracker.command_applied(applied("agent-1", "cmd-1", CommandType::Kill), Some(40.0));

        for _ in 0..POST_INTERVENTION_SCORES {
            assert!(tracker.trace_scored(&agent, 90.0, 1, 0).is_empty());
        }
    }

    #[test]
    fn missing_pre_score_yields_no_delta() {
        let mut tracker = OutcomeTracker::default();
        let agent = AgentId::from("agent-1");
        // First-ever trace for this agent: no history existed at apply time.
        tracker.command_applied(applied("agent-1", "cmd-1", redirect()), None);

        tracker.trace_scored(&agent, 70.0, 1, 0);
        tracker.trace_scored(&agent, 70.0, 1, 0);
        let outcomes = tracker.trace_scored(&agent, 70.0, 1, 0);
        assert_eq!(outcomes[0].pre_intervention_score, None);
        assert_eq!(outcomes[0].delta, None, "no before-picture, no delta claim");
        assert_eq!(outcomes[0].post_intervention_score, Some(70.0));
    }

    #[test]
    fn scores_from_other_agents_do_not_count() {
        let mut tracker = OutcomeTracker::default();
        let other = AgentId::from("agent-2");
        tracker.command_applied(applied("agent-1", "cmd-1", redirect()), Some(40.0));

        for _ in 0..10 {
            assert!(tracker.trace_scored(&other, 90.0, 1, 0).is_empty());
        }
    }

    #[test]
    fn concurrent_measurements_complete_independently() {
        let mut tracker = OutcomeTracker::default();
        let agent = AgentId::from("agent-1");
        tracker.command_applied(applied("agent-1", "cmd-1", redirect()), Some(40.0));
        tracker.trace_scored(&agent, 60.0, 1, 0);
        // Second command applies one trace into the first's window.
        tracker.command_applied(applied("agent-1", "cmd-2", redirect()), Some(60.0));

        tracker.trace_scored(&agent, 70.0, 1, 0);
        let first = tracker.trace_scored(&agent, 80.0, 1, 0);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].command_id.as_str(), "cmd-1");

        let second = tracker.trace_scored(&agent, 90.0, 1, 0);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].command_id.as_str(), "cmd-2");
    }
}
