use crate::ids::{AgentId, SpanId, Timestamp, TraceId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceStatus {
    /// Spans arriving normally.
    Running,
    /// gRPC dropped, within the 60s grace period, still in hot memory.
    Disconnected,
    /// Intervention-driven, still in hot memory, awaiting Resume.
    Paused,
    /// Grace period expired, flushed to warm tier.
    Interrupted,
    /// Reeve restarted, reloading from warm tier.
    Resuming,
    /// Root span arrived cleanly, flushed to warm tier.
    Completed,
    /// Root span arrived with error status.
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("cannot transition trace from {from:?} to {to:?}")]
pub struct InvalidTraceTransition {
    pub from: TraceStatus,
    pub to: TraceStatus,
}

impl TraceStatus {
    /// Validates a transition against the 7-state machine. The only valid
    /// transitions are: Running<->Disconnected, Disconnected->Interrupted,
    /// Running<->Paused, Interrupted->Resuming, Resuming->Running, and
    /// Running->Completed/Failed.
    pub fn transition_to(self, next: TraceStatus) -> Result<TraceStatus, InvalidTraceTransition> {
        use TraceStatus::*;
        let valid = matches!(
            (self, next),
            (Running, Disconnected)
                | (Disconnected, Running)
                | (Disconnected, Interrupted)
                | (Running, Paused)
                | (Paused, Running)
                | (Interrupted, Resuming)
                | (Resuming, Running)
                | (Running, Completed)
                | (Running, Failed)
        );
        if valid {
            Ok(next)
        } else {
            Err(InvalidTraceTransition {
                from: self,
                to: next,
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trace {
    pub id: TraceId,
    pub agent_id: AgentId,
    pub status: TraceStatus,
    pub start_time: Timestamp,
    pub end_time: Option<Timestamp>,
    pub root_span_id: Option<SpanId>,
    /// Written on completion. Indexed for history queries.
    pub final_health_score: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use TraceStatus::*;

    #[test]
    fn valid_transitions_succeed() {
        let cases = [
            (Running, Disconnected),
            (Disconnected, Running),
            (Disconnected, Interrupted),
            (Running, Paused),
            (Paused, Running),
            (Interrupted, Resuming),
            (Resuming, Running),
            (Running, Completed),
            (Running, Failed),
        ];
        for (from, to) in cases {
            assert_eq!(
                from.transition_to(to),
                Ok(to),
                "{from:?} -> {to:?} should be valid"
            );
        }
    }

    #[test]
    fn invalid_transitions_fail() {
        let cases = [
            (Completed, Running),
            (Failed, Resuming),
            (Running, Resuming),
            (Paused, Completed),
            (Disconnected, Failed),
            (Resuming, Paused),
        ];
        for (from, to) in cases {
            assert!(
                from.transition_to(to).is_err(),
                "{from:?} -> {to:?} should be invalid"
            );
        }
    }
}
