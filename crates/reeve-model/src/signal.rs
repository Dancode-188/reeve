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
    /// A pipeline-health notice for the developer: surfaced in ALERTS,
    /// dismissible, advisory rather than actionable by command.
    PipelineWarning {
        message: String,
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

/// Direction of cost rate change. Emitted by the engine once enough cost
/// samples have accumulated (minimum 3 deltas). Until then the field is None
/// in AgentState and no trend arrow is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CostTrend {
    Accelerating,
    Stable,
    Decelerating,
}

/// The best-performing intervention for a firing rule, from the SQL
/// aggregation over measured outcomes. Carried on PolicyAlert so the
/// renderer can show what has historically worked without querying.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectivenessHint {
    /// Lowercase command tag, e.g. "redirect".
    pub command: String,
    /// Average measured quality delta across the samples. Positive = helped.
    pub avg_delta: f64,
    pub sample_count: u32,
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
        /// Active privacy tier. 1 = default (no content capture); 2+ = content capture
        /// enabled. Always 1 until config loading ships in issue #65.
        privacy_tier: u8,
    },
    PolicyAlert {
        rule_id: String,
        /// Human-readable description owned by the engine and carried from the firing
        /// PolicyRule. The renderer never looks this up; user-defined rules would break
        /// any renderer-side table.
        description: String,
        command_type: String,
        requires_confirmation: bool,
        /// When set, the renderer shows a countdown bar and auto-dispatches the command
        /// after this many seconds if the operator does not act first.
        auto_confirm_after_secs: Option<u64>,
        /// What has historically worked when this rule fired: the command type
        /// with the best measured average quality delta, from the effectiveness
        /// aggregation over intervention outcomes. None until enough outcomes
        /// exist. Owned by the engine like `description`; the renderer only
        /// formats it.
        effectiveness: Option<EffectivenessHint>,
    },
    /// An agent completed the gRPC handshake on the control channel.
    /// `capabilities` lists which command types the adapter supports
    /// (e.g. "pause", "redirect").
    AgentControlConnected {
        agent_id: AgentId,
        capabilities: Vec<String>,
    },
    AgentControlDisconnected {
        agent_id: AgentId,
    },
}
