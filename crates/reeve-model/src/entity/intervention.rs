use crate::ids::{AgentId, CommandId, RuleId, SpanId, Timestamp, TraceId};
use serde::{Deserialize, Serialize};

/// The domain-level command shape. Carries its data inline, unlike the
/// proto wire format, which stays a flat enum with a generic payload
/// string for protobuf's zero-value convention. `reeve-intervention`
/// converts between the two at the gRPC boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CommandType {
    Pause,
    Resume,
    Kill,
    Redirect { instruction: String },
    InjectContext { context: String },
}

/// Server-side command lifecycle, stored in `intervention_commands.status`.
/// Distinct from `AckStatus`: this has pre-ack states that never appear on
/// the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    PendingConfirmation,
    Pending,
    Delivered,
    Applied,
    Failed,
    Expired,
    Cancelled,
}

/// Domain-side ack status. No `Unspecified` variant: that's a proto-only
/// artifact for the wire format's zero value, never meaningful here.
/// `Applying` is what lets the renderer show "pause pending · waiting for
/// yield point" instead of looking broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AckStatus {
    Received,
    Applying,
    Applied,
    Failed,
    Expired,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterventionCommand {
    pub id: CommandId,
    pub trace_id: TraceId,
    pub span_id: Option<SpanId>,
    /// `None` when human-issued rather than policy-issued.
    pub policy_id: Option<RuleId>,
    pub command_type: CommandType,
    pub status: CommandStatus,
    pub requires_confirmation: bool,
    pub issued_at: Timestamp,
    pub acknowledged_at: Option<Timestamp>,
    /// "human" or "policy:rule_id".
    pub issued_by: String,
    pub valid_until_ms: Timestamp,
}

/// A command the agent confirmed it applied. The dispatcher records these
/// for the engine's outcome measurement, which compares quality before and
/// after the intervention. Lives in `reeve-model` because the engine must
/// not depend on `reeve-intervention` (see ADR-0029); the shared feed is
/// the same pattern as the NTP offset map and the paused-agents set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppliedCommand {
    pub command_id: CommandId,
    pub trace_id: TraceId,
    pub agent_id: AgentId,
    pub command_type: CommandType,
    pub applied_at_ms: Timestamp,
}

/// A command destined for a proxy-path agent, waiting for that agent's
/// next request to pass through the proxy. Queued by the dispatcher,
/// drained by the proxy: the same shared-state pattern as the paused
/// set and the applied-commands feed.
#[derive(Debug, Clone, PartialEq)]
pub struct ProxyCommand {
    pub id: crate::ids::CommandId,
    pub payload: ProxyPayload,
    /// Expiry inherited from the intervention command; the proxy drops
    /// rather than applies a command whose moment has passed.
    pub valid_until_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProxyPayload {
    Redirect { instruction: String },
    InjectContext { context: String },
}

/// One applied proxy command, reported back for ack processing:
/// (command id, agent id, applied-at wall clock ms).
pub type ProxyApplied = (crate::ids::CommandId, crate::ids::AgentId, i64);

/// The two-sided queue: pending commands per agent, and applications
/// waiting for the dispatcher to fold into its ack handling.
#[derive(Debug, Default)]
pub struct ProxyInterventionState {
    pub pending:
        std::collections::HashMap<crate::ids::AgentId, std::collections::VecDeque<ProxyCommand>>,
    pub applied: Vec<ProxyApplied>,
    /// The circuit breaker: agents here have their Messages requests
    /// refused instead of forwarded.
    pub killed: std::collections::HashSet<crate::ids::AgentId>,
}

pub type ProxyInterventions = std::sync::Arc<std::sync::Mutex<ProxyInterventionState>>;
