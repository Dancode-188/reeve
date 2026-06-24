use crate::ids::Timestamp;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostEntityType {
    Trace,
    Agent,
}

/// Polymorphic rolling accumulator: `entity_id`/`entity_type` lets a
/// ledger row belong to either a trace or an agent, rather than fixing the
/// relationship to one or the other.
///
/// `CostAccumulator`, the in-memory runtime counter that feeds this on
/// trace completion or hot-tier eviction, lives in `reeve-ingestion`, not
/// here. It's a pipeline detail, not a persisted entity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostLedger {
    pub id: String,
    pub entity_id: String,
    pub entity_type: CostEntityType,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_cost_usd: f64,
    pub updated_at: Timestamp,
}
