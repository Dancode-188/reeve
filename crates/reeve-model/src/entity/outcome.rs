use crate::ids::{CommandId, Timestamp, TraceId};
use serde::{Deserialize, Serialize};

/// Measures the quality delta after an intervention, once enough
/// post-intervention spans have been scored. Displayed inline in the
/// trace tree: `↳ redirect +0.58 quality · 4 spans`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterventionOutcome {
    pub id: String,
    pub command_id: CommandId,
    pub trace_id: TraceId,
    pub pre_intervention_score: Option<f64>,
    pub post_intervention_score: Option<f64>,
    /// Positive = improvement.
    pub delta: Option<f64>,
    pub spans_measured: Option<u32>,
    pub measured_at: Timestamp,
}
