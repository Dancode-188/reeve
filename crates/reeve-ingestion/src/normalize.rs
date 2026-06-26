use opentelemetry_proto::tonic::common::v1::{AnyValue, any_value::Value as OtlpValue};
use opentelemetry_proto::tonic::trace::v1::Span as OtlpSpan;
use reeve_model::entity::agent::{Agent, AgentStatus, IntegrationPath};
use reeve_model::entity::span::{InternalSpan, SpanStatus};
use reeve_model::entity::span_event::{EventType, SpanEvent};
use reeve_model::ids::Timestamp;
use std::collections::HashMap;
use tokio::sync::mpsc;

/// Carries a single validated, deduplicated OTLP span from the receive stage
/// to normalize, along with the metadata needed to translate it.
pub struct PipelineSpan {
    pub span: OtlpSpan,
    pub service_name: String,
    pub service_instance_id: String,
    pub framework: String,
    pub arrived_at: Timestamp,
    pub clock_offset_ms: i64,
}

pub trait AttributeTranslator: Send + Sync {
    fn version(&self) -> &str;
    fn translate(&self, ps: PipelineSpan) -> (InternalSpan, Vec<SpanEvent>, Agent);
}

/// Implements OTel GenAI semantic conventions (experimental as of v0.1.0).
/// When the convention changes, add a V2 translator alongside this one.
pub struct V1AttributeTranslator {
    capture_content: bool,
}

impl V1AttributeTranslator {
    pub fn new(capture_content: bool) -> Self {
        Self { capture_content }
    }
}

impl AttributeTranslator for V1AttributeTranslator {
    fn version(&self) -> &str {
        "v1"
    }

    fn translate(&self, ps: PipelineSpan) -> (InternalSpan, Vec<SpanEvent>, Agent) {
        let span_id: String = ps
            .span
            .span_id
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();
        let trace_id: String = ps
            .span
            .trace_id
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();
        let parent_id: Option<String> = if ps.span.parent_span_id.is_empty() {
            None
        } else {
            Some(
                ps.span
                    .parent_span_id
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect(),
            )
        };

        let start_time = (ps.span.start_time_unix_nano / 1_000_000) as i64 + ps.clock_offset_ms;
        let end_time = if ps.span.end_time_unix_nano > 0 {
            Some((ps.span.end_time_unix_nano / 1_000_000) as i64 + ps.clock_offset_ms)
        } else {
            None
        };

        // STATUS_CODE_ERROR = 2 in the OTel proto
        let status = match (
            ps.span.end_time_unix_nano > 0,
            ps.span.status.as_ref().map(|s| s.code),
        ) {
            (false, _) => SpanStatus::InFlight,
            (true, Some(2)) => SpanStatus::Failed,
            (true, _) => SpanStatus::Completed,
        };

        const KNOWN_KEYS: &[&str] = &[
            "gen_ai.system",
            "gen_ai.request.model",
            "gen_ai.operation.name",
            "gen_ai.usage.input_tokens",
            "gen_ai.usage.output_tokens",
            "gen_ai.usage.total_tokens",
            "gen_ai.usage.cost",
        ];

        let mut known = serde_json::Map::new();
        let mut raw_attributes: HashMap<String, serde_json::Value> = HashMap::new();

        for kv in ps.span.attributes {
            if KNOWN_KEYS.contains(&kv.key.as_str()) {
                if let Some(v) = anyvalue_to_json(kv.value) {
                    known.insert(kv.key, v);
                }
            } else if let Some(v) = anyvalue_to_json(kv.value) {
                raw_attributes.insert(kv.key, v);
            }
        }

        let internal_span = InternalSpan {
            id: span_id.clone(),
            trace_id: trace_id.clone(),
            parent_id,
            operation: ps.span.name,
            status,
            start_time,
            end_time,
            arrived_at: ps.arrived_at,
            attributes: serde_json::Value::Object(known),
            raw_attributes,
        };

        let events: Vec<SpanEvent> = ps
            .span
            .events
            .into_iter()
            .enumerate()
            .filter_map(|(idx, event)| {
                let event_type = match event.name.as_str() {
                    "gen_ai.user.message" => Some(EventType::UserMessage),
                    "gen_ai.assistant.message" => Some(EventType::AssistantMessage),
                    "gen_ai.tool.message" => Some(EventType::ToolMessage),
                    "gen_ai.choice" => Some(EventType::Choice),
                    name => {
                        tracing::debug!(event_name = %name, "dropping unknown OTLP span event");
                        None
                    }
                }?;

                let occurred_at = if event.time_unix_nano > 0 {
                    (event.time_unix_nano / 1_000_000) as i64 + ps.clock_offset_ms
                } else {
                    ps.arrived_at
                };

                let content = if self.capture_content {
                    event
                        .attributes
                        .into_iter()
                        .find(|kv| matches!(kv.key.as_str(), "gen_ai.prompt" | "content" | "body"))
                        .and_then(|kv| anyvalue_to_json(kv.value))
                        .map(|v| match v {
                            serde_json::Value::String(s) => s,
                            other => other.to_string(),
                        })
                } else {
                    None
                };

                Some(SpanEvent {
                    id: format!("{}:event:{}", span_id, idx),
                    span_id: span_id.clone(),
                    event_type,
                    occurred_at,
                    content,
                })
            })
            .collect();

        // Agent is always produced from resource metadata. The route stage handles
        // "if not exists, insert" when storage is wired up.
        let agent = Agent {
            id: format!("{}:{}", ps.service_name, ps.service_instance_id),
            name: ps.service_name,
            framework: ps.framework,
            integration: IntegrationPath::Sdk,
            status: AgentStatus::Running,
            first_seen_at: ps.arrived_at,
            last_seen_at: ps.arrived_at,
        };

        (internal_span, events, agent)
    }
}

fn anyvalue_to_json(value: Option<AnyValue>) -> Option<serde_json::Value> {
    let v = value?.value?;
    Some(match v {
        OtlpValue::StringValue(s) => serde_json::Value::String(s),
        OtlpValue::BoolValue(b) => serde_json::Value::Bool(b),
        OtlpValue::IntValue(i) => serde_json::json!(i),
        OtlpValue::DoubleValue(d) => serde_json::json!(d),
        OtlpValue::BytesValue(b) => {
            serde_json::Value::String(b.iter().map(|byte| format!("{:02x}", byte)).collect())
        }
        // profiling-signal-only index; not meaningful for trace spans
        OtlpValue::StringValueStrindex(_)
        | OtlpValue::ArrayValue(_)
        | OtlpValue::KvlistValue(_) => return None,
    })
}

/// Typed output from the normalize stage, consumed by the assemble stage.
pub struct NormalizedSpan {
    pub span: InternalSpan,
    pub events: Vec<SpanEvent>,
    pub agent: Agent,
}

pub async fn run(
    mut rx: mpsc::Receiver<PipelineSpan>,
    capture_content: bool,
    tx: mpsc::Sender<NormalizedSpan>,
) {
    let translator = V1AttributeTranslator::new(capture_content);
    while let Some(ps) = rx.recv().await {
        let (span, events, agent) = translator.translate(ps);
        tracing::debug!(
            span_id = %span.id,
            trace_id = %span.trace_id,
            operation = %span.operation,
            agent_id = %agent.id,
            events = events.len(),
            "normalized span",
        );
        if tx.send(NormalizedSpan { span, events, agent }).await.is_err() {
            tracing::warn!("assemble stage receiver dropped, normalize stage shutting down");
            return;
        }
    }
    tracing::info!("normalize stage shut down");
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
    use opentelemetry_proto::tonic::trace::v1::{Span as OtlpSpan, span::Event as OtlpEvent};

    fn make_pipeline_span(span: OtlpSpan) -> PipelineSpan {
        PipelineSpan {
            span,
            service_name: "test-agent".to_string(),
            service_instance_id: "instance-1".to_string(),
            framework: "opentelemetry".to_string(),
            arrived_at: 1_000_000,
            clock_offset_ms: 0,
        }
    }

    fn string_kv(key: &str, val: &str) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(OtlpValue::StringValue(val.to_string())),
            }),
            key_strindex: 0,
        }
    }

    fn int_kv(key: &str, val: i64) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(OtlpValue::IntValue(val)),
            }),
            key_strindex: 0,
        }
    }

    #[test]
    fn known_attrs_land_in_attributes_unknown_in_raw() {
        let translator = V1AttributeTranslator::new(false);
        let span = OtlpSpan {
            span_id: vec![1u8; 8],
            trace_id: vec![2u8; 16],
            end_time_unix_nano: 2_000_000_000,
            start_time_unix_nano: 1_000_000_000,
            attributes: vec![
                string_kv("gen_ai.request.model", "claude-3-5-sonnet"),
                int_kv("gen_ai.usage.input_tokens", 512),
                string_kv("custom.my_app.version", "1.0"),
            ],
            ..Default::default()
        };
        let (internal, _, _) = translator.translate(make_pipeline_span(span));
        assert_eq!(
            internal.attributes["gen_ai.request.model"],
            serde_json::Value::String("claude-3-5-sonnet".to_string())
        );
        assert_eq!(
            internal.attributes["gen_ai.usage.input_tokens"],
            serde_json::json!(512i64)
        );
        assert!(
            internal.attributes.get("custom.my_app.version").is_none(),
            "unknown key must not appear in attributes"
        );
        assert_eq!(
            internal.raw_attributes["custom.my_app.version"],
            serde_json::Value::String("1.0".to_string())
        );
    }

    #[test]
    fn clock_offset_applied_to_timestamps() {
        let translator = V1AttributeTranslator::new(false);
        let mut ps = make_pipeline_span(OtlpSpan {
            span_id: vec![1u8; 8],
            trace_id: vec![2u8; 16],
            start_time_unix_nano: 1_000_000_000,
            end_time_unix_nano: 2_000_000_000,
            ..Default::default()
        });
        ps.clock_offset_ms = -50;
        let (internal, _, _) = translator.translate(ps);
        assert_eq!(internal.start_time, 950);
        assert_eq!(internal.end_time, Some(1950));
    }

    #[test]
    fn otlp_event_names_map_to_correct_event_type() {
        let translator = V1AttributeTranslator::new(false);
        let span = OtlpSpan {
            span_id: vec![1u8; 8],
            trace_id: vec![2u8; 16],
            end_time_unix_nano: 2_000_000_000,
            events: vec![
                OtlpEvent {
                    name: "gen_ai.user.message".to_string(),
                    time_unix_nano: 1_500_000_000,
                    ..Default::default()
                },
                OtlpEvent {
                    name: "gen_ai.choice".to_string(),
                    time_unix_nano: 1_800_000_000,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let (_, events, _) = translator.translate(make_pipeline_span(span));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, EventType::UserMessage);
        assert_eq!(events[1].event_type, EventType::Choice);
    }

    #[test]
    fn unknown_event_names_are_dropped() {
        let translator = V1AttributeTranslator::new(false);
        let span = OtlpSpan {
            span_id: vec![1u8; 8],
            trace_id: vec![2u8; 16],
            end_time_unix_nano: 2_000_000_000,
            events: vec![
                OtlpEvent {
                    name: "some.custom.event".to_string(),
                    ..Default::default()
                },
                OtlpEvent {
                    name: "gen_ai.user.message".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let (_, events, _) = translator.translate(make_pipeline_span(span));
        assert_eq!(events.len(), 1, "unknown event must be dropped");
        assert_eq!(events[0].event_type, EventType::UserMessage);
    }

    #[test]
    fn span_event_content_is_none_when_capture_disabled() {
        let translator = V1AttributeTranslator::new(false);
        let span = OtlpSpan {
            span_id: vec![1u8; 8],
            trace_id: vec![2u8; 16],
            end_time_unix_nano: 2_000_000_000,
            events: vec![OtlpEvent {
                name: "gen_ai.user.message".to_string(),
                attributes: vec![string_kv("content", "hello world")],
                ..Default::default()
            }],
            ..Default::default()
        };
        let (_, events, _) = translator.translate(make_pipeline_span(span));
        assert_eq!(events.len(), 1);
        assert!(
            events[0].content.is_none(),
            "content must be None when capture is disabled"
        );
    }
}
