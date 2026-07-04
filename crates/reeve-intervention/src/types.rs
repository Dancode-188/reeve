use reeve_model::entity::intervention::AckStatus;
use reeve_model::ids::{AgentId, CommandId};

/// Forwarded by the control server to the dispatcher when an agent
/// sends a `CommandAck` on the control stream. The server converts
/// from the proto wire type at the gRPC boundary before sending here.
pub struct AckNotification {
    pub command_id: CommandId,
    pub agent_id: AgentId,
    pub status: AckStatus,
}
