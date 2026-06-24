use crate::ids::{Timestamp, TraceId};
use serde::{Deserialize, Serialize};

/// Gates whether replay can show LLM response text or only tree structure
/// and timing. This is not a precomputed event log. Replay is
/// reconstructed live by querying spans, span_events, evaluation_results,
/// and intervention_commands in `arrived_at` order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayRecord {
    pub id: String,
    pub trace_id: TraceId,
    pub content_captured: bool,
    pub captured_at: Timestamp,
}
