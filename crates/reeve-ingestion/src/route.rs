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
        }
    }

    pub async fn route(&mut self, trace: InFlightTrace, state: CompletionState) {
        let trace_id = trace.trace_id.clone();
        let agent_id = trace.agent.id.clone();
        let span_count = trace.spans.len();
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
                let earliest = trace
                    .spans
                    .values()
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
            for span in trace.spans.values() {
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
        if let Err(e) = self.warm.save_trace(trace_entity).await {
            tracing::error!(trace_id = %trace_id, error = %e, "failed to save trace");
        }
        if resumable {
            if let Err(e) = self.warm.mark_resumable(&trace_id).await {
                tracing::warn!(error = %e, trace_id = %trace_id, "failed to mark trace resumable");
            }
        }

        for (_, span) in trace.spans {
            if let Err(e) = self.warm.save_span(span).await {
                tracing::error!(trace_id = %trace_id, error = %e, "failed to save span");
            }
        }

        for (_, events) in trace.span_events {
            if !events.is_empty() {
                if let Err(e) = self.warm.save_span_events(events).await {
                    tracing::error!(trace_id = %trace_id, error = %e, "failed to save span events");
                }
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
}
