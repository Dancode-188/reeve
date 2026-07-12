use crate::normalize::NormalizedSpan;
use reeve_model::entity::agent::Agent;
use reeve_model::entity::span::InternalSpan;
use reeve_model::entity::span_event::SpanEvent;
use reeve_model::ids::{AgentId, SpanId, TraceId};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Agents currently paused by an intervention command. Written by the
/// intervention layer when a Pause is acknowledged as applied, cleared on
/// Resume, Kill, or control-channel disconnect. The assembler reads it to
/// suspend the idle timeout: a paused agent emits no spans by design, and
/// that silence must not be mistaken for an interrupted trace.
pub type PausedAgents = Arc<Mutex<HashSet<AgentId>>>;

/// Agents whose control stream has dropped, and when. Written by the
/// control server on disconnect and reconnect, read here on every tick:
/// the same shared-state pattern as the paused set. Proxy agents never
/// appear (no control channel) and keep the plain idle timeout.
pub type DisconnectedAgents = Arc<Mutex<std::collections::HashMap<AgentId, Instant>>>;

/// Traces with a response currently streaming (or a Messages round trip
/// otherwise in flight) through the proxy, with a count of concurrent
/// requests. Written by the proxy, read by the assembler's tick: a trace
/// is not idle while tokens are flowing, no matter how long the model
/// takes. Same shared-map pattern as the paused set.
pub type ActiveStreams = Arc<Mutex<std::collections::HashMap<TraceId, usize>>>;

/// Traces whose conversation turn is still open (a tool_use awaits its
/// tool_result), with the moment the conversation last sent a request.
/// The gap BETWEEN round trips is when the client runs its tools; a
/// build can take minutes with no request in flight, and that silence
/// is expected continuation, not death. Written by the proxy, read by
/// the assembler's tick.
pub type OpenTurns = Arc<Mutex<std::collections::HashMap<TraceId, Instant>>>;

/// How long an open turn holds the idle timeout after its conversation
/// last sent a request. A client that died mid-turn stops sending, so
/// its trace flushes one idle window after this bound instead of
/// lingering forever.
const OPEN_TURN_RECENCY: Duration = Duration::from_secs(5 * 60);

/// How long a disconnected agent's traces survive before flushing as
/// Interrupted and resumable. Generous relative to the idle timeout
/// because a dropped connection is diagnosable, unlike plain silence.
const DISCONNECT_GRACE: Duration = Duration::from_secs(60);

/// Total approximate bytes of in-flight trace data the assembler holds
/// before it starts evicting the stalest traces. Bounds Reeve's memory
/// against an agent that emits spans without ever completing a trace.
const IN_FLIGHT_CEILING_BYTES: usize = 50 * 1024 * 1024;

const STRAGGLER_WINDOW: Duration = Duration::from_secs(2);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, PartialEq)]
pub enum CompletionState {
    InFlight,
    /// Root span arrived and the straggler window has elapsed.
    Completed,
    /// No root span arrived within the idle timeout. The agent was
    /// connected and simply went quiet, so nothing more is coming and
    /// the trace is not resumable.
    Interrupted,
    /// The agent's connection dropped and the grace period expired.
    /// Flushed as resumable: the agent may return and continue.
    InterruptedResumable,
}

pub struct InFlightTrace {
    pub trace_id: TraceId,
    pub agent: Agent,
    pub spans: HashMap<SpanId, InternalSpan>,
    pub children: HashMap<SpanId, Vec<SpanId>>,
    pub span_events: HashMap<SpanId, Vec<SpanEvent>>,
    /// Spans whose parent has not arrived yet. On the threading path the
    /// parent is the turn root, which is emitted LAST, so during a live
    /// turn everything waits here. Every flush path must persist these:
    /// dropping them cost a real session ~30 round trips of spans (#182).
    pub pending_attachment: HashMap<SpanId, InternalSpan>,
    pub root_span_id: Option<SpanId>,
    last_updated: Instant,
    completion_timer: Option<Instant>,
    pub cost_accumulator: f64,
    /// Approximate memory footprint of this trace's spans and events,
    /// maintained incrementally as they arrive.
    pub approx_bytes: usize,
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
            approx_bytes: 0,
        }
    }

    pub fn receive_span(&mut self, span: InternalSpan, events: Vec<SpanEvent>) {
        self.last_updated = Instant::now();
        // Attribute JSON plus event content dominates a span's footprint;
        // the fixed struct overhead is approximated by a flat constant.
        self.approx_bytes += span.attributes.to_string().len()
            + span.operation.len()
            + events
                .iter()
                .map(|e| e.content.as_deref().map_or(0, str::len))
                .sum::<usize>()
            + 256;
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
    paused: PausedAgents,
    disconnected: DisconnectedAgents,
    active_streams: ActiveStreams,
    open_turns: OpenTurns,
    ceiling_bytes: usize,
}

impl Assembler {
    pub fn new(
        paused: PausedAgents,
        disconnected: DisconnectedAgents,
        active_streams: ActiveStreams,
        open_turns: OpenTurns,
    ) -> Self {
        Self {
            traces: HashMap::new(),
            paused,
            disconnected,
            active_streams,
            open_turns,
            ceiling_bytes: IN_FLIGHT_CEILING_BYTES,
        }
    }

    /// Rebuilds an in-flight trace from stored spans, for resuming after
    /// a restart. The idle clock starts fresh, giving the returning
    /// agent a full window to continue.
    pub fn reload(&mut self, spans: Vec<(InternalSpan, Vec<SpanEvent>)>, agent: Agent) {
        for (span, events) in spans {
            let _ = self.receive(span, events, agent.clone());
        }
    }

    pub fn receive(
        &mut self,
        span: InternalSpan,
        events: Vec<SpanEvent>,
        agent: Agent,
    ) -> Vec<(InFlightTrace, CompletionState)> {
        let trace_id = span.trace_id.clone();
        let trace = self
            .traces
            .entry(trace_id.clone())
            .or_insert_with(|| InFlightTrace::new(trace_id.clone(), agent));
        trace.receive_span(span, events);
        self.enforce_ceiling(&trace_id)
    }

    /// Evicts the least recently updated traces while the total footprint
    /// exceeds the ceiling. Staleness, not size, picks the victim: the
    /// trace nobody has written to is the one most likely abandoned, and
    /// evicting the largest would punish exactly the long-running agent
    /// the ceiling exists to protect. The just-updated trace is exempt so
    /// a single oversized trace cannot evict itself into a loop.
    fn enforce_ceiling(&mut self, just_updated: &TraceId) -> Vec<(InFlightTrace, CompletionState)> {
        let mut evicted = Vec::new();
        loop {
            let total: usize = self.traces.values().map(|t| t.approx_bytes).sum();
            if total <= self.ceiling_bytes {
                break;
            }
            let stalest = self
                .traces
                .iter()
                .filter(|(id, _)| *id != just_updated)
                .min_by_key(|(_, t)| t.last_updated)
                .map(|(id, _)| id.clone());
            let Some(id) = stalest else { break };
            if let Some(trace) = self.traces.remove(&id) {
                tracing::warn!(
                    trace_id = %trace.trace_id,
                    approx_bytes = trace.approx_bytes,
                    "in-flight ceiling exceeded; evicting stalest trace as interrupted"
                );
                evicted.push((trace, CompletionState::Interrupted));
            }
        }
        evicted
    }

    /// Returns all traces that have completed or been interrupted, removing
    /// them from the in-flight map.
    pub fn tick(&mut self) -> Vec<(InFlightTrace, CompletionState)> {
        // A paused agent's silence is intentional. Refreshing last_updated
        // while the pause holds keeps the idle timeout from firing, and gives
        // the agent a full idle window after Resume before its next span is
        // due. The straggler-window path is untouched: a trace whose root
        // already arrived is complete regardless of pause state.
        {
            let paused = self.paused.lock().unwrap();
            if !paused.is_empty() {
                for trace in self.traces.values_mut() {
                    if paused.contains(&trace.agent.id) {
                        trace.last_updated = Instant::now();
                    }
                }
            }
        }

        // A response actively streaming through the proxy is the opposite
        // of idleness, however long the model takes: refresh those traces
        // the same way pause does. A real 8-minute Claude Code turn hit
        // the idle timeout repeatedly mid-turn without this (#182).
        {
            let streaming = self.active_streams.lock().unwrap();
            if !streaming.is_empty() {
                for trace in self.traces.values_mut() {
                    if streaming.contains_key(&trace.trace_id) {
                        trace.last_updated = Instant::now();
                    }
                }
            }
        }

        // An open turn between round trips is the client running its
        // tools: a build can take minutes with no request in flight, and
        // that silence is expected continuation. Hold the idle timeout
        // while the turn is open and the conversation was seen recently;
        // a client that died mid-turn stops sending, its recency lapses,
        // and the plain timeout resumes (#200).
        {
            let open = self.open_turns.lock().unwrap();
            if !open.is_empty() {
                for trace in self.traces.values_mut() {
                    if open
                        .get(&trace.trace_id)
                        .is_some_and(|seen| seen.elapsed() < OPEN_TURN_RECENCY)
                    {
                        trace.last_updated = Instant::now();
                    }
                }
            }
        }

        // A dropped connection is not silence: while the grace period
        // runs, the idle timeout is held off (same mechanism as pause);
        // when it expires, the flush below picks the trace up as
        // resumable. Reconnection removes the map entry and the idle
        // clock starts fresh from the refreshes done here.
        let grace_expired: HashSet<AgentId> = {
            let disconnected = self.disconnected.lock().unwrap();
            for trace in self.traces.values_mut() {
                if let Some(since) = disconnected.get(&trace.agent.id) {
                    if since.elapsed() < DISCONNECT_GRACE {
                        trace.last_updated = Instant::now();
                    }
                }
            }
            disconnected
                .iter()
                .filter(|(_, since)| since.elapsed() >= DISCONNECT_GRACE)
                .map(|(id, _)| id.clone())
                .collect()
        };

        let to_finalize: Vec<TraceId> = self
            .traces
            .keys()
            .filter(|tid| {
                grace_expired.contains(&self.traces[*tid].agent.id)
                    || self.traces[*tid].check_completion() != CompletionState::InFlight
            })
            .cloned()
            .collect();

        let mut finalized = Vec::new();
        for tid in to_finalize {
            if let Some(trace) = self.traces.remove(&tid) {
                let state = if grace_expired.contains(&trace.agent.id)
                    && trace.check_completion() != CompletionState::Completed
                {
                    CompletionState::InterruptedResumable
                } else {
                    trace.check_completion()
                };
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
        Self::new(
            Arc::new(Mutex::new(HashSet::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
        )
    }
}

pub async fn run(
    mut rx: mpsc::Receiver<NormalizedSpan>,
    tick_ms: u64,
    tx: mpsc::Sender<(InFlightTrace, CompletionState)>,
    paused: PausedAgents,
    disconnected: DisconnectedAgents,
    active_streams: ActiveStreams,
    open_turns: OpenTurns,
) {
    let mut assembler = Assembler::new(paused, disconnected, active_streams, open_turns);
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
                        for evicted in assembler.receive(ns.span, ns.events, ns.agent) {
                            if tx.send(evicted).await.is_err() {
                                tracing::warn!("route stage unavailable, evicted trace dropped");
                            }
                        }
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
                .is_some_and(|c| c.iter().any(|id| id.as_str() == "child-1")),
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

    /// Backdates a trace's last activity so the idle timeout is already due.
    fn make_idle(assembler: &mut Assembler, trace_id: &str) {
        let trace = assembler.traces.get_mut(trace_id).unwrap();
        trace.last_updated = Instant::now() - IDLE_TIMEOUT - Duration::from_secs(1);
    }

    #[test]
    fn idle_trace_without_root_is_interrupted() {
        let mut assembler = Assembler::default();
        assembler.receive(
            make_span("child-1", "trace-1", Some("never-arrives")),
            vec![],
            make_agent(),
        );
        make_idle(&mut assembler, "trace-1");

        let finalized = assembler.tick();
        assert_eq!(finalized.len(), 1);
        assert_eq!(finalized[0].1, CompletionState::Interrupted);
    }

    #[test]
    fn an_open_turn_survives_the_idle_timeout_until_recency_lapses() {
        // The #200 shape: a build runs client-side for minutes, so no
        // request is in flight and no span arrives, but the turn is
        // open (a tool_use awaits its result). Recently-seen open turns
        // hold the timeout; a conversation that stops sending entirely
        // (client died mid-turn) lapses and flushes.
        let open: OpenTurns = Arc::new(Mutex::new(std::collections::HashMap::new()));
        open.lock()
            .unwrap()
            .insert("trace-1".into(), Instant::now());

        let mut assembler = Assembler::new(
            Arc::new(Mutex::new(HashSet::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            open.clone(),
        );
        assembler.receive(
            make_span("chat-1", "trace-1", Some("unarrived-root")),
            vec![],
            make_agent(),
        );
        make_idle(&mut assembler, "trace-1");
        assert!(
            assembler.tick().is_empty(),
            "an open turn with a recent request is alive, however long the tool runs"
        );

        // The client died mid-turn: no request ever comes again, so the
        // recency bound lapses and the plain timeout resumes.
        open.lock().unwrap().insert(
            "trace-1".into(),
            Instant::now() - OPEN_TURN_RECENCY - Duration::from_secs(1),
        );
        make_idle(&mut assembler, "trace-1");
        let finalized = assembler.tick();
        assert_eq!(finalized.len(), 1);
        assert_eq!(finalized[0].1, CompletionState::Interrupted);
    }

    #[test]
    fn actively_streaming_trace_survives_idle_timeout() {
        // A model can stream a single response for minutes while the
        // trace's spans only arrive at stream end: the proxy marks the
        // round trip in flight, and that must hold the idle timeout the
        // same way pause does (#182).
        let active: ActiveStreams = Arc::new(Mutex::new(std::collections::HashMap::new()));
        active.lock().unwrap().insert("trace-1".into(), 1);

        let mut assembler = Assembler::new(
            Arc::new(Mutex::new(HashSet::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            active.clone(),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
        );
        assembler.receive(
            make_span("chat-1", "trace-1", Some("unarrived-root")),
            vec![],
            make_agent(),
        );
        make_idle(&mut assembler, "trace-1");
        assert!(
            assembler.tick().is_empty(),
            "a streaming trace is not idle, however long the model takes"
        );

        // Stream ends, the exemption ends: a full idle window later the
        // plain timeout applies again.
        active.lock().unwrap().remove(&TraceId::from("trace-1"));
        make_idle(&mut assembler, "trace-1");
        let finalized = assembler.tick();
        assert_eq!(finalized.len(), 1);
        assert_eq!(finalized[0].1, CompletionState::Interrupted);
    }

    #[test]
    fn paused_agent_trace_survives_idle_timeout() {
        let paused: PausedAgents = Arc::new(Mutex::new(HashSet::new()));
        paused.lock().unwrap().insert("agent-1".into());

        let mut assembler = Assembler::new(
            paused,
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
        );
        assembler.receive(
            make_span("child-1", "trace-1", Some("never-arrives")),
            vec![],
            make_agent(),
        );
        make_idle(&mut assembler, "trace-1");

        assert!(
            assembler.tick().is_empty(),
            "paused agent's trace must not be finalized by the idle timeout"
        );
    }

    #[test]
    fn resumed_agent_gets_full_idle_window() {
        let paused: PausedAgents = Arc::new(Mutex::new(HashSet::new()));
        paused.lock().unwrap().insert("agent-1".into());

        let mut assembler = Assembler::new(
            paused.clone(),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
        );
        assembler.receive(
            make_span("child-1", "trace-1", Some("never-arrives")),
            vec![],
            make_agent(),
        );
        make_idle(&mut assembler, "trace-1");

        // While paused, the tick refreshes last_updated.
        assert!(assembler.tick().is_empty());

        // Immediately after resume the trace must still be in flight: the
        // refresh during the pause restarted the idle window.
        paused.lock().unwrap().clear();
        assert!(
            assembler.tick().is_empty(),
            "resumed agent must get a fresh idle window, not an instant interrupt"
        );

        // Once the fresh window elapses with no spans, interruption is correct.
        make_idle(&mut assembler, "trace-1");
        let finalized = assembler.tick();
        assert_eq!(finalized.len(), 1);
        assert_eq!(finalized[0].1, CompletionState::Interrupted);
    }

    #[test]
    fn pause_does_not_hold_open_a_trace_whose_root_arrived() {
        let paused: PausedAgents = Arc::new(Mutex::new(HashSet::new()));
        paused.lock().unwrap().insert("agent-1".into());

        let mut assembler = Assembler::new(
            paused,
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
        );
        assembler.receive(make_span("root-1", "trace-1", None), vec![], make_agent());

        // Backdate the straggler window so the trace is due for completion.
        let trace = assembler.traces.get_mut("trace-1").unwrap();
        trace.completion_timer = Some(Instant::now() - STRAGGLER_WINDOW - Duration::from_secs(1));

        let finalized = assembler.tick();
        assert_eq!(finalized.len(), 1);
        assert_eq!(
            finalized[0].1,
            CompletionState::Completed,
            "a trace whose root arrived completes regardless of pause state"
        );
    }

    #[test]
    fn disconnect_grace_holds_the_idle_timeout() {
        let disconnected: DisconnectedAgents =
            Arc::new(Mutex::new(std::collections::HashMap::new()));
        let mut assembler = Assembler::new(
            Arc::new(Mutex::new(HashSet::new())),
            disconnected.clone(),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
        );
        assembler.receive(
            make_span("child", "t1", Some("missing")),
            vec![],
            make_agent(),
        );

        // Disconnected just now: within grace, the trace must stay in
        // flight even if the idle clock would otherwise have expired.
        disconnected
            .lock()
            .unwrap()
            .insert("agent-1".into(), Instant::now());
        if let Some(trace) = assembler.traces.get_mut(&TraceId::from("t1")) {
            trace.last_updated = Instant::now() - IDLE_TIMEOUT - Duration::from_secs(1);
        }
        assert!(
            assembler.tick().is_empty(),
            "grace period must hold off the idle timeout"
        );
    }

    #[test]
    fn expired_grace_flushes_as_resumable() {
        let disconnected: DisconnectedAgents =
            Arc::new(Mutex::new(std::collections::HashMap::new()));
        let mut assembler = Assembler::new(
            Arc::new(Mutex::new(HashSet::new())),
            disconnected.clone(),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
        );
        assembler.receive(
            make_span("child", "t1", Some("missing")),
            vec![],
            make_agent(),
        );

        disconnected.lock().unwrap().insert(
            "agent-1".into(),
            Instant::now() - DISCONNECT_GRACE - Duration::from_secs(1),
        );
        let finalized = assembler.tick();
        assert_eq!(finalized.len(), 1);
        assert_eq!(
            finalized[0].1,
            CompletionState::InterruptedResumable,
            "grace expiry flushes the trace as resumable"
        );
    }

    #[test]
    fn reconnect_within_grace_resumes_the_idle_clock() {
        let disconnected: DisconnectedAgents =
            Arc::new(Mutex::new(std::collections::HashMap::new()));
        let mut assembler = Assembler::new(
            Arc::new(Mutex::new(HashSet::new())),
            disconnected.clone(),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
        );
        assembler.receive(
            make_span("child", "t1", Some("missing")),
            vec![],
            make_agent(),
        );

        disconnected
            .lock()
            .unwrap()
            .insert("agent-1".into(), Instant::now());
        assert!(assembler.tick().is_empty());

        // The control server clears the entry on reconnect; the refresh
        // done during grace means the idle clock starts fresh from here.
        disconnected.lock().unwrap().clear();
        assert!(
            assembler.tick().is_empty(),
            "a reconnected agent's trace stays in flight"
        );
    }

    #[test]
    fn completed_trace_is_never_downgraded_by_grace_expiry() {
        let disconnected: DisconnectedAgents =
            Arc::new(Mutex::new(std::collections::HashMap::new()));
        let mut assembler = Assembler::new(
            Arc::new(Mutex::new(HashSet::new())),
            disconnected.clone(),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
        );
        // Root arrived: the straggler window governs, not the grace.
        assembler.receive(make_span("root", "t1", None), vec![], make_agent());
        if let Some(trace) = assembler.traces.get_mut(&TraceId::from("t1")) {
            trace.completion_timer = Some(Instant::now() - STRAGGLER_WINDOW);
        }
        disconnected.lock().unwrap().insert(
            "agent-1".into(),
            Instant::now() - DISCONNECT_GRACE - Duration::from_secs(1),
        );
        let finalized = assembler.tick();
        assert_eq!(finalized.len(), 1);
        assert_eq!(
            finalized[0].1,
            CompletionState::Completed,
            "a root that arrived before disconnect still completes normally"
        );
    }

    #[test]
    fn ceiling_evicts_the_stalest_trace_first() {
        let mut assembler = Assembler::new(
            Arc::new(Mutex::new(HashSet::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
        );
        assembler.ceiling_bytes = 2_000;

        // Two traces; t1 goes stale, t2 keeps receiving.
        let _ = assembler.receive(make_span("a", "t1", Some("m")), vec![], make_agent());
        if let Some(t) = assembler.traces.get_mut(&TraceId::from("t1")) {
            t.last_updated = Instant::now() - Duration::from_secs(10);
        }
        let _ = assembler.receive(make_span("b", "t2", Some("m")), vec![], make_agent());

        // Push t2 over the ceiling: the STALE t1 is evicted, not t2.
        let mut big = make_span("c", "t2", Some("m"));
        big.attributes = serde_json::json!({"filler": "x".repeat(2_000)});
        let evicted = assembler.receive(big, vec![], make_agent());
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].0.trace_id, TraceId::from("t1"));
        assert_eq!(evicted[0].1, CompletionState::Interrupted);
        assert!(
            assembler.traces.contains_key(&TraceId::from("t2")),
            "the actively written trace survives"
        );
    }

    #[test]
    fn a_single_oversized_trace_is_never_self_evicted() {
        let mut assembler = Assembler::new(
            Arc::new(Mutex::new(HashSet::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
        );
        assembler.ceiling_bytes = 500;
        let mut big = make_span("a", "t1", Some("m"));
        big.attributes = serde_json::json!({"filler": "x".repeat(5_000)});
        let evicted = assembler.receive(big, vec![], make_agent());
        assert!(
            evicted.is_empty(),
            "the only trace, however large, keeps flowing"
        );
        assert!(assembler.traces.contains_key(&TraceId::from("t1")));
    }
}
