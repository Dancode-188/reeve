use crate::ids::{AgentId, Timestamp};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Running,
    Paused,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationPath {
    Sdk,
    Proxy,
    Log,
}

/// Auto-created on first span arrival from `service.name` +
/// `service.instance.id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Agent {
    pub id: AgentId,
    pub name: String,
    pub framework: String,
    pub integration: IntegrationPath,
    pub status: AgentStatus,
    pub first_seen_at: Timestamp,
    pub last_seen_at: Timestamp,
}
