use crate::entity::agent::{Agent, AgentStatus};
use crate::ids::{AgentId, SpanId, TraceId};

/// Produced by the ingestion pipeline. Consumers: renderer, evaluation engine.
#[derive(Clone, Debug)]
pub enum IngestionEvent {
    SpanCompleted {
        trace_id: TraceId,
        span_id: SpanId,
    },
    TraceCompleted {
        trace_id: TraceId,
        agent_id: AgentId,
        span_count: usize,
        cost: f64,
    },
    StreamingUpdate {
        trace_id: TraceId,
        span_id: SpanId,
        content: String,
    },
    AgentConnected {
        agent: Agent,
    },
    AgentStatusChanged {
        agent_id: AgentId,
        status: AgentStatus,
    },
}

/// Confidence in an LLM judge result, derived from self-consistency scoring.
/// Tier 1 evaluators are deterministic and carry no confidence value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvaluationConfidence {
    /// Two rubric phrasings agreed within 0.10.
    High,
    /// Two rubric phrasings diverged between 0.10 and 0.30.
    Medium,
    /// Two rubric phrasings diverged by more than 0.30. The result is
    /// excluded from the health score.
    Low,
}

/// Produced by the evaluation engine. Consumer: renderer.
#[derive(Clone, Debug)]
pub enum EngineEvent {
    EvaluationComplete {
        trace_id: TraceId,
        span_id: Option<SpanId>,
        metric: String,
        score: f64,
        /// None for Tier 1 evaluators (deterministic; no second pass).
        confidence: Option<EvaluationConfidence>,
    },
    HealthScoreUpdated {
        agent_id: AgentId,
        trace_id: TraceId,
        score: f64,
        tier2_pending: bool,
        weight_coverage: f64,
    },
    /// Emitted once on engine startup after the Ollama probe completes.
    EvaluationBackendReady {
        /// Human-readable backend description, e.g. "local (phi4-mini)" or "disabled".
        backend: String,
        /// Why the backend is disabled, if applicable.
        reason: Option<String>,
    },
    PolicyAlert {
        rule_id: String,
        command_type: String,
        requires_confirmation: bool,
    },
}
