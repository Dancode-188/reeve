pub mod fingerprint;
pub mod heuristic;
pub mod llm_judge;

use fingerprint::AgentFingerprint;
use reeve_model::entity::span::InternalSpan;
use reeve_model::ids::{AgentId, TraceId};

pub struct TraceContext<'a> {
    pub trace_id: TraceId,
    pub agent_id: AgentId,
    pub span_count: usize,
    pub cost: f64,
    pub spans: &'a [InternalSpan],
    pub fingerprint: Option<&'a AgentFingerprint>,
}
