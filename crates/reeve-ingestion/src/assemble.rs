use crate::normalize::NormalizedSpan;
use reeve_model::entity::agent::Agent;
use reeve_model::entity::span::InternalSpan;
use reeve_model::entity::span_event::SpanEvent;
use reeve_model::ids::{SpanId, TraceId};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const STRAGGLER_WINDOW: Duration = Duration::from_secs(2);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, PartialEq)]
pub enum CompletionState {
    InFlight,
    /// Root span arrived and the straggler window has elapsed.
    Completed,
    /// No root span arrived within the idle timeout.
    Interrupted,
}

pub struct InFlightTrace {
    pub trace_id: TraceId,
    pub agent: Agent,
    pub spans: HashMap<SpanId, InternalSpan>,
    pub children: HashMap<SpanId, Vec<SpanId>>,
    pub span_events: HashMap<SpanId, Vec<SpanEvent>>,
    pending_attachment: HashMap<SpanId, InternalSpan>,
    pub root_span_id: Option<SpanId>,
    last_updated: Instant,
    completion_timer: Option<Instant>,
    pub cost_accumulator: f64,
}

impl InFlightTrace {
    pub fn new(trace_id: TraceId, agent: Agent) -> Self {
        Self {
            trace_id,
            agent,
            spans: HashMap::new(),
            children: HashMap::new(),
            span_events: HashMap::new(),
            pending_attachment: HashMap::new(),
            root_span_id: None,
            last_updated: Instant::now(),
            completion_timer: None,
            cost_accumulator: 0.0,
        }
    }

    pub fn receive_span(&mut self, span: InternalSpan, events: Vec<SpanEvent>) {
        self.last_updated = Instant::now();
        self.span_events.insert(span.id.clone(), events);

        if let Some(cost) = span
            .attributes
            .get("gen_ai.usage.cost")
            .and_then(|v| v.as_f64())
        {
            self.cost_accumulator += cost;
        }

        let span_id = span.id.clone();
        let parent_id = span.parent_id.clone();

        if let Some(pid) = parent_id {
            if self.spans.contains_key(&pid) {
                self.children.entry(pid).or_default().push(span_id.clone());
                self.spans.insert(span_id, span);
            } else {
                // Parent not yet seen; hold until it arrives.
                self.pending_attachment.insert(span_id, span);
            }
        } else if let Some(existing_root) = &self.root_span_id {
            // A well-formed OTel trace has exactly one root. A second no-parent
            // span means the agent is misbehaving. First root wins; the duplicate
            // is kept in spans so no data is lost, but the completion timer is
            // not reset (a runaway agent could otherwise hold a trace open
            // indefinitely by emitting no-parent spans).
            tracing::warn!(
                trace_id = %self.trace_id,
                span_id = %span_id,
                existing_root = %existing_root,
                "duplicate root candidate ignored; first no-parent span wins",
            );
            self.spans.insert(span_id, span);
        } else {
            // First no-parent span: this is the root; start the straggler window.
            self.root_span_id = Some(span_id.clone());
            self.completion_timer = Some(Instant::now());
            self.spans.insert(span_id, span);
        }

        self.drain_orphans();
    }

    /// Repeatedly adopts pending spans whose parent is now in `spans`.
    /// One pass can unblock another, so we loop until nothing moves.
    fn drain_orphans(&mut self) {
        loop {
            let adoptable: Vec<SpanId> = self
                .pending_attachment
                .keys()
                .filter(|id| {
                    self.pending_attachment[*id]
                        .parent_id
                        .as_ref()
                        .is_some_and(|pid| self.spans.contains_key(pid))
                })
                .cloned()
                .collect();

            if adoptable.is_empty() {
                break;
            }

            for id in adoptable {
                if let Some(span) = self.pending_attachment.remove(&id) {
                    if let Some(pid) = &span.parent_id {
                        self.children
                            .entry(pid.clone())
                            .or_default()
                            .push(id.clone());
                    }
                    self.spans.insert(id, span);
                }
            }
        }
    }

    pub fn check_completion(&self) -> CompletionState {
        if let Some(timer) = self.completion_timer {
            if timer.elapsed() >= STRAGGLER_WINDOW {
                return CompletionState::Completed;
            }
        } else if self.last_updated.elapsed() >= IDLE_TIMEOUT {
            return CompletionState::Interrupted;
        }
        CompletionState::InFlight
    }

    pub fn pending_count(&self) -> usize {
        self.pending_attachment.len()
    }
}

pub struct Assembler {
    traces: HashMap<TraceId, InFlightTrace>,
}

impl Assembler {
    pub fn new() -> Self {
        Self {
            traces: HashMap::new(),
        }
    }

    pub fn receive(&mut self, span: InternalSpan, events: Vec<SpanEvent>, agent: Agent) {
        let trace_id = span.trace_id.clone();
        let trace = self
            .traces
            .entry(trace_id.clone())
            .or_insert_with(|| InFlightTrace::new(trace_id, agent));
        trace.receive_span(span, events);
    }

    /// Returns all traces that have completed or been interrupted, removing
    /// them from the in-flight map.
    pub fn tick(&mut self) -> Vec<(InFlightTrace, CompletionState)> {
        let to_finalize: Vec<TraceId> = self
            .traces
            .keys()
            .filter(|tid| self.traces[*tid].check_completion() != CompletionState::InFlight)
            .cloned()
            .collect();

        let mut finalized = Vec::new();
        for tid in to_finalize {
            if let Some(trace) = self.traces.remove(&tid) {
                let state = trace.check_completion();
                finalized.push((trace, state));
            }
        }
        finalized
    }

    #[cfg(test)]
    pub fn get(&self, trace_id: &str) -> Option<&InFlightTrace> {
        self.traces.get(trace_id)
    }
}

impl Default for Assembler {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn run(
    mut rx: mpsc::Receiver<NormalizedSpan>,
    tick_ms: u64,
    tx: mpsc::Sender<(InFlightTrace, CompletionState)>,
) {
    let mut assembler = Assembler::new();
    let mut interval = tokio::time::interval(Duration::from_millis(tick_ms));

    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Some(ns) => {
                        tracing::debug!(
                            span_id = %ns.span.id,
                            trace_id = %ns.span.trace_id,
                            "assembling span",
                        );
                        assembler.receive(ns.span, ns.events, ns.agent);
                    }
                    None => {
                        tracing::info!("assemble stage shut down");
                        return;
                    }
                }
            }
            _ = interval.tick() => {
                for (trace, state) in assembler.tick() {
                    tracing::debug!(
                        trace_id = %trace.trace_id,
                        agent_id = %trace.agent.id,
                        spans = trace.spans.len(),
                        pending = trace.pending_count(),
                        cost = trace.cost_accumulator,
                        state = ?state,
                        "trace finalized",
                    );
                    if tx.send((trace, state)).await.is_err() {
                        tracing::warn!("route stage receiver dropped, assemble stage shutting down");
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reeve_model::entity::agent::{AgentStatus, IntegrationPath};
    use reeve_model::entity::span::{InternalSpan, SpanStatus};

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

    #[test]
    fn orphan_lands_in_pending_when_parent_absent() {
        let mut trace = InFlightTrace::new("trace-1".into(), make_agent());
        trace.receive_span(make_span("child-1", "trace-1", Some("parent-1")), vec![]);

        assert_eq!(trace.spans.len(), 0, "child should not be in spans yet");
        assert_eq!(
            trace.pending_count(),
            1,
            "child should be waiting in pending_attachment"
        );
    }

    #[test]
    fn orphan_adopted_when_parent_arrives_later() {
        let mut trace = InFlightTrace::new("trace-1".into(), make_agent());

        trace.receive_span(make_span("child-1", "trace-1", Some("root-1")), vec![]);
        assert_eq!(
            trace.pending_count(),
            1,
            "child must be pending before root arrives"
        );

        trace.receive_span(make_span("root-1", "trace-1", None), vec![]);

        assert_eq!(
            trace.pending_count(),
            0,
            "orphan should be adopted after parent arrives"
        );
        assert_eq!(trace.spans.len(), 2);
        assert!(
            trace
                .children
                .get("root-1")
                .map_or(false, |c| c.iter().any(|id| id.as_str() == "child-1")),
            "root-1 must have child-1 in its children list"
        );
    }

    #[test]
    fn root_span_sets_completion_timer() {
        let mut trace = InFlightTrace::new("trace-1".into(), make_agent());
        assert!(
            trace.completion_timer.is_none(),
            "no timer before root arrives"
        );

        trace.receive_span(make_span("root-1", "trace-1", None), vec![]);

        assert!(
            trace.completion_timer.is_some(),
            "timer must be set after root arrives"
        );
        assert_eq!(trace.root_span_id.as_deref(), Some("root-1"));
        assert_eq!(
            trace.check_completion(),
            CompletionState::InFlight,
            "straggler window should not have elapsed yet"
        );
    }

    #[test]
    fn mutual_orphans_stay_in_pending() {
        let mut trace = InFlightTrace::new("trace-1".into(), make_agent());

        // A's parent is B, B's parent is A — circular, neither can ever be adopted
        trace.receive_span(make_span("span-a", "trace-1", Some("span-b")), vec![]);
        trace.receive_span(make_span("span-b", "trace-1", Some("span-a")), vec![]);

        assert_eq!(trace.spans.len(), 0, "neither span can be adopted");
        assert_eq!(
            trace.pending_count(),
            2,
            "both must remain in pending_attachment"
        );
    }

    #[test]
    fn single_span_trace_with_no_parent_is_root() {
        let mut trace = InFlightTrace::new("trace-1".into(), make_agent());
        trace.receive_span(make_span("only-span", "trace-1", None), vec![]);

        assert_eq!(trace.spans.len(), 1);
        assert_eq!(trace.root_span_id.as_deref(), Some("only-span"));
        assert_eq!(trace.pending_count(), 0);
    }

    #[test]
    fn duplicate_root_candidate_does_not_override_first() {
        let mut trace = InFlightTrace::new("trace-1".into(), make_agent());

        trace.receive_span(make_span("root-1", "trace-1", None), vec![]);
        let first_timer = trace.completion_timer.unwrap();

        trace.receive_span(make_span("root-2", "trace-1", None), vec![]);

        assert_eq!(
            trace.root_span_id.as_deref(),
            Some("root-1"),
            "first no-parent span must remain the root"
        );
        assert_eq!(
            trace.completion_timer.unwrap(),
            first_timer,
            "completion timer must not be reset by duplicate root"
        );
        assert_eq!(
            trace.spans.len(),
            2,
            "duplicate root span must still be kept in spans"
        );
    }

    #[test]
    fn cost_accumulates_across_spans() {
        let mut trace = InFlightTrace::new("trace-1".into(), make_agent());

        let mut span1 = make_span("span-1", "trace-1", None);
        span1.attributes = serde_json::json!({"gen_ai.usage.cost": 0.0025});

        let mut span2 = make_span("span-2", "trace-1", Some("span-1"));
        span2.attributes = serde_json::json!({"gen_ai.usage.cost": 0.0010});

        trace.receive_span(span1, vec![]);
        trace.receive_span(span2, vec![]);

        assert!(
            (trace.cost_accumulator - 0.0035).abs() < 1e-9,
            "cost should sum across spans, got {}",
            trace.cost_accumulator
        );
    }
}
