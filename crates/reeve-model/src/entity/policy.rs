use crate::entity::intervention::CommandType;
use crate::ids::RuleId;
use serde::{Deserialize, Serialize};

/// Evaluated Agent > Framework > Global when multiple rules fire
/// simultaneously. More specific scopes take precedence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RuleScope {
    Global,
    Framework(String),
    Agent(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyRule {
    pub id: RuleId,
    pub name: String,
    /// evalexpr DSL string, e.g. `health_score < 30`.
    pub trigger_condition: String,
    pub command_type: CommandType,
    pub requires_confirmation: bool,
    pub cooldown_secs: u64,
    pub scope: RuleScope,
    pub enabled: bool,
    /// When set, the confirmation modal auto-executes after this many
    /// seconds instead of waiting indefinitely for a human response.
    pub auto_confirm_after_secs: Option<u64>,
}
