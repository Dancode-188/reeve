use crate::assemble::{CompletionState, InFlightTrace};
use reeve_model::entity::agent::AgentStatus;
use reeve_model::entity::span::SpanStatus;
use reeve_model::entity::trace::{Trace, TraceStatus};
use reeve_model::ids::AgentId;
use reeve_model::signal::IngestionEvent;
use reeve_storage::hot::HotStore;
use reeve_storage::warm::WarmStore;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};

pub struct Router {
    hot: Arc<Mutex<HotStore>>,
    warm: Arc<WarmStore>,
    signal_tx: broadcast::Sender<IngestionEvent>,
    seen_agents: HashSet<AgentId>,
    last_batch_warning: Option<std::time::Instant>,
}

impl Router {
    pub fn new(
        hot: Arc<Mutex<HotStore>>,
        warm: Arc<WarmStore>,
        signal_tx: broadcast::Sender<IngestionEvent>,
    ) -> Self {
        Self {
            hot,
            warm,
            signal_tx,
            seen_agents: HashSet::new(),
            last_batch_warning: None,
        }
    }

    /// A trace whose spans consistently arrived long after they started
    /// means the agent's OTel exporter batches on the default interval,
    /// and the live cockpit is quietly seconds behind reality. One
    /// warning per interval; a persistently misconfigured agent should
    /// nag, not spam.
    fn check_batch_latency(&mut self, trace: &InFlightTrace) {
        const DELTA_THRESHOLD_MS: i64 = 4_000;
        const MIN_SPANS: usize = 3;
        const WARN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

        if trace.spans.len() < MIN_SPANS {
            return;
        }
        let laggy = trace
            .spans
            .values()
            .filter(|s| s.arrived_at - s.start_time > DELTA_THRESHOLD_MS)
            .count();
        if laggy * 10 < trace.spans.len() * 8 {
            return; // fewer than 80% of spans lag: not a batching signature
        }
        let now = std::time::Instant::now();
        if self
            .last_batch_warning
            .is_some_and(|at| now.duration_since(at) < WARN_INTERVAL)
        {
            return;
        }
        self.last_batch_warning = Some(now);
        let _ = self.signal_tx.send(IngestionEvent::PipelineWarning {
            message: format!(
                "{} delivers spans seconds late (OTel batching); set schedule_delay_millis=500 in its exporter",
                trace.agent.name
            ),
        });
    }

    pub async fn route(&mut self, trace: InFlightTrace, state: CompletionState) {
        if state == CompletionState::Completed {
            self.check_batch_latency(&trace);
        }
        let trace_id = trace.trace_id.clone();
        let agent_id = trace.agent.id.clone();
        // Pending spans count and persist like attached ones everywhere
        // below: a span that entered the pipeline must reach storage,
        // even when its parent (the turn root, emitted last) never came.
        let span_count = trace.spans.len() + trace.pending_attachment.len();
        let cost = trace.cost_accumulator;

        let status = match state {
            CompletionState::Completed => {
                // A root span with SpanStatus::Failed means the agent run failed.
                let root_failed = trace
                    .root_span_id
                    .as_ref()
                    .and_then(|id| trace.spans.get(id))
                    .is_some_and(|s| s.status == SpanStatus::Failed);
                if root_failed {
                    TraceStatus::Failed
                } else {
                    TraceStatus::Completed
                }
            }
            CompletionState::Interrupted | CompletionState::InterruptedResumable => {
                TraceStatus::Interrupted
            }
            CompletionState::InFlight => {
                tracing::warn!(
                    trace_id = %trace_id,
                    "route received an InFlight trace; this should not happen"
                );
                return;
            }
        };

        let (start_time, end_time) = trace
            .root_span_id
            .as_ref()
            .and_then(|id| trace.spans.get(id))
            .map(|root| (root.start_time, root.end_time))
            .unwrap_or_else(|| {
                // A rootless flush is usually all-pending (children of an
                // unarrived turn root), so the earliest must look there
                // too: this used to fall through to 0 and render as a
                // 56-year duration in history.
                let earliest = trace
                    .spans
                    .values()
                    .chain(trace.pending_attachment.values())
                    .map(|s| s.start_time)
                    .min()
                    .unwrap_or(0);
                (earliest, None)
            });

        let trace_entity = Trace {
            id: trace_id.clone(),
            agent_id: agent_id.clone(),
            status,
            start_time,
            end_time,
            root_span_id: trace.root_span_id.clone(),
            final_health_score: None,
        };

        // Hot store writes are synchronous; collect any evicted spans to flush
        // to warm store after releasing the lock.
        let mut evicted: Vec<_> = Vec::new();
        {
            let mut hot = self.hot.lock().expect("hot store mutex poisoned");
            hot.upsert_trace(trace_entity.clone());
            for span in trace
                .spans
                .values()
                .chain(trace.pending_attachment.values())
            {
                if let Some(e) = hot.push_span(span.clone()) {
                    evicted.push(e);
                }
            }
        }

        for span in evicted {
            if let Err(e) = self.warm.save_span(span).await {
                tracing::error!(error = %e, "failed to flush evicted span to warm store");
            }
        }

        let is_first_encounter = self.seen_agents.insert(agent_id.clone());

        if let Err(e) = self.warm.upsert_agent(trace.agent.clone()).await {
            tracing::error!(trace_id = %trace_id, error = %e, "failed to upsert agent");
        }

        if is_first_encounter {
            let _ = self.signal_tx.send(IngestionEvent::AgentConnected {
                agent: trace.agent.clone(),
            });
        }

        let resumable = state == CompletionState::InterruptedResumable;
        // One transaction per finalized trace instead of one per
        // statement: the per-statement pattern paid an fsync each and
        // capped write throughput low enough that the soak's pipeline
        // backed up 85 minutes (#246).
        let spans: Vec<_> = trace
            .spans
            .into_values()
            .chain(trace.pending_attachment.into_values())
            .collect();
        let events: Vec<_> = trace.span_events.into_values().flatten().collect();
        if let Err(e) = self
            .warm
            .save_finalized_trace(trace_entity, spans, events)
            .await
        {
            tracing::error!(trace_id = %trace_id, error = %e, "failed to save finalized trace");
        }
        if resumable {
            if let Err(e) = self.warm.mark_resumable(&trace_id).await {
                tracing::warn!(error = %e, trace_id = %trace_id, "failed to mark trace resumable");
            }
        }

        let agent_status = match status {
            TraceStatus::Failed => AgentStatus::Error,
            _ => AgentStatus::Idle,
        };
        let _ = self.signal_tx.send(IngestionEvent::AgentStatusChanged {
            agent_id: agent_id.clone(),
            status: agent_status,
        });
        let _ = self.signal_tx.send(IngestionEvent::TraceCompleted {
            trace_id: trace_id.clone(),
            agent_id: agent_id.clone(),
            span_count,
            cost,
        });

        tracing::debug!(
            trace_id = %trace_id,
            agent_id = %agent_id,
            status = ?status,
            spans = span_count,
            "trace routed to storage",
        );
    }
}

pub async fn run(
    mut rx: mpsc::Receiver<(InFlightTrace, CompletionState)>,
    hot: Arc<Mutex<HotStore>>,
    warm: Arc<WarmStore>,
    signal_tx: broadcast::Sender<IngestionEvent>,
) {
    let mut router = Router::new(hot, warm, signal_tx);
    while let Some((trace, state)) = rx.recv().await {
        router.route(trace, state).await;
    }
    tracing::info!("route stage shut down");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assemble::InFlightTrace;
    use reeve_model::entity::Agent;
    use reeve_model::entity::agent::{AgentStatus, IntegrationPath};
    use reeve_model::entity::span::{InternalSpan, SpanStatus};
    use reeve_model::ids::{SpanId, TraceId};
    use reeve_storage::warm::WarmStore;
    use std::collections::HashMap;

    fn make_agent() -> Agent {
        Agent {
            id: "agent-1".into(),
            name: "test-service".to_string(),
            framework: "custom".to_string(),
            integration: IntegrationPath::Sdk,
            status: AgentStatus::Running,
            first_seen_at: 0,
            last_seen_at: 0,
        }
    }

    fn make_router(hot_capacity: usize) -> (Router, Arc<WarmStore>) {
        let hot = Arc::new(Mutex::new(HotStore::new(hot_capacity)));
        let warm = Arc::new(WarmStore::open_in_memory().unwrap());
        let (signal_tx, _) = broadcast::channel(16);
        let router = Router::new(hot, warm.clone(), signal_tx);
        (router, warm)
    }

    fn make_span(id: &str, trace_id: &str, parent_id: Option<&str>) -> InternalSpan {
        InternalSpan {
            id: id.into(),
            trace_id: trace_id.into(),
            parent_id: parent_id.map(Into::into),
            operation: "test.op".to_string(),
            status: SpanStatus::Completed,
            start_time: 1000,
            end_time: Some(2000),
            arrived_at: 1001,
            attributes: serde_json::Value::Object(serde_json::Map::new()),
            raw_attributes: HashMap::new(),
        }
    }

    fn make_trace_with_spans(trace_id: &str, span_ids: &[&str]) -> InFlightTrace {
        let mut trace = InFlightTrace::new(trace_id.into(), make_agent());
        for (i, id) in span_ids.iter().enumerate() {
            let parent = if i == 0 { None } else { Some(span_ids[0]) };
            trace.receive_span(make_span(id, trace_id, parent), vec![]);
        }
        trace
    }

    #[tokio::test]
    async fn completed_trace_is_flushed_to_warm_store() {
        let (mut router, warm) = make_router(10_000);
        let trace = make_trace_with_spans("trace-1", &["root-1", "child-1", "child-2"]);

        router.route(trace, CompletionState::Completed).await;

        let saved = warm.get_trace(&TraceId::from("trace-1")).await.unwrap();
        assert!(saved.is_some(), "trace must be in warm store after routing");
        assert_eq!(saved.unwrap().status, TraceStatus::Completed);

        let span = warm.get_span(&SpanId::from("root-1")).await.unwrap();
        assert!(span.is_some(), "spans must be flushed to warm store");
    }

    #[tokio::test]
    async fn interrupted_flush_persists_pending_spans() {
        // The #182 shape: a long turn's chats parent to a root that has
        // not arrived, so at flush time every span is pending. A real
        // session lost ~30 round trips of spans here.
        let (mut router, warm) = make_router(10_000);
        let mut trace = InFlightTrace::new("trace-1".into(), make_agent());
        trace.receive_span(
            make_span("chat-1", "trace-1", Some("unarrived-root")),
            vec![],
        );
        trace.receive_span(
            make_span("chat-2", "trace-1", Some("unarrived-root")),
            vec![],
        );
        assert!(trace.spans.is_empty(), "everything waits in pending");

        router.route(trace, CompletionState::Interrupted).await;

        for id in ["chat-1", "chat-2"] {
            assert!(
                warm.get_span(&SpanId::from(id)).await.unwrap().is_some(),
                "pending span {id} must survive the flush"
            );
        }
        let saved = warm
            .get_trace(&TraceId::from("trace-1"))
            .await
            .unwrap()
            .expect("trace saved");
        assert_eq!(
            saved.start_time, 1000,
            "started_at comes from the pending spans, never 0"
        );
    }

    #[tokio::test]
    async fn hot_store_eviction_flushes_evicted_span_to_warm_store() {
        // Capacity 1: the second span push evicts the first.
        let (mut router, warm) = make_router(1);
        let trace = make_trace_with_spans("trace-1", &["root-1", "child-1"]);

        router.route(trace, CompletionState::Completed).await;

        // Both spans end up in warm store: one via eviction, one via normal flush.
        let root = warm.get_span(&SpanId::from("root-1")).await.unwrap();
        let child = warm.get_span(&SpanId::from("child-1")).await.unwrap();
        assert!(root.is_some(), "evicted span must be in warm store");
        assert!(child.is_some(), "remaining span must be in warm store");
    }

    #[tokio::test]
    async fn interrupted_trace_saved_with_interrupted_status() {
        let (mut router, warm) = make_router(10_000);
        let trace = make_trace_with_spans("trace-1", &["child-1"]);

        router.route(trace, CompletionState::Interrupted).await;

        let saved = warm.get_trace(&TraceId::from("trace-1")).await.unwrap();
        assert!(saved.is_some());
        assert_eq!(
            saved.unwrap().status,
            TraceStatus::Interrupted,
            "interrupted traces must be saved with Interrupted status"
        );
    }

    #[tokio::test]
    async fn agent_connected_signal_fires_on_first_encounter() {
        let hot = Arc::new(Mutex::new(HotStore::new(10_000)));
        let warm = Arc::new(WarmStore::open_in_memory().unwrap());
        let (signal_tx, mut signal_rx) = broadcast::channel(16);
        let mut router = Router::new(hot, warm, signal_tx);

        let trace = make_trace_with_spans("trace-1", &["root-1"]);
        router.route(trace, CompletionState::Completed).await;

        let signals: Vec<_> = std::iter::from_fn(|| signal_rx.try_recv().ok()).collect();
        let has_connected = signals
            .iter()
            .any(|s| matches!(s, IngestionEvent::AgentConnected { .. }));
        assert!(has_connected, "AgentConnected must fire on first encounter");

        // Second trace from same agent must not fire AgentConnected again.
        let trace2 = make_trace_with_spans("trace-2", &["root-2"]);
        router.route(trace2, CompletionState::Completed).await;

        let signals2: Vec<_> = std::iter::from_fn(|| signal_rx.try_recv().ok()).collect();
        let connected_again = signals2
            .iter()
            .any(|s| matches!(s, IngestionEvent::AgentConnected { .. }));
        assert!(
            !connected_again,
            "AgentConnected must not fire on subsequent traces"
        );
    }

    #[tokio::test]
    async fn laggy_trace_raises_one_batch_warning() {
        let hot = Arc::new(Mutex::new(HotStore::new(1000)));
        let warm = Arc::new(WarmStore::open_in_memory().unwrap());
        let (signal_tx, mut signal_rx) = broadcast::channel(16);
        let mut router = Router::new(hot, warm, signal_tx);

        // Every span arrived 5s after it started: the batching signature.
        let mut trace = InFlightTrace::new("t-lag".into(), make_agent());
        for (i, id) in ["r", "a", "b"].iter().enumerate() {
            let mut span = make_span(id, "t-lag", if i == 0 { None } else { Some("r") });
            span.start_time = 1_000;
            span.arrived_at = 6_500;
            trace.receive_span(span, vec![]);
        }
        router.route(trace, CompletionState::Completed).await;

        let mut warned = 0;
        while let Ok(ev) = signal_rx.try_recv() {
            if let IngestionEvent::PipelineWarning { message } = ev {
                assert!(message.contains("schedule_delay_millis"));
                warned += 1;
            }
        }
        assert_eq!(warned, 1, "the batching signature warns exactly once");

        // A second laggy trace inside the throttle window stays silent.
        let mut trace2 = InFlightTrace::new("t-lag-2".into(), make_agent());
        for (i, id) in ["r", "a", "b"].iter().enumerate() {
            let mut span = make_span(id, "t-lag-2", if i == 0 { None } else { Some("r") });
            span.start_time = 1_000;
            span.arrived_at = 6_500;
            trace2.receive_span(span, vec![]);
        }
        router.route(trace2, CompletionState::Completed).await;
        let mut warned_again = false;
        while let Ok(ev) = signal_rx.try_recv() {
            if matches!(ev, IngestionEvent::PipelineWarning { .. }) {
                warned_again = true;
            }
        }
        assert!(!warned_again, "warnings are throttled, not per trace");
    }

    #[tokio::test]
    async fn prompt_traces_never_warn() {
        let hot = Arc::new(Mutex::new(HotStore::new(1000)));
        let warm = Arc::new(WarmStore::open_in_memory().unwrap());
        let (signal_tx, mut signal_rx) = broadcast::channel(16);
        let mut router = Router::new(hot, warm, signal_tx);

        // make_span arrives 1ms after start: healthy delivery.
        let trace = make_trace_with_spans("t-ok", &["r", "a", "b"]);
        router.route(trace, CompletionState::Completed).await;
        while let Ok(ev) = signal_rx.try_recv() {
            assert!(
                !matches!(ev, IngestionEvent::PipelineWarning { .. }),
                "prompt delivery must not warn"
            );
        }
    }
}
