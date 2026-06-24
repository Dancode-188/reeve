use crate::ids::{SpanId, Timestamp};
use serde::{Deserialize, Serialize};

/// A developer annotation added during live observation or replay.
/// Displayed as a ♦ indicator in the trace tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpanNote {
    pub id: String,
    pub span_id: SpanId,
    pub content: String,
    pub created_at: Timestamp,
}
