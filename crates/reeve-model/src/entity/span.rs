use crate::ids::{SpanId, Timestamp, TraceId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanStatus {
    InFlight,
    Completed,
    Failed,
}

/// Reeve's internal span representation. `arrived_at` is the Reeve-side
/// wall clock at arrival, distinct from `start_time`/`end_time` (the
/// agent-side OTel timestamps). It's what makes faithful replay ordering
/// possible. Content lives in `SpanEvent`, not here, per OTel GenAI
/// convention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InternalSpan {
    pub id: SpanId,
    pub trace_id: TraceId,
    pub parent_id: Option<SpanId>,
    pub operation: String,
    pub status: SpanStatus,
    pub start_time: Timestamp,
    pub end_time: Option<Timestamp>,
    pub arrived_at: Timestamp,
    pub attributes: serde_json::Value,
    /// Catch-all for attributes outside the normalized set, since OTel
    /// GenAI semantic conventions are still experimental and will change.
    pub raw_attributes: HashMap<String, serde_json::Value>,
}
