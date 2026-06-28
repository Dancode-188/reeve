use crate::entity::agent::{Agent, AgentStatus};
use crate::ids::{AgentId, SpanId, TraceId};

#[derive(Clone, Debug)]
pub enum EngineSignal {
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
