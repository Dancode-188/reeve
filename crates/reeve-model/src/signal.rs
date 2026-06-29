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

/// Produced by the evaluation engine. Consumer: renderer.
#[derive(Clone, Debug)]
pub enum EngineEvent {
    EvaluationComplete {
        trace_id: TraceId,
        span_id: Option<SpanId>,
        metric: String,
        score: f64,
    },
    HealthScoreUpdated {
        agent_id: AgentId,
        trace_id: TraceId,
        score: f64,
        tier2_pending: bool,
    },
    PolicyAlert {
        rule_id: String,
        command_type: String,
        requires_confirmation: bool,
    },
}
