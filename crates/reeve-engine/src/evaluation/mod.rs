pub mod fingerprint;
/// The composite scoring arithmetic lives in reeve-model so replay can
/// recompute scores exactly as the engine computed them live. Re-exported
/// here to keep the engine-internal path stable.
pub use reeve_model::scoring as health_score;
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
