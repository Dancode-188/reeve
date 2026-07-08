use crate::normalize::PipelineSpan;
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse, trace_service_server::TraceService,
};
use opentelemetry_proto::tonic::common::v1::{AnyValue, any_value::Value as OtlpValue};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};

/// NTP clock offsets keyed by agent_id. Populated by reeve-intervention when
/// the four-timestamp exchange completes. When present for a given agent, the
/// NTP offset takes precedence over the sample-based approximation.
pub type NtpOffsets = Arc<Mutex<HashMap<String, i64>>>;

const DEDUP_MAX: usize = 10_000;
const CLOCK_SAMPLES_NEEDED: usize = 10;

struct Deduplicator {
    seen: HashSet<Vec<u8>>,
}

impl Deduplicator {
    fn new() -> Self {
        Self {
            seen: HashSet::new(),
        }
    }

    fn is_duplicate(&mut self, span_id: &[u8]) -> bool {
        if self.seen.len() >= DEDUP_MAX {
            self.seen.clear();
        }
        !self.seen.insert(span_id.to_vec())
    }
}

struct AgentClockState {
    samples: Vec<i64>,
    offset_ms: Option<i64>,
}

impl AgentClockState {
    fn new() -> Self {
        Self {
            samples: Vec::with_capacity(CLOCK_SAMPLES_NEEDED),
            offset_ms: None,
        }
    }

    fn record_sample(&mut self, arrived_at_ms: i64, span_end_ms: i64) {
        if self.offset_ms.is_some() {
            return;
        }
        self.samples.push(arrived_at_ms - span_end_ms);
        if self.samples.len() >= CLOCK_SAMPLES_NEEDED {
            self.offset_ms = self.samples.iter().copied().min();
        }
    }

    fn offset_ms(&self) -> i64 {
        self.offset_ms.unwrap_or(0)
    }
}

pub struct OtlpReceiver {
    dedup: Arc<Mutex<Deduplicator>>,
    clocks: Arc<Mutex<HashMap<String, AgentClockState>>>,
    pipeline_tx: mpsc::Sender<PipelineSpan>,
    ntp_offsets: NtpOffsets,
}

impl OtlpReceiver {
    pub fn new(pipeline_tx: mpsc::Sender<PipelineSpan>, ntp_offsets: NtpOffsets) -> Self {
        Self {
            dedup: Arc::new(Mutex::new(Deduplicator::new())),
            clocks: Arc::new(Mutex::new(HashMap::new())),
            pipeline_tx,
            ntp_offsets,
        }
    }
}

#[tonic::async_trait]
impl TraceService for OtlpReceiver {
    async fn export(
        &self,
        request: Request<ExportTraceServiceRequest>,
    ) -> Result<Response<ExportTraceServiceResponse>, Status> {
        let peer_ip = request
            .remote_addr()
            .map(|a| match a.ip() {
                IpAddr::V6(v6) => v6
                    .to_ipv4_mapped()
                    .map(|v4| v4.to_string())
                    .unwrap_or_else(|| v6.to_string()),
                ip => ip.to_string(),
            })
            .unwrap_or_else(|| "unknown".to_string());

        let arrived_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_millis() as i64;

        let payload = request.into_inner();
        let mut accepted = 0u32;
        let mut duplicates = 0u32;
        let mut invalid = 0u32;

        for resource_spans in payload.resource_spans {
            let (service_name, service_instance_id, framework) =
                extract_resource_meta(resource_spans.resource.as_ref());

            for scope_spans in resource_spans.scope_spans {
                for span in scope_spans.spans {
                    if span.span_id.is_empty() || span.trace_id.is_empty() {
                        invalid += 1;
                        continue;
                    }

                    let is_dup = {
                        let mut dedup = self.dedup.lock().expect("dedup mutex poisoned");
                        dedup.is_duplicate(&span.span_id)
                    };
                    if is_dup {
                        duplicates += 1;
                        continue;
                    }

                    let clock_offset_ms = {
                        // Prefer the NTP-derived offset when the agent has
                        // completed the four-timestamp handshake. The NTP map
                        // is keyed by agent_id, which the SDK sets equal to
                        // service.instance.id in its OTel resource attributes.
                        let ntp = self
                            .ntp_offsets
                            .lock()
                            .expect("ntp_offsets mutex poisoned")
                            .get(&service_instance_id)
                            .copied();
                        if let Some(offset) = ntp {
                            offset
                        } else {
                            let mut clocks = self.clocks.lock().expect("clocks mutex poisoned");
                            let state = clocks
                                .entry(peer_ip.clone())
                                .or_insert_with(AgentClockState::new);
                            let span_end_ms = (span.end_time_unix_nano / 1_000_000) as i64;
                            if span_end_ms > 0 {
                                state.record_sample(arrived_at_ms, span_end_ms);
                            }
                            state.offset_ms()
                        }
                    };

                    let ps = PipelineSpan {
                        span,
                        service_name: service_name.clone(),
                        service_instance_id: service_instance_id.clone(),
                        framework: framework.clone(),
                        arrived_at: arrived_at_ms,
                        clock_offset_ms,
                        integration: reeve_model::entity::IntegrationPath::Sdk,
                    };

                    if self.pipeline_tx.send(ps).await.is_err() {
                        tracing::warn!(
                            peer = %peer_ip,
                            "normalize stage unavailable, span discarded"
                        );
                    }

                    accepted += 1;
                }
            }
        }

        if accepted > 0 || duplicates > 0 || invalid > 0 {
            tracing::debug!(
                peer = %peer_ip,
                accepted,
                duplicates,
                invalid,
                "received span batch",
            );
        }
        if invalid > 0 {
            tracing::warn!(
                peer = %peer_ip,
                invalid,
                "dropped spans with empty span_id or trace_id",
            );
        }

        Ok(Response::new(ExportTraceServiceResponse {
            partial_success: None,
        }))
    }
}

fn extract_resource_meta(
    resource: Option<&opentelemetry_proto::tonic::resource::v1::Resource>,
) -> (String, String, String) {
    let mut service_name = String::from("unknown");
    let mut service_instance_id = String::new();
    let mut framework = String::from("unknown");

    if let Some(r) = resource {
        for kv in &r.attributes {
            match kv.key.as_str() {
                "service.name" => service_name = get_string_value(&kv.value),
                "service.instance.id" => service_instance_id = get_string_value(&kv.value),
                "telemetry.sdk.name" => framework = get_string_value(&kv.value),
                _ => {}
            }
        }
    }

    (service_name, service_instance_id, framework)
}

fn get_string_value(value: &Option<AnyValue>) -> String {
    value
        .as_ref()
        .and_then(|av| av.value.as_ref())
        .and_then(|v| {
            if let OtlpValue::StringValue(s) = v {
                Some(s.as_str())
            } else {
                None
            }
        })
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_rejects_second_occurrence() {
        let mut d = Deduplicator::new();
        assert!(!d.is_duplicate(b"span-1"));
        assert!(d.is_duplicate(b"span-1"));
        assert!(!d.is_duplicate(b"span-2"));
    }

    #[test]
    fn dedup_clears_at_capacity() {
        let mut d = Deduplicator::new();
        for i in 0..DEDUP_MAX {
            let id = i.to_le_bytes().to_vec();
            assert!(!d.is_duplicate(&id));
        }
        assert_eq!(d.seen.len(), DEDUP_MAX);
        assert!(!d.is_duplicate(b"after-clear"));
        assert_eq!(d.seen.len(), 1);
    }

    #[test]
    fn clock_state_uses_minimum_over_samples() {
        let mut state = AgentClockState::new();
        for i in 0..9 {
            state.record_sample(1000 + i * 50, 0);
        }
        assert!(
            state.offset_ms.is_none(),
            "should not lock in before 10 samples"
        );
        state.record_sample(100, 0); // offset: 100, the minimum
        assert_eq!(state.offset_ms(), 100, "should pick the minimum sample");
    }

    #[test]
    fn clock_state_ignores_samples_after_lock_in() {
        let mut state = AgentClockState::new();
        for _ in 0..10 {
            state.record_sample(200, 0);
        }
        assert_eq!(state.offset_ms(), 200);
        state.record_sample(5, 0);
        assert_eq!(
            state.offset_ms(),
            200,
            "offset must not change after lock-in"
        );
    }
}
