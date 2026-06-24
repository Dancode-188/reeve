use crate::ids::{SpanEventId, SpanId, Timestamp};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventType {
    #[serde(rename = "gen_ai.user.message")]
    UserMessage,
    #[serde(rename = "gen_ai.assistant.message")]
    AssistantMessage,
    #[serde(rename = "gen_ai.tool.message")]
    ToolMessage,
    #[serde(rename = "gen_ai.choice")]
    Choice,
}

/// `content` is `None` under privacy tier 1 (metadata only, no message
/// text stored).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpanEvent {
    pub id: SpanEventId,
    pub span_id: SpanId,
    pub event_type: EventType,
    pub occurred_at: Timestamp,
    pub content: Option<String>,
}
