use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse, trace_service_server::TraceService,
};
use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tonic::{Request, Response, Status};

const DEDUP_MAX: usize = 10_000;
const CLOCK_SAMPLES_NEEDED: usize = 10;

/// Sliding-window deduplicator keyed by raw span_id bytes. Cleared entirely
/// once it hits DEDUP_MAX rather than using LRU, which is enough for
/// OTel retry dedup without the overhead of a true eviction policy.
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

/// Per-peer clock offset estimate. Takes the minimum of the first
/// CLOCK_SAMPLES_NEEDED samples of (arrived_at_ms - span_end_time_ms).
/// Minimum rather than mean: the sample with the least observed delay
/// has the least queueing noise mixed in with the skew signal.
/// See ADR-0004 for why this is an approximation and what replaces it at v0.3.0.
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
    clocks: Arc<Mutex<std::collections::HashMap<String, AgentClockState>>>,
}

impl OtlpReceiver {
    pub fn new() -> Self {
        Self {
            dedup: Arc::new(Mutex::new(Deduplicator::new())),
            clocks: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }
}

impl Default for OtlpReceiver {
    fn default() -> Self {
        Self::new()
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

        for resource_spans in &payload.resource_spans {
            for scope_spans in &resource_spans.scope_spans {
                for span in &scope_spans.spans {
                    if span.span_id.is_empty() || span.trace_id.is_empty() {
                        invalid += 1;
                        continue;
                    }

                    let mut dedup = self.dedup.lock().expect("dedup mutex poisoned");
                    if dedup.is_duplicate(&span.span_id) {
                        duplicates += 1;
                        continue;
                    }
                    drop(dedup);

                    let span_end_ms = (span.end_time_unix_nano / 1_000_000) as i64;
                    if span_end_ms > 0 {
                        let mut clocks = self.clocks.lock().expect("clocks mutex poisoned");
                        clocks
                            .entry(peer_ip.clone())
                            .or_insert_with(AgentClockState::new)
                            .record_sample(arrived_at_ms, span_end_ms);
                    }

                    accepted += 1;
                }
            }
        }

        let offset_ms = self
            .clocks
            .lock()
            .expect("clocks mutex poisoned")
            .get(&peer_ip)
            .map(|s| s.offset_ms())
            .unwrap_or(0);

        if accepted > 0 || duplicates > 0 || invalid > 0 {
            tracing::debug!(
                peer = %peer_ip,
                accepted,
                duplicates,
                invalid,
                clock_offset_ms = offset_ms,
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
        // next insert triggers clear, so this should NOT be a duplicate
        assert!(!d.is_duplicate(b"after-clear"));
        assert_eq!(d.seen.len(), 1);
    }

    #[test]
    fn clock_state_uses_minimum_over_samples() {
        let mut state = AgentClockState::new();
        // feed 10 samples: 9 large (noisy), 1 small (the real skew)
        for i in 0..9 {
            state.record_sample(1000 + i * 50, 0); // offsets: 1000, 1050, ..., 1400
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
            state.record_sample(200, 0); // all 200
        }
        assert_eq!(state.offset_ms(), 200);
        state.record_sample(5, 0); // should be ignored
        assert_eq!(
            state.offset_ms(),
            200,
            "offset must not change after lock-in"
        );
    }
}
