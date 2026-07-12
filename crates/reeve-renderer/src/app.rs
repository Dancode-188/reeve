use crate::impact::ImpactState;
use crate::input::Action;
use crate::mouse::MouseTarget;
use crate::replay::{ReplayEvent, ReplayState};
use indexmap::IndexMap;
use reeve_intervention::dispatcher::Dispatcher;
use reeve_model::entity::agent::Agent;
use reeve_model::entity::intervention::{CommandStatus, CommandType, InterventionCommand};
use reeve_model::entity::span::InternalSpan;
use reeve_model::entity::trace::Trace;
use reeve_model::ids::{AgentId, CommandId, SpanId, TraceId};
use reeve_model::signal::{CostTrend, EngineEvent, EvaluationConfidence, IngestionEvent};
use reeve_storage::warm::{CostSummary, WarmStore};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

pub struct MetricScore {
    pub name: String,
    pub score: f64,
    pub confidence: Option<EvaluationConfidence>,
}

pub struct PolicyAlertEntry {
    pub description: String,
    pub command_type: String,
    /// Preformatted effectiveness note, e.g. "redirect: +0.42 avg · 5 tries".
    /// None until enough measured outcomes exist for the firing rule.
    pub effectiveness: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FlashTarget {
    HealthGauge,
    CostTotal,
    AlertSection,
    AgentRow(AgentId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashDirection {
    Positive,
    Negative,
    Neutral,
    Alert,
}

#[derive(Clone)]
pub struct AgentState {
    pub agent: Agent,
    pub last_trace_id: Option<TraceId>,
    pub trace_count: u32,
    pub total_cost: f64,
    /// Running estimate for an in-flight streamed response, ticking the
    /// header during generation. Folded into `total_cost` (from real
    /// usage) when the trace completes, so this resets to zero there.
    pub live_cost: f64,
    pub cost_history: Vec<f64>,
    pub cost_trend: Option<CostTrend>,
}

impl AgentState {
    /// The one number every cost display shows: settled cost plus the
    /// live in-flight estimate. Two renderings of the same agent's cost
    /// disagreeing mid-generation reads as a bug, whatever the
    /// internals say, so nothing renders `total_cost` directly.
    pub fn display_cost(&self) -> f64 {
        self.total_cost + self.live_cost
    }

    fn new(agent: Agent) -> Self {
        Self {
            agent,
            last_trace_id: None,
            trace_count: 0,
            total_cost: 0.0,
            live_cost: 0.0,
            cost_history: Vec::new(),
            cost_trend: None,
        }
    }
}

pub struct TraceView {
    pub trace_id: TraceId,
    pub root: Option<SpanId>,
    pub spans: HashMap<SpanId, InternalSpan>,
    pub children: HashMap<SpanId, Vec<SpanId>>,
    pub names: HashMap<SpanId, String>,
    pub span_order: Vec<SpanId>,
    pub scroll: u16,
    pub selected: Option<SpanId>,
    pub collapsed: HashSet<SpanId>,
    /// Per-span composite health scores. Populated when HealthScoreUpdated fires
    /// for the trace. Only gen_ai.* operation spans receive a badge.
    pub span_health_scores: HashMap<SpanId, f64>,
    /// Pre-formatted outcome annotations loaded from the warm store on trace load.
    pub outcome_lines: Vec<OutcomeLine>,
    /// Spans not reachable from the root because their parent has not
    /// arrived yet. Rendered as flat rows awaiting the parent. Populated
    /// during replay, where arrival order routinely puts children first.
    pub orphans: Vec<SpanId>,
    /// Developer annotations keyed by span: the tree shows an indicator and
    /// the right panel shows the content in the NOTE box.
    pub notes: HashMap<SpanId, String>,
}

/// Whether a trace that just completed should replace what the panel
/// is showing. A single-round-trip stub (one chat plus its root) is the
/// side-call shape: it never steals the panel from a larger loaded
/// tree. It may replace another stub, and anything loads into an empty
/// panel.
pub fn completed_trace_takes_panel(completed_spans: usize, loaded_spans: Option<usize>) -> bool {
    completed_spans > 2 || loaded_spans.is_none_or(|n| n <= 2)
}

/// One open turn's live view and its display ordering.
pub struct LiveTurn {
    pub agent_id: AgentId,
    /// Arrival order; the lowest sequence among an agent's open turns
    /// is the one the panel shows.
    pub seq: u64,
    pub view: TraceView,
}

/// Open turns keyed by trace. Keyed by trace and not by agent because
/// one agent runs concurrent conversations (side calls): an agent-keyed
/// slot let every side call destroy the main turn's view. Per agent the
/// panel shows the OLDEST open turn, mirroring the streaming box rule.
#[derive(Default)]
pub struct LiveTurns {
    entries: Vec<LiveTurn>,
    seq: u64,
}

impl LiveTurns {
    /// Folds an observed span into its turn's view, opening one for a
    /// trace not seen before. Bounded: over 32 open turns, the oldest
    /// is evicted, so traces that never complete cannot leak forever.
    pub fn observe(
        &mut self,
        agent_id: AgentId,
        trace_id: TraceId,
        span_id: SpanId,
        parent_span_id: Option<SpanId>,
        operation: String,
    ) {
        let view = match self
            .entries
            .iter_mut()
            .find(|t| t.view.trace_id == trace_id)
        {
            Some(t) => &mut t.view,
            None => {
                if self.entries.len() >= 32 {
                    if let Some(pos) = self
                        .entries
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, t)| t.seq)
                        .map(|(i, _)| i)
                    {
                        self.entries.remove(pos);
                    }
                }
                let seq = self.seq;
                self.seq += 1;
                self.entries.push(LiveTurn {
                    agent_id,
                    seq,
                    view: TraceView::empty(trace_id.clone()),
                });
                &mut self.entries.last_mut().expect("just pushed").view
            }
        };
        view.observe(span_id, parent_span_id, operation);
    }

    /// The turn is over: its view retires.
    pub fn retire(&mut self, trace_id: &TraceId) {
        self.entries.retain(|t| &t.view.trace_id != trace_id);
    }

    /// How many turns this agent has open right now.
    pub fn count_for(&self, agent_id: &AgentId) -> usize {
        self.entries
            .iter()
            .filter(|t| &t.agent_id == agent_id)
            .count()
    }

    pub fn oldest_for(&self, agent_id: &AgentId) -> Option<&TraceView> {
        self.entries
            .iter()
            .filter(|t| &t.agent_id == agent_id)
            .min_by_key(|t| t.seq)
            .map(|t| &t.view)
    }

    pub fn oldest_for_mut(&mut self, agent_id: &AgentId) -> Option<&mut TraceView> {
        self.entries
            .iter_mut()
            .filter(|t| &t.agent_id == agent_id)
            .min_by_key(|t| t.seq)
            .map(|t| &mut t.view)
    }
}

impl TraceView {
    /// An empty view for a turn that just opened; spans fold in as they
    /// are observed.
    pub fn empty(trace_id: TraceId) -> Self {
        Self {
            trace_id,
            root: None,
            spans: HashMap::new(),
            children: HashMap::new(),
            names: HashMap::new(),
            span_order: Vec::new(),
            scroll: 0,
            selected: None,
            collapsed: HashSet::new(),
            span_health_scores: HashMap::new(),
            outcome_lines: Vec::new(),
            orphans: Vec::new(),
            notes: HashMap::new(),
        }
    }

    /// Folds one observed span into a live view. Spans whose parent has
    /// not arrived become top-level rows (the parent is usually the turn
    /// root, which only arrives when the turn ends); a parentless span
    /// IS that root.
    pub fn observe(&mut self, span_id: SpanId, parent_span_id: Option<SpanId>, operation: String) {
        self.names.insert(span_id.clone(), operation);
        if let Some(pid) = parent_span_id {
            if self.names.contains_key(&pid) {
                self.children.entry(pid).or_default().push(span_id.clone());
            } else {
                self.orphans.push(span_id.clone());
            }
        } else {
            self.root = Some(span_id.clone());
        }
        self.span_order.push(span_id);
    }
}

/// One in-flight generation. The box shows the oldest active stream of
/// the selected agent, so a quick side call can never displace the main
/// generation the operator is watching.
pub struct ActiveStream {
    pub span_id: SpanId,
    pub agent_id: AgentId,
    pub content: String,
    /// Arrival order; the lowest sequence among an agent's active
    /// streams is the one being displayed.
    pub seq: u64,
}

pub struct StreamingState {
    /// Replay's display text. Live sessions derive their text from
    /// `active` and `last_finished` instead; see streaming_display.
    pub content: String,
    pub scroll: u16,
    pub auto_scroll: bool,
    pub cursor_tick: u8,
    pub active: Vec<ActiveStream>,
    /// Per agent, the text of the last displayed stream that finished,
    /// so the box holds the final response between turns.
    pub last_finished: HashMap<AgentId, String>,
    pub next_seq: u64,
}

impl Default for StreamingState {
    fn default() -> Self {
        Self {
            content: String::new(),
            scroll: 0,
            auto_scroll: true,
            cursor_tick: 0,
            active: Vec::new(),
            last_finished: HashMap::new(),
            next_seq: 0,
        }
    }
}

impl StreamingState {
    /// Folds a streaming tick into its span's active stream.
    pub fn update(&mut self, span_id: SpanId, agent_id: AgentId, content: String) {
        match self.active.iter_mut().find(|s| s.span_id == span_id) {
            Some(s) => s.content = content,
            None => {
                let seq = self.next_seq;
                self.next_seq += 1;
                self.active.push(ActiveStream {
                    span_id,
                    agent_id,
                    content,
                    seq,
                });
            }
        }
    }

    /// Retires a finished span's stream. Its text becomes the agent's
    /// between-turns display only if it was the one being shown;
    /// otherwise a side call finishing would overwrite the main
    /// response the operator was reading.
    pub fn finish(&mut self, span_id: &SpanId) {
        let Some(pos) = self.active.iter().position(|s| &s.span_id == span_id) else {
            return;
        };
        let removed = self.active.remove(pos);
        let was_displayed = !self
            .active
            .iter()
            .any(|s| s.agent_id == removed.agent_id && s.seq < removed.seq);
        if was_displayed {
            self.last_finished.insert(removed.agent_id, removed.content);
        }
    }

    /// The text an agent's box shows: its oldest active stream, else the
    /// last displayed stream that finished.
    pub fn display_for(&self, agent_id: &AgentId) -> &str {
        self.active
            .iter()
            .filter(|s| &s.agent_id == agent_id)
            .min_by_key(|s| s.seq)
            .map(|s| s.content.as_str())
            .unwrap_or_else(|| {
                self.last_finished
                    .get(agent_id)
                    .map(String::as_str)
                    .unwrap_or("")
            })
    }
}

#[derive(Default, Clone, Copy, PartialEq)]
pub enum PanelFocus {
    #[default]
    Left,
    Center,
    Right,
}

/// What the cockpit's screen currently means. Fleet is the live three-panel
/// default. Focus trades the agent list for a compact trace-history strip on
/// one agent. History and Cost are reserved variants; their keys stay inert
/// until those views exist, because switching to a blank mode is worse than
/// not switching.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    #[default]
    Fleet,
    Focus,
    History,
    Cost,
}

pub struct FatalError {
    pub message: String,
    pub hint: Option<String>,
}

/// What the overlay is currently waiting for.
#[derive(Debug, Clone, PartialEq)]
pub enum OverlayMode {
    Menu,
    TextInput {
        command: OverlayCommand,
        buffer: String,
    },
    KillConfirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayCommand {
    Pause,
    Redirect,
    InjectContext,
    Kill,
}

pub struct InterventionOverlayState {
    pub agent_id: AgentId,
    pub mode: OverlayMode,
}

pub struct SuggestedIntervention {
    pub command: OverlayCommand,
    pub text: String,
}

/// Pre-formatted outcome annotation for the trace tree.
pub struct OutcomeLine {
    /// The span beneath which the line should appear, or None for root.
    pub span_id: Option<SpanId>,
    /// Formatted text, e.g. "redirect +0.58 quality · 4 spans".
    pub text: String,
}

pub struct PendingConfirmation {
    pub agent_id: AgentId,
    pub rule_id: String,
    pub description: String,
    pub command_type: String,
    pub auto_confirm_after_secs: Option<u64>,
    pub arrived_at_ms: i64,
    /// Whether the target agent's integration can actually apply the
    /// suggested command. When false, confirm opens the intervention
    /// overlay instead of dispatching a guaranteed dead letter, and no
    /// auto-confirm countdown runs.
    pub supported: bool,
}

/// Whether a policy-suggested command can be dispatched to an agent
/// with these capabilities. Resume rides the pause capability.
pub fn suggestion_supported(caps: &[String], command_type: &str) -> bool {
    let needed = if command_type == "resume" {
        "pause"
    } else {
        command_type
    };
    caps.iter().any(|c| c == needed)
}

pub struct InterventionTemplate {
    pub key: char,
    pub label: &'static str,
    pub text: &'static str,
}

pub const TEMPLATES: &[InterventionTemplate] = &[
    InterventionTemplate {
        key: '1',
        label: "Summarize progress, then stop",
        text: "Please summarize what you have accomplished so far and stop.",
    },
    InterventionTemplate {
        key: '2',
        label: "Refocus on original task",
        text: "You seem off-track. Refocus on your original task.",
    },
];

pub struct AppState {
    pub agents: IndexMap<AgentId, AgentState>,
    pub selected_agent: Option<usize>,
    pub trace: Option<TraceView>,
    /// Open turns built from SpanObserved events as spans enter the
    /// pipeline; the trace panel prefers these over the loaded
    /// completed trace, and TraceCompleted retires entries so the
    /// loaded trace takes over.
    pub live_turns: LiveTurns,
    pub streaming: StreamingState,
    pub health_score: Option<f64>,
    pub prev_health_score: Option<f64>,
    pub health_weight_coverage: Option<f64>,
    pub health_tier2_pending: bool,
    pub metric_scores: Vec<MetricScore>,
    pub policy_alerts: VecDeque<PolicyAlertEntry>,
    /// Human-readable evaluation backend description, e.g. "local (phi4-mini)"
    /// or "disabled". Set once on engine startup.
    pub eval_backend: Option<String>,
    /// Reason the evaluation backend is disabled, if applicable.
    pub eval_backend_reason: Option<String>,
    /// Active privacy tier from engine startup event. 1 = default (no content
    /// capture); 2+ = content capture enabled. Controls mini-metric row states.
    pub privacy_tier: u8,
    pub flash_targets: HashMap<FlashTarget, (FlashDirection, u8)>,
    pub panel_focus: PanelFocus,
    pub view_mode: ViewMode,
    /// The selected agent's recent traces, newest first, loaded from the
    /// warm store when Focus view opens.
    pub focus_traces: Vec<Trace>,
    pub focus_selected: usize,
    /// Completed traces with their total cost, newest first, loaded from
    /// the warm store when History view opens.
    pub history_entries: Vec<(Trace, f64)>,
    pub history_selected: usize,
    /// True while the selected history row is asking y/n before deletion.
    pub history_confirm_delete: bool,
    /// DVR state while replaying a trace. Some = replay owns the keyboard
    /// and the footer becomes a scrubber.
    pub replay: Option<ReplayState>,
    /// Intervention impact charts. Some = the center panel shows the
    /// before/projected/after comparison for one command.
    pub impact: Option<ImpactState>,
    /// Spending analytics, loaded from the warm store when Cost view opens.
    pub cost_summary: CostSummary,
    /// Command palette input buffer. Some = the palette row is open above
    /// the footer and owns the keyboard.
    pub palette: Option<String>,
    /// Index into the palette's current completion matches.
    pub palette_match: usize,
    /// True while `kill all` waits for its y/n confirmation.
    pub palette_confirm_kill: bool,
    /// Open note input: the span being annotated and the buffer so far.
    pub note_input: Option<(SpanId, String)>,
    /// Filter bar: Some means the input row is open below the TRACE header.
    /// The string filters tree rows by operation/tool name substring.
    pub filter_input: Option<String>,
    /// A filter kept applied after the bar closed with Enter.
    pub filter_applied: Option<String>,
    /// Theme name the palette or T selected; the render loop applies it.
    pub pending_theme: Option<String>,
    /// Agents whose control stream has dropped: the fleet row carries an
    /// offline marker until they reconnect. Proxy agents never appear
    /// here (no control channel).
    pub control_disconnected: HashSet<AgentId>,
    /// Agents whose circuit breaker is engaged, mirrored from the
    /// dispatcher each tick (same pattern as pause). Drives the
    /// [killed] fleet marker and the overlay's Revive.
    pub killed: HashSet<AgentId>,
    /// Desktop notifications opt-in, read from config at startup.
    pub notifications_enabled: bool,
    /// Last health score seen per agent, feeding the terminal-title
    /// fleet summary.
    pub latest_agent_health: HashMap<AgentId, f64>,
    /// Agents whose health crossed a band boundary for the worse while not
    /// selected. Their AGENTS rows pulse in the new band's color until the
    /// developer selects them or the score recovers. Peripheral vision for
    /// fleets; distinct from the one-tick flash on the selected agent.
    pub sustained_alerts: HashMap<AgentId, f64>,
    /// Transient notices: text plus remaining render ticks. Drawn bottom
    /// right, newest last, dropped as the TTL runs out.
    pub toasts: VecDeque<(String, u16)>,
    /// Zoomed panel: the focused panel takes the full body width until z
    /// is pressed again or Backspace steps out.
    pub zoomed: bool,
    /// Mouse capture wanted. The render loop reconciles the terminal's
    /// actual capture state with this; m toggles it, and the header shows
    /// a dim indicator while off so text selection visibly works again.
    pub mouse_enabled: bool,
    pub show_help: bool,
    pub errors: Vec<String>,
    /// Unrecoverable startup error. When set, the normal cockpit is replaced
    /// by a full-screen error card.
    pub fatal_error: Option<FatalError>,
    /// True when the user has dimmed the degraded-backend banner with [d].
    pub degraded_dismissed: bool,
    /// Capabilities reported by each connected agent during the control handshake.
    pub agent_capabilities: HashMap<AgentId, Vec<String>>,
    /// Commands dispatched from the UI that have not yet been acknowledged.
    pub pending_commands: HashMap<CommandId, AgentId>,
    /// Intervention overlay state when the modal is open.
    pub overlay: Option<InterventionOverlayState>,
    /// Pre-written suggestion surfaced by a policy alert or evaluation threshold.
    pub active_suggestion: Option<SuggestedIntervention>,
    /// Policy-issued command waiting for operator confirmation before dispatch.
    pub pending_confirmation: Option<PendingConfirmation>,
}

impl AppState {
    /// Raise a desktop notification when the developer opted in and the
    /// system has notify-send; silently does nothing otherwise. Reserved
    /// for events worth interrupting over: a critical band crossing and a
    /// policy alert waiting on confirmation.
    pub fn notify(&self, body: &str) {
        if !self.notifications_enabled {
            return;
        }
        let body = body.to_string();
        tokio::spawn(async move {
            let _ = tokio::process::Command::new("notify-send")
                .arg("Reeve")
                .arg(&body)
                .output()
                .await;
        });
    }

    /// Queue a transient notice. Three seconds at the 15fps tick rate.
    pub fn toast(&mut self, text: impl Into<String>) {
        self.toasts.push_back((text.into(), 45));
        if self.toasts.len() > 3 {
            self.toasts.pop_front();
        }
    }

    /// Decrement all flash TTLs by one tick and remove expired entries.
    pub fn advance_flash(&mut self) {
        for (_, ttl) in self.toasts.iter_mut() {
            *ttl = ttl.saturating_sub(1);
        }
        self.toasts.retain(|(_, ttl)| *ttl > 0);
        self.flash_targets.retain(|_, (_, ttl)| {
            *ttl = ttl.saturating_sub(1);
            *ttl > 0
        });
    }

    pub fn flash_color(
        &self,
        target: &FlashTarget,
        theme: &crate::theme::Theme,
    ) -> Option<ratatui::style::Color> {
        self.flash_targets.get(target).map(|(dir, _)| match dir {
            FlashDirection::Positive => theme.health_ok(),
            FlashDirection::Negative => theme.health_crit(),
            FlashDirection::Neutral => theme.text(),
            FlashDirection::Alert => theme.health_warn(),
        })
    }

    /// What the agent can actually do. SDK agents declare capabilities
    /// in their control handshake; proxy agents have no handshake, and
    /// their capabilities are what the request path supports: redirect
    /// and inject-context, applied on the next request. Pause is absent
    /// rather than present-but-broken.
    pub fn effective_capabilities(&self, agent_id: &AgentId) -> Vec<String> {
        if let Some(caps) = self.agent_capabilities.get(agent_id) {
            return caps.clone();
        }
        let is_proxy = self
            .agents
            .get(agent_id)
            .is_some_and(|a| a.agent.integration == reeve_model::entity::IntegrationPath::Proxy);
        if is_proxy {
            vec![
                "redirect".to_string(),
                "inject_context".to_string(),
                "kill".to_string(),
            ]
        } else {
            Vec::new()
        }
    }

    /// Whether the agent arrived via the proxy path.
    /// What the streaming box shows. Replay owns the box while active;
    /// live sessions show the selected agent's oldest active stream, so
    /// a quick side call never displaces the main generation, falling
    /// back to the last displayed stream that finished.
    pub fn streaming_display(&self) -> &str {
        if self.replay.is_some() {
            return &self.streaming.content;
        }
        let Some((agent_id, _)) = self.selected_agent.and_then(|i| self.agents.get_index(i)) else {
            return "";
        };
        self.streaming.display_for(agent_id)
    }

    /// The live view the trace panel shows for the selected agent: its
    /// OLDEST open turn, so concurrent side calls never displace the
    /// main conversation being watched. None once every turn completes.
    pub fn live_view_for_selected(&self) -> Option<&TraceView> {
        let (agent_id, _) = self.agents.get_index(self.selected_agent?)?;
        self.live_turns.oldest_for(agent_id)
    }

    fn live_view_for_selected_mut(&mut self) -> Option<&mut TraceView> {
        let (agent_id, _) = self.agents.get_index(self.selected_agent?)?;
        let agent_id = agent_id.clone();
        self.live_turns.oldest_for_mut(&agent_id)
    }

    /// The tree the center panel is actually showing: the live view
    /// while a turn is open, else the loaded completed trace. Selection
    /// and mouse hits must resolve against this, never against a tree
    /// the panel is not displaying.
    pub fn center_view(&self) -> Option<&TraceView> {
        self.live_view_for_selected().or(self.trace.as_ref())
    }

    pub fn center_view_mut(&mut self) -> Option<&mut TraceView> {
        if self.live_view_for_selected().is_some() {
            return self.live_view_for_selected_mut();
        }
        self.trace.as_mut()
    }

    /// Folds an observed span into its turn's live view, opening one for
    /// a trace not seen before.
    pub fn observe_span(
        &mut self,
        agent_id: AgentId,
        trace_id: TraceId,
        span_id: SpanId,
        parent_span_id: Option<SpanId>,
        operation: String,
    ) {
        self.live_turns
            .observe(agent_id, trace_id, span_id, parent_span_id, operation);
    }

    pub fn is_proxy_agent(&self, agent_id: &AgentId) -> bool {
        self.agents
            .get(agent_id)
            .is_some_and(|a| a.agent.integration == reeve_model::entity::IntegrationPath::Proxy)
    }

    /// Compact fleet summary for the terminal tab title: agent count and
    /// the worst band when anything is below healthy. A glance at the tab
    /// bar answers "is anything wrong" without switching to it.
    pub fn title_summary(&self) -> String {
        let n = self.agents.len();
        if n == 0 {
            return "reeve".to_string();
        }
        let critical = self
            .latest_agent_health
            .values()
            .filter(|s| **s < 50.0)
            .count();
        let caution = self
            .latest_agent_health
            .values()
            .filter(|s| **s >= 50.0 && **s < 80.0)
            .count();
        let plural = if n == 1 { "agent" } else { "agents" };
        if critical > 0 {
            format!("reeve: {n} {plural}, {critical} critical")
        } else if caution > 0 {
            format!("reeve: {n} {plural}, {caution} caution")
        } else {
            format!("reeve: {n} {plural}")
        }
    }

    pub fn selected_agent_id(&self) -> Option<&AgentId> {
        self.selected_agent
            .and_then(|i| self.agents.get_index(i))
            .map(|(id, _)| id)
    }
}

pub struct App {
    pub ingestion_rx: broadcast::Receiver<IngestionEvent>,
    pub engine_event_rx: broadcast::Receiver<EngineEvent>,
    pub warm: Arc<WarmStore>,
    pub dispatcher: Arc<Dispatcher>,
    pub should_quit: bool,
    /// Set on r while degraded; the engine consumes it and re-probes the
    /// evaluation backend. None only in tests.
    pub reprobe_requested: Option<Arc<std::sync::atomic::AtomicBool>>,
    pub state: AppState,
}

impl App {
    pub async fn new(
        ingestion_rx: broadcast::Receiver<IngestionEvent>,
        engine_event_rx: broadcast::Receiver<EngineEvent>,
        warm: Arc<WarmStore>,
        dispatcher: Arc<Dispatcher>,
    ) -> Self {
        let mut agents: IndexMap<AgentId, AgentState> = IndexMap::new();
        match warm.list_agents().await {
            Ok(list) => {
                for mut agent in list {
                    agent.status = reeve_model::entity::agent::AgentStatus::Idle;
                    let id = agent.id.clone();
                    agents.insert(id, AgentState::new(agent));
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to load agents on startup");
            }
        }

        let selected_agent = if agents.is_empty() { None } else { Some(0) };

        Self {
            ingestion_rx,
            engine_event_rx,
            warm,
            dispatcher,
            should_quit: false,
            reprobe_requested: None,
            state: AppState {
                agents,
                selected_agent,
                trace: None,
                live_turns: LiveTurns::default(),
                streaming: StreamingState::default(),
                health_score: None,
                prev_health_score: None,
                health_weight_coverage: None,
                health_tier2_pending: false,
                metric_scores: Vec::new(),
                policy_alerts: VecDeque::new(),
                eval_backend: None,
                eval_backend_reason: None,
                privacy_tier: 1,
                flash_targets: HashMap::new(),
                panel_focus: PanelFocus::default(),
                view_mode: ViewMode::default(),
                focus_traces: Vec::new(),
                focus_selected: 0,
                history_entries: Vec::new(),
                history_selected: 0,
                history_confirm_delete: false,
                replay: None,
                impact: None,
                cost_summary: CostSummary::default(),
                palette: None,
                palette_match: 0,
                palette_confirm_kill: false,
                note_input: None,
                filter_input: None,
                filter_applied: None,
                pending_theme: None,
                control_disconnected: HashSet::new(),
                killed: HashSet::new(),
                notifications_enabled: false,
                latest_agent_health: HashMap::new(),
                sustained_alerts: HashMap::new(),
                toasts: VecDeque::new(),
                zoomed: false,
                mouse_enabled: true,
                show_help: false,
                errors: Vec::new(),
                fatal_error: None,
                degraded_dismissed: false,
                agent_capabilities: HashMap::new(),
                pending_commands: HashMap::new(),
                overlay: None,
                active_suggestion: None,
                pending_confirmation: None,
            },
        }
    }

    pub async fn handle_ingestion_event(&mut self, event: IngestionEvent) {
        match event {
            IngestionEvent::AgentConnected { agent } => {
                let id = agent.id.clone();
                self.state
                    .agents
                    .entry(id)
                    .or_insert_with(|| AgentState::new(agent));
                if self.state.selected_agent.is_none() && !self.state.agents.is_empty() {
                    self.state.selected_agent = Some(0);
                }
            }
            IngestionEvent::AgentStatusChanged { agent_id, status } => {
                // Idle is per trace, but an agent runs concurrent
                // conversations: a side call completing must not flap a
                // still-working agent to idle. Idle applies only when no
                // other work is open; the TraceCompleted handler settles
                // it after the completing turn retires. Error and the
                // rest apply unconditionally.
                let still_working = status == reeve_model::entity::AgentStatus::Idle
                    && (self.state.live_turns.count_for(&agent_id) > 1
                        || self
                            .state
                            .streaming
                            .active
                            .iter()
                            .any(|st| st.agent_id == agent_id));
                if !still_working {
                    if let Some(s) = self.state.agents.get_mut(&agent_id) {
                        s.agent.status = status;
                    }
                }
            }
            IngestionEvent::TraceCompleted {
                trace_id,
                agent_id,
                span_count,
                cost,
            } => {
                if let Some(s) = self.state.agents.get_mut(&agent_id) {
                    s.last_trace_id = Some(trace_id.clone());
                    s.trace_count += 1;
                    s.total_cost += cost;
                    // The real cost just landed; the live estimate it
                    // was standing in for retires.
                    s.live_cost = 0.0;
                    s.cost_history.push(cost);
                    if s.cost_history.len() > 60 {
                        s.cost_history.remove(0);
                    }
                }
                // The turn is over: the live view retires and the loaded
                // completed trace takes the panel back.
                self.state.live_turns.retire(&trace_id);
                // The completing turn was this agent's last open work:
                // NOW idle is true (its own status event was suppressed
                // while the retiring turn still counted as open).
                if self.state.live_turns.count_for(&agent_id) == 0
                    && !self
                        .state
                        .streaming
                        .active
                        .iter()
                        .any(|st| st.agent_id == agent_id)
                {
                    if let Some(s) = self.state.agents.get_mut(&agent_id) {
                        if s.agent.status == reeve_model::entity::AgentStatus::Running {
                            s.agent.status = reeve_model::entity::AgentStatus::Idle;
                        }
                    }
                }
                let is_selected = self
                    .state
                    .selected_agent
                    .and_then(|i| self.state.agents.get_index(i))
                    .map(|(id, _)| id == &agent_id)
                    .unwrap_or(false);
                let takes_panel = completed_trace_takes_panel(
                    span_count,
                    self.state.trace.as_ref().map(|tv| tv.spans.len()),
                );
                if is_selected && takes_panel {
                    self.state.active_suggestion = None;
                    self.state.metric_scores.clear();
                    self.state.health_tier2_pending = false;
                    self.load_trace(trace_id).await;
                    self.update_ctx_suggestion();
                }
            }
            IngestionEvent::PipelineWarning { message } => {
                if self.state.policy_alerts.len() >= 5 {
                    self.state.policy_alerts.pop_front();
                }
                self.state.policy_alerts.push_back(PolicyAlertEntry {
                    description: message,
                    command_type: "warning".to_string(),
                    effectiveness: None,
                });
                self.state
                    .flash_targets
                    .insert(FlashTarget::AlertSection, (FlashDirection::Alert, 2));
            }
            IngestionEvent::StreamingUpdate {
                content,
                agent_id,
                span_id,
                cost_so_far,
                ..
            } => {
                // The update carries the full accumulated text, not a
                // delta: assign, so a missed broadcast message self-heals
                // on the next update instead of losing text. Keyed by
                // span so concurrent side calls cannot stomp the main
                // generation in the shared box.
                self.state
                    .streaming
                    .update(span_id, agent_id.clone(), content);
                if let Some(s) = self.state.agents.get_mut(&agent_id) {
                    if let Some(cost) = cost_so_far {
                        s.live_cost = cost;
                    }
                    // The pipeline only reports agent status when a trace
                    // completes, so between traces the fleet reads idle
                    // even while tokens are visibly streaming. A live
                    // generation is what running means; the completion
                    // signal puts the status back afterward. Paused is
                    // left alone so a pause issued mid-stream keeps
                    // rendering as paused.
                    if matches!(
                        s.agent.status,
                        reeve_model::entity::AgentStatus::Idle
                            | reeve_model::entity::AgentStatus::Error
                    ) {
                        s.agent.status = reeve_model::entity::AgentStatus::Running;
                    }
                }
            }
            IngestionEvent::SpanObserved {
                agent_id,
                trace_id,
                span_id,
                parent_span_id,
                operation,
            } => {
                // A span being observed means its round trip finalized:
                // any stream it was feeding is over.
                self.state.streaming.finish(&span_id);
                self.state
                    .observe_span(agent_id, trace_id, span_id, parent_span_id, operation);
            }
            IngestionEvent::SpanCompleted { .. } => {}
        }
    }

    pub fn handle_engine_event(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::HealthScoreUpdated {
                agent_id,
                trace_id,
                score,
                weight_coverage,
                tier2_pending,
            } => {
                self.state
                    .latest_agent_health
                    .insert(agent_id.clone(), score);
                let prev = self.state.prev_health_score;
                self.state.prev_health_score = Some(score);
                self.state.health_score = Some(score);
                self.state.health_weight_coverage = Some(weight_coverage);
                self.state.health_tier2_pending = tier2_pending;

                if let Some(prev_score) = prev {
                    let dir = if score > prev_score + 0.5 {
                        FlashDirection::Positive
                    } else if score < prev_score - 0.5 {
                        FlashDirection::Negative
                    } else {
                        FlashDirection::Neutral
                    };
                    self.state
                        .flash_targets
                        .insert(FlashTarget::HealthGauge, (dir, 2));
                    self.state
                        .flash_targets
                        .insert(FlashTarget::AgentRow(agent_id.clone()), (dir, 2));

                    // Band crossings on unselected agents start a sustained
                    // pulse; recovery or selection clears it.
                    let is_selected = self.state.selected_agent_id() == Some(&agent_id);
                    match sustained_transition(prev_score, score, is_selected) {
                        Some(true) => {
                            self.state.sustained_alerts.insert(agent_id.clone(), score);
                            if score < 50.0 {
                                let name = self
                                    .state
                                    .agents
                                    .get(&agent_id)
                                    .map(|a| a.agent.name.clone())
                                    .unwrap_or_else(|| agent_id.to_string());
                                self.state
                                    .notify(&format!("{name} is critical (health {score:.0})"));
                            }
                        }
                        Some(false) => {
                            self.state.sustained_alerts.remove(&agent_id);
                        }
                        None => {}
                    }
                }

                // Associate the score with gen_ai.* spans in the loaded trace.
                if let Some(ref mut tv) = self.state.trace {
                    if tv.trace_id == trace_id {
                        let llm_spans: Vec<SpanId> = tv
                            .spans
                            .iter()
                            .filter(|(_, s)| s.operation.starts_with("gen_ai."))
                            .map(|(id, _)| id.clone())
                            .collect();
                        for sid in llm_spans {
                            tv.span_health_scores.insert(sid, score);
                        }
                    }
                }
            }
            EngineEvent::EvaluationBackendReady {
                backend,
                reason,
                privacy_tier,
            } => {
                if let Some(ref r) = reason {
                    tracing::info!(reason = r, "evaluation backend disabled");
                }
                // A recovery deserves its three seconds: degraded before,
                // working now.
                let was_degraded = self.state.eval_backend.as_deref() == Some("disabled");
                if was_degraded && backend != "disabled" {
                    self.state
                        .toast(format!("tier 2 evaluation resumed \u{00B7} {backend}"));
                }
                self.state.eval_backend_reason = reason;
                self.state.eval_backend = Some(backend);
                self.state.privacy_tier = privacy_tier;
            }
            EngineEvent::EvaluationComplete {
                metric,
                score,
                confidence,
                ..
            } => {
                if let Some(entry) = self
                    .state
                    .metric_scores
                    .iter_mut()
                    .find(|e| e.name == metric)
                {
                    entry.score = score;
                    entry.confidence = confidence;
                } else {
                    self.state.metric_scores.push(MetricScore {
                        name: metric.clone(),
                        score,
                        confidence,
                    });
                }
                if metric == "faithfulness" && score < 0.6 {
                    self.state.active_suggestion = Some(SuggestedIntervention {
                        command: OverlayCommand::InjectContext,
                        text: "Please only use retrieved source material.".to_string(),
                    });
                }
                self.update_ctx_suggestion();
            }
            EngineEvent::PolicyAlert {
                rule_id,
                description,
                command_type,
                requires_confirmation,
                auto_confirm_after_secs,
                effectiveness,
            } => {
                if self.state.policy_alerts.len() >= 5 {
                    self.state.policy_alerts.pop_front();
                }
                self.state.policy_alerts.push_back(PolicyAlertEntry {
                    description: description.clone(),
                    command_type: command_type.clone(),
                    effectiveness: effectiveness.map(|h| {
                        format!(
                            "{}: {:+.2} avg \u{00B7} {} tries",
                            h.command, h.avg_delta, h.sample_count
                        )
                    }),
                });
                // Most severe first on screen: the weight of the command
                // the rule carries, ties most recent first. The panel draws
                // back-of-deque at the top, so the back must hold the most
                // severe newest entry: drain newest-first, stable-sort by
                // rank, then reverse so severity ascends front to back.
                let mut entries: Vec<PolicyAlertEntry> =
                    self.state.policy_alerts.drain(..).rev().collect();
                entries.sort_by_key(|e| alert_severity_rank(&e.command_type));
                entries.reverse();
                self.state.policy_alerts = entries.into();
                self.state
                    .flash_targets
                    .insert(FlashTarget::AlertSection, (FlashDirection::Alert, 2));
                if let Some(s) = suggestion_for_rule(&rule_id) {
                    self.state.active_suggestion = Some(s);
                }
                if requires_confirmation {
                    self.state
                        .notify(&format!("policy alert needs confirmation: {description}"));
                    if let Some(agent_id) = self.state.selected_agent_id().cloned() {
                        // A suggestion the target cannot apply must not
                        // dispatch, and must never auto-dispatch: the
                        // countdown would fire a guaranteed dead letter.
                        let supported = suggestion_supported(
                            &self.state.effective_capabilities(&agent_id),
                            &command_type,
                        );
                        self.state.pending_confirmation = Some(PendingConfirmation {
                            agent_id,
                            rule_id,
                            description,
                            command_type,
                            auto_confirm_after_secs: auto_confirm_after_secs.filter(|_| supported),
                            arrived_at_ms: current_ms(),
                            supported,
                        });
                    }
                }
            }
            EngineEvent::AgentControlConnected {
                agent_id,
                capabilities,
            } => {
                self.state.control_disconnected.remove(&agent_id);
                self.state.agent_capabilities.insert(agent_id, capabilities);
            }
            EngineEvent::AgentControlDisconnected { agent_id } => {
                self.state.control_disconnected.insert(agent_id.clone());
                self.state.agent_capabilities.remove(&agent_id);
                // Close overlay if open for the disconnecting agent.
                if let Some(ref ov) = self.state.overlay {
                    if ov.agent_id == agent_id {
                        self.state.overlay = None;
                    }
                }
            }
        }
    }

    async fn load_trace(&mut self, trace_id: TraceId) {
        match self.warm.list_spans_for_trace(&trace_id).await {
            Ok(spans) if spans.is_empty() => {
                // Nothing stored for this trace: keep whatever the panel
                // is showing rather than replacing it with a blank view.
            }
            Ok(spans) => {
                let mut span_map: HashMap<SpanId, InternalSpan> = HashMap::new();
                let mut children: HashMap<SpanId, Vec<SpanId>> = HashMap::new();
                let mut names: HashMap<SpanId, String> = HashMap::new();
                let mut root: Option<SpanId> = None;
                let mut arrival: Vec<SpanId> = Vec::new();

                for span in spans {
                    if let Some(ref pid) = span.parent_id {
                        children
                            .entry(pid.clone())
                            .or_default()
                            .push(span.id.clone());
                    } else if root.is_none() {
                        root = Some(span.id.clone());
                    }
                    names.insert(span.id.clone(), span.operation.clone());
                    arrival.push(span.id.clone());
                    span_map.insert(span.id.clone(), span);
                }

                let collapsed = HashSet::new();
                let mut span_order = root
                    .as_ref()
                    .map(|r| flatten_tree(r, &children, &collapsed))
                    .unwrap_or_default();
                // Spans not reachable from the root: an interrupted trace
                // is often rootless (the turn root only emits at turn
                // end), and these used to vanish, rendering a trace full
                // of spans as "no trace selected".
                let reachable: HashSet<&SpanId> = span_order.iter().collect();
                let orphans: Vec<SpanId> = arrival
                    .iter()
                    .filter(|id| !reachable.contains(id))
                    .cloned()
                    .collect();
                span_order.extend(orphans.iter().cloned());

                let raw_outcomes = self
                    .warm
                    .get_intervention_outcomes_for_trace(&trace_id)
                    .await
                    .unwrap_or_default();
                let mut outcome_lines: Vec<OutcomeLine> = Vec::new();
                for oc in raw_outcomes {
                    let (span_id, cmd_label) =
                        match self.warm.get_intervention_command(&oc.command_id).await {
                            Ok(Some(cmd)) => {
                                let label = match &cmd.command_type {
                                    CommandType::Pause | CommandType::Resume => "pause",
                                    CommandType::Kill => "kill",
                                    CommandType::Redirect { .. } => "redirect",
                                    CommandType::InjectContext { .. } => "inject",
                                };
                                (cmd.span_id, label)
                            }
                            _ => (None, "intervention"),
                        };
                    let delta_str = match oc.delta {
                        Some(d) => format!("{d:+.2} quality"),
                        None => "no delta".to_string(),
                    };
                    let spans_str = match oc.spans_measured {
                        Some(n) => format!(" \u{00B7} {n} spans"),
                        None => String::new(),
                    };
                    outcome_lines.push(OutcomeLine {
                        span_id,
                        text: format!("{cmd_label} {delta_str}{spans_str}"),
                    });
                }
                let notes = self
                    .warm
                    .span_notes_for_trace(&trace_id)
                    .await
                    .map(|m| {
                        m.into_iter()
                            .map(|(k, n)| (SpanId::from(k.as_str()), n.content))
                            .collect()
                    })
                    .unwrap_or_default();

                self.state.trace = Some(TraceView {
                    trace_id,
                    root,
                    spans: span_map,
                    children,
                    names,
                    span_order,
                    scroll: 0,
                    selected: None,
                    collapsed,
                    span_health_scores: HashMap::new(),
                    outcome_lines,
                    orphans,
                    notes,
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to load trace spans");
            }
        }
    }

    pub async fn handle_action(&mut self, action: Action) {
        // Confirmation modal takes priority over everything else.
        if self.state.pending_confirmation.is_some() {
            self.handle_confirmation_action(action).await;
            return;
        }
        // When overlay is open, consume most actions before they reach the cockpit.
        if self.state.overlay.is_some() {
            self.handle_overlay_action(action).await;
            return;
        }
        // The filter bar owns the keyboard while open.
        if self.state.filter_input.is_some() {
            self.handle_filter_action(action);
            return;
        }
        // The note input owns the keyboard while open.
        if self.state.note_input.is_some() {
            self.handle_note_action(action).await;
            return;
        }
        // The palette owns the keyboard while open.
        if self.state.palette.is_some() {
            self.handle_palette_action(action).await;
            return;
        }
        // Replay owns the keyboard while active: h/l become stepping keys,
        // and Esc exits back to History.
        if self.state.replay.is_some() && self.handle_replay_action(&action) {
            return;
        }
        // The impact view only reads; Esc closes it, quit still works.
        if self.state.impact.is_some() {
            match action {
                Action::Dismiss => self.state.impact = None,
                Action::Quit => self.should_quit = true,
                _ => {}
            }
            return;
        }
        // History view owns navigation and its own delete confirmation; only
        // quit, help, and the mode keys pass through to the shared handling.
        if self.state.view_mode == ViewMode::History && self.handle_history_action(&action).await {
            return;
        }

        match action {
            Action::Quit => self.should_quit = true,
            Action::MoveUp | Action::VimUp => match self.state.panel_focus {
                PanelFocus::Left => {
                    self.move_selection(-1);
                    self.load_trace_for_selected().await;
                }
                PanelFocus::Center => self.move_center_selection(-1),
                _ => {}
            },
            Action::MoveDown | Action::VimDown => match self.state.panel_focus {
                PanelFocus::Left => {
                    self.move_selection(1);
                    self.load_trace_for_selected().await;
                }
                PanelFocus::Center => self.move_center_selection(1),
                _ => {}
            },
            Action::ScrollUp => match self.state.panel_focus {
                PanelFocus::Center => {
                    if let Some(ref mut tv) = self.state.trace {
                        tv.scroll = tv.scroll.saturating_sub(1);
                    }
                }
                PanelFocus::Right => {
                    self.state.streaming.auto_scroll = false;
                    self.state.streaming.scroll = self.state.streaming.scroll.saturating_sub(1);
                }
                _ => {}
            },
            Action::ScrollDown => match self.state.panel_focus {
                PanelFocus::Center => {
                    if let Some(ref mut tv) = self.state.trace {
                        tv.scroll += 1;
                    }
                }
                PanelFocus::Right => {
                    self.state.streaming.scroll += 1;
                }
                _ => {}
            },
            // With a filter applied, Tab cycles matches instead of panels.
            Action::NextPanel if self.state.filter_applied.is_some() => {
                self.cycle_filter_match(true);
            }
            Action::PrevPanel if self.state.filter_applied.is_some() => {
                self.cycle_filter_match(false);
            }
            Action::NextPanel => {
                self.state.panel_focus = match self.state.panel_focus {
                    PanelFocus::Left => PanelFocus::Center,
                    PanelFocus::Center => PanelFocus::Right,
                    PanelFocus::Right => PanelFocus::Left,
                };
            }
            Action::PrevPanel => {
                self.state.panel_focus = match self.state.panel_focus {
                    PanelFocus::Left => PanelFocus::Right,
                    PanelFocus::Center => PanelFocus::Left,
                    PanelFocus::Right => PanelFocus::Center,
                };
            }
            Action::Select => {
                if self.state.panel_focus == PanelFocus::Center {
                    if let Some(ref mut tv) = self.state.trace {
                        if let Some(selected) = tv.selected.clone() {
                            if tv.collapsed.contains(&selected) {
                                tv.collapsed.remove(&selected);
                            } else {
                                tv.collapsed.insert(selected.clone());
                            }
                            tv.span_order = tv
                                .root
                                .as_ref()
                                .map(|r| flatten_tree(r, &tv.children, &tv.collapsed))
                                .unwrap_or_default();
                        }
                    }
                }
            }
            Action::ToggleHelp => {
                self.state.show_help = !self.state.show_help;
            }
            Action::Dismiss => {
                self.state.show_help = false;
            }
            Action::DismissDegraded => {
                self.state.degraded_dismissed = true;
            }
            Action::Retry => {
                if self.state.fatal_error.is_some() {
                    self.state.fatal_error = None;
                } else {
                    // Ask the engine to re-probe the evaluation backend. The
                    // banner stays until its EvaluationBackendReady answers,
                    // so a failed probe re-degrades honestly instead of the
                    // banner clearing on hope.
                    if let Some(ref flag) = self.reprobe_requested {
                        flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    self.state.degraded_dismissed = false;
                    self.state.toast("re-probing evaluation backend");
                }
            }
            Action::OverlayOpen => {
                if let Some(agent_id) = self.state.selected_agent_id().cloned() {
                    self.state.overlay = Some(InterventionOverlayState {
                        agent_id,
                        mode: OverlayMode::Menu,
                    });
                }
            }
            Action::QuickPause => {
                if let Some(agent_id) = self.state.selected_agent_id().cloned() {
                    let cmd = self.pause_or_resume(&agent_id);
                    self.dispatch_command(agent_id, cmd).await;
                }
            }
            Action::Char('1') => {
                self.state.view_mode = ViewMode::Fleet;
            }
            Action::Char('2') => {
                self.enter_focus().await;
            }
            Action::Char('3') => {
                self.enter_history().await;
            }
            Action::Char('m') => {
                self.state.mouse_enabled = !self.state.mouse_enabled;
            }
            Action::JumpTop => match self.state.panel_focus {
                PanelFocus::Left => {
                    if !self.state.agents.is_empty() {
                        self.state.selected_agent = Some(0);
                        self.load_trace_for_selected().await;
                    }
                }
                PanelFocus::Center => {
                    if let Some(tv) = self.state.center_view_mut() {
                        tv.selected = tv.span_order.first().cloned();
                        tv.scroll = 0;
                    }
                }
                PanelFocus::Right => {
                    self.state.streaming.auto_scroll = false;
                    self.state.streaming.scroll = 0;
                }
            },
            Action::JumpBottom => match self.state.panel_focus {
                PanelFocus::Left => {
                    let len = self.state.agents.len();
                    if len > 0 {
                        self.state.selected_agent = Some(len - 1);
                        self.load_trace_for_selected().await;
                    }
                }
                PanelFocus::Center => {
                    if let Some(tv) = self.state.center_view_mut() {
                        tv.selected = tv.span_order.last().cloned();
                        tv.scroll = tv.span_order.len().saturating_sub(1) as u16;
                    }
                }
                PanelFocus::Right => {
                    self.state.streaming.auto_scroll = true;
                }
            },
            Action::HalfPageDown => {
                if self.state.panel_focus == PanelFocus::Center {
                    self.move_center_selection(10);
                }
            }
            Action::HalfPageUp => {
                if self.state.panel_focus == PanelFocus::Center {
                    self.move_center_selection(-10);
                }
            }
            // a expands every node, A collapses to the root.
            Action::Char('a') => {
                if let Some(ref mut tv) = self.state.trace {
                    tv.collapsed.clear();
                    tv.span_order = tv
                        .root
                        .as_ref()
                        .map(|r| flatten_tree(r, &tv.children, &tv.collapsed))
                        .unwrap_or_default();
                }
            }
            Action::Char('A') => {
                if let Some(ref mut tv) = self.state.trace {
                    if let Some(root) = tv.root.clone() {
                        // Every node with children collapses, the root
                        // included: collapse-all leaves just the root row.
                        tv.collapsed = tv.children.keys().cloned().collect();
                        tv.span_order = flatten_tree(&root, &tv.children, &tv.collapsed);
                        tv.selected = Some(root);
                    }
                }
            }
            Action::Char('z') => {
                self.state.zoomed = !self.state.zoomed;
            }
            Action::Char('P') => {
                // Fleet pause behind the same confirmation style kill all
                // uses; shares the palette's dispatch path.
                self.state.palette = Some("pause all".to_string());
                self.state.palette_match = 0;
                self.execute_palette_command("pause all").await;
            }
            Action::Backspace => {
                // Step back one level: zoom, then view mode, then nothing.
                if self.state.zoomed {
                    self.state.zoomed = false;
                } else if self.state.view_mode != ViewMode::Fleet {
                    self.state.view_mode = ViewMode::Fleet;
                }
            }
            Action::Char('/') => {
                if self.state.trace.is_some() {
                    self.state.filter_input =
                        Some(self.state.filter_applied.clone().unwrap_or_default());
                }
            }
            Action::Char('x') => {
                // Dismiss the top alert, which the panel draws from the back
                // of the deque. An alert that fired is informational once its
                // command has been through its own dispatch and confirmation
                // path.
                self.state.policy_alerts.pop_back();
            }
            Action::Char('y') => {
                let copied = self.state.trace.as_ref().and_then(|tv| {
                    let id = tv.selected.as_ref()?;
                    let span = tv.spans.get(id)?;
                    let dur = span
                        .end_time
                        .map(|e| format!("{} ms", e - span.start_time))
                        .unwrap_or_else(|| "running".to_string());
                    Some(format!(
                        "{} · {} · {} · {:?}",
                        span.operation,
                        id.as_str(),
                        dur,
                        span.status
                    ))
                });
                if let Some(text) = copied {
                    crate::clipboard::copy(&text);
                    self.state.toast("span copied");
                }
            }
            Action::Char('Y') => {
                if let Some(id) = self.state.trace.as_ref().map(|tv| tv.trace_id.clone()) {
                    crate::clipboard::copy(id.as_str());
                    self.state.toast("trace id copied");
                }
            }
            Action::Char('e') => {
                if let Some(trace_id) = self.state.trace.as_ref().map(|tv| tv.trace_id.clone()) {
                    match self.export_trace(&trace_id).await {
                        Ok(path) => self.state.toast(format!("exported {path}")),
                        Err(e) => {
                            tracing::warn!(error = %e, "trace export failed");
                            self.state.toast("export failed");
                        }
                    }
                }
            }
            Action::Char('n') => {
                if let Some(span_id) = self.state.trace.as_ref().and_then(|tv| tv.selected.clone())
                {
                    let existing = self
                        .state
                        .trace
                        .as_ref()
                        .and_then(|tv| tv.notes.get(&span_id).cloned())
                        .unwrap_or_default();
                    self.state.note_input = Some((span_id, existing));
                }
            }
            Action::Char(':') => {
                self.state.palette = Some(String::new());
                self.state.palette_match = 0;
                self.state.palette_confirm_kill = false;
            }
            Action::Char('T') => {
                let current = self.state.pending_theme.as_deref().unwrap_or("");
                let idx = crate::theme::BUILTIN_THEMES
                    .iter()
                    .position(|t| *t == current)
                    .unwrap_or(0);
                let next =
                    crate::theme::BUILTIN_THEMES[(idx + 1) % crate::theme::BUILTIN_THEMES.len()];
                self.state.pending_theme = Some(next.to_string());
            }
            Action::Char('4') => match self.warm.cost_summary().await {
                Ok(summary) => {
                    self.state.cost_summary = summary;
                    self.state.view_mode = ViewMode::Cost;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to load cost summary");
                }
            },
            // Focus view: step backward/forward through the agent's trace
            // history. Newest-first list, so '[' (older) moves down it.
            Action::Char('[') if self.state.view_mode == ViewMode::Focus => {
                self.focus_step(1).await;
            }
            Action::Char(']') if self.state.view_mode == ViewMode::Focus => {
                self.focus_step(-1).await;
            }
            Action::Resize(_, _) | Action::Char(_) => {}
            // Line editing only means something inside a text input.
            Action::DeleteWord | Action::ClearLine => {}
        }
    }

    async fn handle_overlay_action(&mut self, action: Action) {
        let Some(overlay) = self.state.overlay.as_mut() else {
            return;
        };

        match &overlay.mode {
            OverlayMode::KillConfirm => match action {
                Action::Char('y') => {
                    let agent_id = overlay.agent_id.clone();
                    self.state.overlay = None;
                    self.dispatch_command(agent_id, CommandType::Kill).await;
                }
                Action::Char('n') | Action::Dismiss => {
                    if let Some(ref mut ov) = self.state.overlay {
                        ov.mode = OverlayMode::Menu;
                    }
                }
                _ => {}
            },
            OverlayMode::TextInput { .. } => match action {
                Action::DeleteWord | Action::ClearLine => {
                    if let Some(InterventionOverlayState {
                        mode: OverlayMode::TextInput { buffer, .. },
                        ..
                    }) = self.state.overlay.as_mut()
                    {
                        edit_buffer(buffer, matches!(action, Action::DeleteWord));
                    }
                }
                Action::Select => {
                    let Some(ov) = self.state.overlay.take() else {
                        return;
                    };
                    if let OverlayMode::TextInput { command, buffer } = ov.mode {
                        let cmd_type = match command {
                            OverlayCommand::Redirect => CommandType::Redirect {
                                instruction: buffer,
                            },
                            OverlayCommand::InjectContext => {
                                CommandType::InjectContext { context: buffer }
                            }
                            _ => return,
                        };
                        self.dispatch_command(ov.agent_id, cmd_type).await;
                    }
                }
                Action::Dismiss => {
                    if let Some(ref mut ov) = self.state.overlay {
                        ov.mode = OverlayMode::Menu;
                    }
                }
                Action::Backspace => {
                    if let Some(ref mut ov) = self.state.overlay {
                        if let OverlayMode::TextInput { ref mut buffer, .. } = ov.mode {
                            buffer.pop();
                        }
                    }
                }
                Action::Char(c) => {
                    if let Some(ref mut ov) = self.state.overlay {
                        if let OverlayMode::TextInput { ref mut buffer, .. } = ov.mode {
                            buffer.push(c);
                        }
                    }
                }
                _ => {}
            },
            OverlayMode::Menu => {
                let agent_id = overlay.agent_id.clone();
                let caps = self.state.effective_capabilities(&agent_id);

                // Suggestion keys take priority when a suggestion is active.
                if self.state.active_suggestion.is_some() {
                    match action {
                        // [Enter] dispatches the suggestion immediately.
                        Action::Select => {
                            let suggestion = self.state.active_suggestion.take().unwrap();
                            let cmd_type = suggestion_to_command_type(suggestion);
                            self.state.overlay = None;
                            self.dispatch_command(agent_id, cmd_type).await;
                            return;
                        }
                        // [Tab] copies suggestion text into the input field.
                        Action::NextPanel => {
                            let suggestion = self.state.active_suggestion.take().unwrap();
                            let (command, text) = (suggestion.command, suggestion.text);
                            if let Some(ref mut ov) = self.state.overlay {
                                ov.mode = OverlayMode::TextInput {
                                    command,
                                    buffer: text,
                                };
                            }
                            return;
                        }
                        // [Esc] dismisses just the suggestion, keeps overlay open.
                        Action::Dismiss => {
                            self.state.active_suggestion = None;
                            return;
                        }
                        _ => {}
                    }
                }

                match action {
                    Action::Dismiss => {
                        self.state.overlay = None;
                    }
                    // [p] in overlay = pause/resume toggle
                    Action::QuickPause if caps.contains(&"pause".to_string()) => {
                        self.state.overlay = None;
                        let cmd = self.pause_or_resume(&agent_id);
                        self.dispatch_command(agent_id, cmd).await;
                    }
                    // [r] in overlay = redirect
                    Action::Retry if caps.contains(&"redirect".to_string()) => {
                        if let Some(ref mut ov) = self.state.overlay {
                            ov.mode = OverlayMode::TextInput {
                                command: OverlayCommand::Redirect,
                                buffer: String::new(),
                            };
                        }
                    }
                    Action::Char('c') if caps.contains(&"inject_context".to_string()) => {
                        if let Some(ref mut ov) = self.state.overlay {
                            ov.mode = OverlayMode::TextInput {
                                command: OverlayCommand::InjectContext,
                                buffer: String::new(),
                            };
                        }
                    }
                    // [k] in overlay = kill. Only a running or paused agent
                    // can be killed; anything else gets told up front rather
                    // than walked through a confirmation the SDK will refuse.
                    Action::VimUp if caps.contains(&"kill".to_string()) => {
                        // An engaged breaker flips k to Revive: the one
                        // recovery short of restarting Reeve. No confirm,
                        // since revive is the safe direction.
                        if self.state.killed.contains(&agent_id) {
                            self.state.overlay = None;
                            self.dispatch_command(agent_id, CommandType::Resume).await;
                            self.state.toast("breaker cleared; agent may resume");
                            return;
                        }
                        // A proxy agent between requests shows idle, and the
                        // breaker's whole point is stopping the next request,
                        // so proxy agents are killable in any state.
                        let killable = self.state.is_proxy_agent(&agent_id)
                            || self
                                .state
                                .overlay
                                .as_ref()
                                .and_then(|ov| self.state.agents.get(&ov.agent_id))
                                .is_some_and(|a| {
                                    matches!(
                                        a.agent.status,
                                        reeve_model::entity::AgentStatus::Running
                                            | reeve_model::entity::AgentStatus::Paused
                                    )
                                });
                        if killable {
                            if let Some(ref mut ov) = self.state.overlay {
                                ov.mode = OverlayMode::KillConfirm;
                            }
                        } else {
                            self.state.toast("agent is not running, nothing to kill");
                        }
                    }
                    // [1] and [2] load templates into the input field.
                    Action::Char('1') if caps.contains(&"redirect".to_string()) => {
                        if let Some(ref mut ov) = self.state.overlay {
                            ov.mode = OverlayMode::TextInput {
                                command: OverlayCommand::Redirect,
                                buffer: TEMPLATES[0].text.to_string(),
                            };
                        }
                    }
                    Action::Char('2') if caps.contains(&"redirect".to_string()) => {
                        if let Some(ref mut ov) = self.state.overlay {
                            ov.mode = OverlayMode::TextInput {
                                command: OverlayCommand::Redirect,
                                buffer: TEMPLATES[1].text.to_string(),
                            };
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    async fn handle_confirmation_action(&mut self, action: Action) {
        match action {
            Action::Select => {
                if let Some(pc) = self.state.pending_confirmation.take() {
                    if !pc.supported {
                        // The suggestion cannot apply on this agent's
                        // path: hand the operator the actions that can,
                        // instead of dispatching a dead letter.
                        self.state.overlay = Some(InterventionOverlayState {
                            agent_id: pc.agent_id,
                            mode: OverlayMode::Menu,
                        });
                        return;
                    }
                    let issued_by = format!("policy:{}", pc.rule_id);
                    self.dispatch_confirmation(pc, issued_by).await;
                }
            }
            Action::Dismiss => {
                self.state.pending_confirmation = None;
            }
            _ => {}
        }
    }

    async fn dispatch_confirmation(&mut self, pc: PendingConfirmation, issued_by: String) {
        let Some(cmd_type) = confirmation_command_type(&pc.command_type, &pc.description) else {
            tracing::warn!(
                command_type = %pc.command_type,
                rule_id = %pc.rule_id,
                "confirmed policy command has unknown type; nothing dispatched"
            );
            return;
        };
        self.dispatch_command_with_attribution(pc.agent_id, cmd_type, issued_by)
            .await;
    }

    async fn dispatch_command(&mut self, agent_id: AgentId, command_type: CommandType) {
        self.dispatch_command_with_attribution(agent_id, command_type, "human".to_string())
            .await;
    }

    /// The pause key is a toggle: Resume when the dispatcher has a confirmed
    /// applied Pause for this agent, Pause otherwise. The dispatcher's set is
    /// the only state that reflects what the agent actually acknowledged, as
    /// opposed to what was merely sent.
    fn pause_or_resume(&self, agent_id: &AgentId) -> CommandType {
        if self.dispatcher.is_paused(agent_id) {
            CommandType::Resume
        } else {
            CommandType::Pause
        }
    }

    async fn dispatch_command_with_attribution(
        &mut self,
        agent_id: AgentId,
        command_type: CommandType,
        issued_by: String,
    ) {
        let now_ms = current_ms();
        let command_id = CommandId::from(format!("cmd-{now_ms:x}"));
        let trace_id = self
            .state
            .agents
            .get(&agent_id)
            .and_then(|s| s.last_trace_id.clone())
            .unwrap_or_else(|| TraceId::from(""));

        let command = InterventionCommand {
            id: command_id.clone(),
            trace_id,
            span_id: None,
            policy_id: None,
            command_type,
            status: CommandStatus::Pending,
            requires_confirmation: false,
            issued_at: now_ms,
            acknowledged_at: None,
            issued_by,
            valid_until_ms: now_ms + 60_000,
        };

        let dispatched = self.dispatcher.dispatch(&agent_id, command).await;
        if dispatched {
            self.state.pending_commands.insert(command_id, agent_id);
        }
    }

    /// Whether keystrokes are currently text rather than commands. Drives the
    /// mode-aware key mapping in `input::map_event`.
    pub fn text_input_active(&self) -> bool {
        self.state.note_input.is_some()
            || self.state.filter_input.is_some()
            || self.state.palette.is_some()
            || matches!(
                self.state.overlay,
                Some(InterventionOverlayState {
                    mode: OverlayMode::TextInput { .. },
                    ..
                })
            )
    }

    /// Called from the tick loop. Overlays the dispatcher's confirmed pause
    /// state onto agent display status. The dispatcher is authoritative for
    /// pause because it processes the applied acks; ingestion events cannot
    /// carry pause state since a paused agent emits no spans.
    /// Mirrors the dispatcher's breaker state into the fleet each tick,
    /// so a kill from any source (operator or policy) becomes visible.
    pub fn sync_killed_status(&mut self) {
        let ids: Vec<AgentId> = self.state.agents.keys().cloned().collect();
        for id in ids {
            if self.dispatcher.is_killed(&id) {
                self.state.killed.insert(id);
            } else {
                self.state.killed.remove(&id);
            }
        }
    }

    pub fn sync_pause_status(&mut self) {
        use reeve_model::entity::agent::AgentStatus;
        for (agent_id, s) in self.state.agents.iter_mut() {
            let paused = self.dispatcher.is_paused(agent_id);
            if paused && s.agent.status != AgentStatus::Paused {
                s.agent.status = AgentStatus::Paused;
            } else if !paused && s.agent.status == AgentStatus::Paused {
                s.agent.status = AgentStatus::Idle;
            }
        }
    }

    /// Called from the tick loop. Dispatches a policy confirmation automatically when
    /// `auto_confirm_after_secs` has elapsed since the alert arrived.
    pub async fn check_auto_confirm(&mut self) {
        let expired = self
            .state
            .pending_confirmation
            .as_ref()
            .and_then(|pc| pc.auto_confirm_after_secs)
            .map(|secs| {
                let pc = self.state.pending_confirmation.as_ref().unwrap();
                current_ms() >= pc.arrived_at_ms + (secs as i64 * 1000)
            })
            .unwrap_or(false);
        if expired {
            if let Some(pc) = self.state.pending_confirmation.take() {
                let issued_by = format!("policy_auto:{}", pc.rule_id);
                self.dispatch_confirmation(pc, issued_by).await;
            }
        }
    }

    fn update_ctx_suggestion(&mut self) {
        let Some(trace) = self.state.trace.as_ref() else {
            return;
        };
        for span in trace.spans.values() {
            let Some(tok_in) = span
                .attributes
                .get("gen_ai.usage.input_tokens")
                .and_then(|v| v.as_u64())
            else {
                continue;
            };
            let model = span
                .attributes
                .get("gen_ai.request.model")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if let Some(max) = crate::context_windows::context_window_for_model(model) {
                let pct = tok_in as f64 / f64::from(max);
                if pct >= 0.85 && self.state.active_suggestion.is_none() {
                    self.state.active_suggestion = Some(SuggestedIntervention {
                        command: OverlayCommand::InjectContext,
                        text: "Please complete your current task promptly.".to_string(),
                    });
                    return;
                }
            }
        }
    }

    fn move_center_selection(&mut self, delta: i32) {
        // The displayed tree, live or loaded: selection on a tree the
        // panel is not showing reads as dead keys.
        let Some(tv) = self.state.center_view_mut() else {
            return;
        };
        if tv.span_order.is_empty() {
            return;
        }
        let current = tv
            .selected
            .as_ref()
            .and_then(|id| tv.span_order.iter().position(|s| s == id))
            .unwrap_or(0);
        let next = (current as i32 + delta).rem_euclid(tv.span_order.len() as i32) as usize;
        tv.selected = Some(tv.span_order[next].clone());
    }

    /// Opens History view: completed traces for the selected agent, or all
    /// agents when none is selected. An empty list is a meaningful view
    /// here, unlike Focus, so entry always succeeds.
    async fn enter_history(&mut self) {
        let agent_id = self.state.selected_agent_id().cloned();
        match self.warm.list_history(agent_id.as_ref(), 200).await {
            Ok(entries) => {
                self.state.history_entries = entries;
                self.state.history_selected = 0;
                self.state.history_confirm_delete = false;
                self.state.view_mode = ViewMode::History;
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to load trace history");
            }
        }
    }

    /// Handles an action while History view is active. Returns true when the
    /// action was consumed. Quit, help, and the mode-switch keys fall
    /// through to the shared handler.
    async fn handle_history_action(&mut self, action: &Action) -> bool {
        if self.state.history_confirm_delete {
            match action {
                Action::Char('y') => {
                    if let Some((trace, _)) =
                        self.state.history_entries.get(self.state.history_selected)
                    {
                        let trace_id = trace.id.clone();
                        if let Err(e) = self.warm.delete_trace(&trace_id).await {
                            tracing::warn!(error = %e, "failed to delete trace");
                        } else {
                            self.state
                                .history_entries
                                .remove(self.state.history_selected);
                            if self.state.history_selected >= self.state.history_entries.len() {
                                self.state.history_selected =
                                    self.state.history_entries.len().saturating_sub(1);
                            }
                            // The deleted trace may be what the right panel
                            // is showing; drop it rather than display a
                            // trace that no longer exists.
                            if self
                                .state
                                .trace
                                .as_ref()
                                .is_some_and(|tv| tv.trace_id == trace_id)
                            {
                                self.state.trace = None;
                            }
                        }
                    }
                    self.state.history_confirm_delete = false;
                }
                _ => {
                    // Any other key declines: deletion must be deliberate.
                    self.state.history_confirm_delete = false;
                }
            }
            return true;
        }
        match action {
            Action::MoveDown | Action::VimDown => {
                let len = self.state.history_entries.len();
                if len > 0 && self.state.history_selected < len - 1 {
                    self.state.history_selected += 1;
                }
                true
            }
            Action::MoveUp | Action::VimUp => {
                self.state.history_selected = self.state.history_selected.saturating_sub(1);
                true
            }
            Action::JumpTop => {
                self.state.history_selected = 0;
                true
            }
            Action::JumpBottom => {
                self.state.history_selected = self.state.history_entries.len().saturating_sub(1);
                true
            }
            Action::HalfPageDown => {
                let len = self.state.history_entries.len();
                if len > 0 {
                    self.state.history_selected = (self.state.history_selected + 10).min(len - 1);
                }
                true
            }
            Action::HalfPageUp => {
                self.state.history_selected = self.state.history_selected.saturating_sub(10);
                true
            }
            Action::Select => {
                // Loads full detail into the right panel from the warm
                // store. The load alone was invisible: nothing was
                // selected, so SPAN DETAIL kept saying "select a span".
                // Selecting the root makes the detail land.
                if let Some((trace, _)) =
                    self.state.history_entries.get(self.state.history_selected)
                {
                    let id = trace.id.clone();
                    self.load_trace(id).await;
                    if let Some(ref mut tv) = self.state.trace {
                        tv.selected = tv.root.clone().or_else(|| tv.span_order.first().cloned());
                    }
                }
                true
            }
            // 'd' maps to DismissDegraded globally; in History it means
            // delete, which is why History intercepts before shared handling.
            Action::DismissDegraded => {
                if !self.state.history_entries.is_empty() {
                    self.state.history_confirm_delete = true;
                }
                true
            }
            Action::Char('R') => {
                if let Some((trace, _)) =
                    self.state.history_entries.get(self.state.history_selected)
                {
                    let id = trace.id.clone();
                    self.enter_replay(id).await;
                }
                true
            }
            Action::Char('W') => {
                if let Some((trace, _)) =
                    self.state.history_entries.get(self.state.history_selected)
                {
                    let id = trace.id.clone();
                    let agent = trace.agent_id.clone();
                    self.enter_impact(id, agent).await;
                }
                true
            }
            Action::Dismiss if !self.state.show_help => {
                self.state.view_mode = ViewMode::Fleet;
                true
            }
            _ => false,
        }
    }

    fn handle_filter_action(&mut self, action: Action) {
        match action {
            Action::Dismiss => {
                // Esc clears everything: the bar and any applied filter.
                self.state.filter_input = None;
                self.state.filter_applied = None;
            }
            Action::Select => {
                // Enter keeps the filter but closes the bar.
                let buffer = self.state.filter_input.take().unwrap_or_default();
                self.state.filter_applied = if buffer.is_empty() {
                    None
                } else {
                    Some(buffer)
                };
                self.cycle_filter_match(true);
            }
            Action::Char(c) => {
                if let Some(ref mut buffer) = self.state.filter_input {
                    buffer.push(c);
                }
            }
            Action::Backspace => {
                if let Some(ref mut buffer) = self.state.filter_input {
                    buffer.pop();
                }
            }
            Action::DeleteWord | Action::ClearLine => {
                if let Some(ref mut buffer) = self.state.filter_input {
                    edit_buffer(buffer, matches!(action, Action::DeleteWord));
                }
            }
            // Tab cycles matches without closing the bar.
            Action::NextPanel => self.cycle_filter_match(true),
            _ => {}
        }
    }

    /// The filter active for display purposes: the live buffer while the
    /// bar is open, the applied filter after Enter.
    pub fn active_filter(&self) -> Option<&str> {
        self.state
            .filter_input
            .as_deref()
            .or(self.state.filter_applied.as_deref())
            .filter(|f| !f.is_empty())
    }

    /// Moves the tree selection to the next or previous span matching the
    /// active filter, wrapping around.
    fn cycle_filter_match(&mut self, forward: bool) {
        let Some(filter) = self.active_filter().map(str::to_string) else {
            return;
        };
        let Some(tv) = self.state.trace.as_mut() else {
            return;
        };
        let matches: Vec<usize> = tv
            .span_order
            .iter()
            .enumerate()
            .filter(|(_, id)| span_matches_filter(tv.spans.get(*id), tv.names.get(*id), &filter))
            .map(|(i, _)| i)
            .collect();
        if matches.is_empty() {
            return;
        }
        let current = tv
            .selected
            .as_ref()
            .and_then(|id| tv.span_order.iter().position(|s| s == id));
        let next = match (current, forward) {
            (Some(c), true) => matches.iter().find(|&&i| i > c).or(matches.first()),
            (Some(c), false) => matches.iter().rev().find(|&&i| i < c).or(matches.last()),
            (None, true) => matches.first(),
            (None, false) => matches.last(),
        };
        if let Some(&idx) = next {
            tv.selected = Some(tv.span_order[idx].clone());
        }
    }

    async fn handle_note_action(&mut self, action: Action) {
        match action {
            Action::Dismiss => {
                self.state.note_input = None;
            }
            Action::Char(c) => {
                if let Some((_, ref mut buffer)) = self.state.note_input {
                    buffer.push(c);
                }
            }
            Action::Backspace => {
                if let Some((_, ref mut buffer)) = self.state.note_input {
                    buffer.pop();
                }
            }
            Action::DeleteWord | Action::ClearLine => {
                if let Some((_, ref mut buffer)) = self.state.note_input {
                    edit_buffer(buffer, matches!(action, Action::DeleteWord));
                }
            }
            Action::Select => {
                let Some((span_id, content)) = self.state.note_input.take() else {
                    return;
                };
                if content.is_empty() {
                    return;
                }
                let now_ms = current_ms();
                let note = reeve_model::entity::SpanNote {
                    id: format!("note-{now_ms:x}"),
                    span_id: span_id.clone(),
                    content: content.clone(),
                    created_at: now_ms,
                };
                if let Err(e) = self.warm.save_span_note(note).await {
                    tracing::warn!(error = %e, "failed to save span note");
                    return;
                }
                if let Some(ref mut tv) = self.state.trace {
                    tv.notes.insert(span_id, content);
                }
                self.state.toast("note saved");
            }
            _ => {}
        }
    }

    /// Static palette commands. Agent- and theme-parameterized entries are
    /// generated against live names at match time.
    const PALETTE_COMMANDS: &'static [&'static str] =
        &["pause all", "resume all", "kill all", "replay last"];

    /// The commands matching the current palette buffer, in display order.
    pub fn palette_matches(&self) -> Vec<String> {
        let Some(buffer) = self.state.palette.as_deref() else {
            return Vec::new();
        };
        let mut candidates: Vec<String> = Self::PALETTE_COMMANDS
            .iter()
            .map(|c| c.to_string())
            .collect();
        for state in self.state.agents.values() {
            candidates.push(format!("pause agent {}", state.agent.name));
            candidates.push(format!("resume agent {}", state.agent.name));
        }
        for theme in crate::theme::BUILTIN_THEMES {
            candidates.push(format!("theme {theme}"));
        }
        candidates.retain(|c| c.starts_with(buffer));
        candidates
    }

    async fn handle_palette_action(&mut self, action: Action) {
        if self.state.palette_confirm_kill {
            if let Action::Char('y') = action {
                let ids: Vec<AgentId> = self.state.agents.keys().cloned().collect();
                for id in ids {
                    self.dispatch_command(id, CommandType::Kill).await;
                }
            }
            self.state.palette_confirm_kill = false;
            self.state.palette = None;
            return;
        }
        match action {
            Action::Dismiss => {
                self.state.palette = None;
            }
            Action::Char(c) => {
                if let Some(ref mut buffer) = self.state.palette {
                    buffer.push(c);
                    self.state.palette_match = 0;
                }
            }
            Action::Backspace => {
                if let Some(ref mut buffer) = self.state.palette {
                    buffer.pop();
                    self.state.palette_match = 0;
                }
            }
            Action::DeleteWord | Action::ClearLine => {
                if let Some(ref mut buffer) = self.state.palette {
                    edit_buffer(buffer, matches!(action, Action::DeleteWord));
                    self.state.palette_match = 0;
                }
            }
            // Tab cycles the completion matches.
            Action::NextPanel => {
                let count = self.palette_matches().len();
                if count > 0 {
                    self.state.palette_match = (self.state.palette_match + 1) % count;
                }
            }
            Action::Select => {
                let matches = self.palette_matches();
                let chosen = matches
                    .get(self.state.palette_match)
                    .cloned()
                    .or_else(|| self.state.palette.clone());
                if let Some(command) = chosen {
                    self.execute_palette_command(&command).await;
                }
            }
            _ => {}
        }
    }

    async fn execute_palette_command(&mut self, command: &str) {
        match command {
            "kill all" => {
                // The one palette command that cannot be walked back.
                self.state.palette_confirm_kill = true;
                return;
            }
            "pause all" => {
                let ids: Vec<AgentId> = self
                    .state
                    .agents
                    .keys()
                    .filter(|id| !self.dispatcher.is_paused(id))
                    .cloned()
                    .collect();
                let count = ids.len();
                for id in ids {
                    self.dispatch_command(id, CommandType::Pause).await;
                }
                self.state
                    .toast(format!("pause dispatched to {count} agents"));
            }
            "resume all" => {
                let ids: Vec<AgentId> = self
                    .state
                    .agents
                    .keys()
                    .filter(|id| self.dispatcher.is_paused(id))
                    .cloned()
                    .collect();
                for id in ids {
                    self.dispatch_command(id, CommandType::Resume).await;
                }
            }
            "replay last" => {
                if let Ok(entries) = self.warm.list_history(None, 1).await {
                    if let Some((trace, _)) = entries.first() {
                        let id = trace.id.clone();
                        self.state.palette = None;
                        self.enter_replay(id).await;
                        return;
                    }
                }
            }
            other => {
                if let Some(name) = other.strip_prefix("theme ") {
                    if crate::theme::BUILTIN_THEMES.contains(&name) {
                        self.state.pending_theme = Some(name.to_string());
                        self.state.toast(format!("theme: {name}"));
                    }
                } else if let Some(name) = other.strip_prefix("pause agent ") {
                    if let Some(id) = self.agent_id_by_name(name) {
                        self.dispatch_command(id, CommandType::Pause).await;
                    }
                } else if let Some(name) = other.strip_prefix("resume agent ") {
                    if let Some(id) = self.agent_id_by_name(name) {
                        self.dispatch_command(id, CommandType::Resume).await;
                    }
                }
            }
        }
        self.state.palette = None;
    }

    /// Writes the loaded trace as JSON next to the working directory:
    /// trace, spans, evaluations, commands, and notes. Span content rides
    /// along when the privacy tier captured it, the same visibility the
    /// cockpit itself has.
    async fn export_trace(&self, trace_id: &TraceId) -> Result<String, String> {
        let trace = self
            .warm
            .get_trace(trace_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("trace not found")?;
        let spans = self
            .warm
            .list_spans_for_trace(trace_id)
            .await
            .map_err(|e| e.to_string())?;
        let evaluations = self
            .warm
            .list_evaluations_for_trace(trace_id)
            .await
            .map_err(|e| e.to_string())?;
        let commands = self
            .warm
            .list_commands_for_trace(trace_id)
            .await
            .map_err(|e| e.to_string())?;
        let notes = self
            .warm
            .span_notes_for_trace(trace_id)
            .await
            .map_err(|e| e.to_string())?;
        let content = self
            .warm
            .span_content_for_trace(trace_id)
            .await
            .map_err(|e| e.to_string())?;

        let doc = serde_json::json!({
            "trace": trace,
            "spans": spans,
            "evaluations": evaluations,
            "commands": commands,
            "notes": notes.values().collect::<Vec<_>>(),
            "span_content": content,
        });
        let short: String = trace_id.as_str().chars().take(8).collect();
        let path = format!("reeve-export-{short}.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        Ok(path)
    }

    fn agent_id_by_name(&self, name: &str) -> Option<AgentId> {
        self.state
            .agents
            .iter()
            .find(|(_, s)| s.agent.name == name)
            .map(|(id, _)| id.clone())
    }

    /// Applies a resolved mouse target to the state. Selection mirrors what
    /// the equivalent keys do; a click on the already-selected span folds
    /// it, standing in for double-click without timing state.
    pub async fn apply_mouse_target(&mut self, target: MouseTarget) {
        match target {
            MouseTarget::SelectAgent(idx) => {
                if idx < self.state.agents.len() {
                    self.state.selected_agent = Some(idx);
                    self.acknowledge_sustained();
                    self.load_trace_for_selected().await;
                }
            }
            MouseTarget::SelectSpan(span_id) => {
                if let Some(tv) = self.state.center_view_mut() {
                    tv.selected = Some(span_id);
                }
            }
            MouseTarget::ToggleSpan(span_id) => {
                if let Some(tv) = self.state.center_view_mut() {
                    // A live view has no root to re-flatten from; its
                    // rows follow arrival order, so folding waits until
                    // the trace completes.
                    if tv.root.is_none() {
                        tv.selected = Some(span_id);
                        return;
                    }
                    if tv.collapsed.contains(&span_id) {
                        tv.collapsed.remove(&span_id);
                    } else {
                        tv.collapsed.insert(span_id);
                    }
                    tv.span_order = tv
                        .root
                        .as_ref()
                        .map(|r| flatten_tree(r, &tv.children, &tv.collapsed))
                        .unwrap_or_default();
                }
            }
            MouseTarget::SelectHistoryRow(idx) => {
                if idx < self.state.history_entries.len() {
                    self.state.history_selected = idx;
                }
            }
            MouseTarget::ScrollPanel { center, up, .. } => {
                if center {
                    if let Some(ref mut tv) = self.state.trace {
                        tv.scroll = if up {
                            tv.scroll.saturating_sub(1)
                        } else {
                            tv.scroll + 1
                        };
                    }
                } else {
                    self.state.streaming.auto_scroll = false;
                    self.state.streaming.scroll = if up {
                        self.state.streaming.scroll.saturating_sub(1)
                    } else {
                        self.state.streaming.scroll + 1
                    };
                }
            }
            MouseTarget::Seek(fraction) => {
                if let Some(ref mut replay) = self.state.replay {
                    replay.seek(fraction);
                    self.rebuild_replay_view();
                }
            }
            MouseTarget::None => {}
        }
    }

    /// Opens the impact view for the selected trace's intervention. A trace
    /// without an applied command, or without traces on both sides of it in
    /// the agent's history, silently stays in History: there is nothing to
    /// compare yet.
    async fn enter_impact(&mut self, trace_id: TraceId, agent_id: AgentId) {
        let commands = self
            .warm
            .list_commands_for_trace(&trace_id)
            .await
            .unwrap_or_default();
        let Some(command) = commands
            .iter()
            .find(|c| c.status == CommandStatus::Applied)
            .or(commands.first())
        else {
            return;
        };
        let tag = match &command.command_type {
            CommandType::Pause => "pause",
            CommandType::Resume => "resume",
            CommandType::Kill => "kill",
            CommandType::Redirect { .. } => "redirect",
            CommandType::InjectContext { .. } => "inject_context",
        };

        // The agent's history chronologically, so pre and post read
        // left to right.
        let mut history = match self.warm.list_history(Some(&agent_id), 200).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = %e, "failed to load history for impact view");
                return;
            }
        };
        history.reverse();
        let Some(idx) = history.iter().position(|(t, _)| t.id == trace_id) else {
            return;
        };
        self.state.impact = ImpactState::build(&history, idx, tag.to_string());
    }

    /// Loads a trace's full recorded timeline and starts replaying it.
    async fn enter_replay(&mut self, trace_id: TraceId) {
        let spans = match self.warm.list_spans_for_trace(&trace_id).await {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => return,
            Err(e) => {
                tracing::warn!(error = %e, "failed to load spans for replay");
                return;
            }
        };
        let evals = self
            .warm
            .list_evaluations_for_trace(&trace_id)
            .await
            .unwrap_or_default();
        let commands = self
            .warm
            .list_commands_for_trace(&trace_id)
            .await
            .unwrap_or_default();
        let span_content = self
            .warm
            .span_content_for_trace(&trace_id)
            .await
            .unwrap_or_default();
        let notes = self
            .warm
            .span_notes_for_trace(&trace_id)
            .await
            .map(|m| m.into_iter().map(|(k, n)| (k, n.content)).collect())
            .unwrap_or_default();
        let mut replay = ReplayState::new(trace_id, spans, evals, commands);
        replay.span_content = span_content;
        replay.notes = notes;
        self.state.replay = Some(replay);
        self.rebuild_replay_view();
    }

    /// Returns true when the action was consumed by replay.
    fn handle_replay_action(&mut self, action: &Action) -> bool {
        let Some(replay) = self.state.replay.as_mut() else {
            return false;
        };
        match action {
            Action::Char(' ') => replay.toggle_play(),
            // h/l are PrevPanel/NextPanel in normal mode; in replay they
            // step the timeline span by span.
            Action::NextPanel => replay.step(true),
            Action::PrevPanel => replay.step(false),
            Action::Char('>') => replay.cycle_speed(true),
            Action::Char('<') => replay.cycle_speed(false),
            Action::Char('0') => replay.reset_speed(),
            Action::Char('I') => replay.jump_to_marker(true),
            // Shift+I arrives as 'I'; plain 'i' maps to OverlayOpen, which
            // replay repurposes as the backward marker jump.
            Action::OverlayOpen => replay.jump_to_marker(false),
            Action::Dismiss => {
                self.state.replay = None;
                self.state.streaming.content.clear();
                return true;
            }
            Action::Quit => {
                self.should_quit = true;
                return true;
            }
            _ => return true, // swallow everything else; replay owns the keys
        }
        self.rebuild_replay_view();
        true
    }

    /// Called every render tick: advances the virtual clock while playing
    /// and rebuilds the visible state when new events emitted.
    pub fn advance_replay(&mut self, wall_ms: f64) {
        let advanced = match self.state.replay.as_mut() {
            Some(r) => r.tick(wall_ms),
            None => return,
        };
        if advanced {
            self.rebuild_replay_view();
        }
    }

    /// Rebuilds the trace view, quality rows, gauge, and streaming box from
    /// the replay's emitted prefix. Rebuilding from scratch each time keeps
    /// one source of truth for visibility; warm-store traces are small
    /// enough that this is well under a render tick.
    fn rebuild_replay_view(&mut self) {
        let Some(replay) = self.state.replay.as_ref() else {
            return;
        };

        let mut span_map: HashMap<SpanId, InternalSpan> = HashMap::new();
        let mut children: HashMap<SpanId, Vec<SpanId>> = HashMap::new();
        let mut names: HashMap<SpanId, String> = HashMap::new();
        let mut root: Option<SpanId> = None;
        let mut metric_scores: Vec<MetricScore> = Vec::new();
        let mut latest_llm_span: Option<SpanId> = None;

        for event in replay.emitted() {
            match event {
                ReplayEvent::Span(span) => {
                    if let Some(ref pid) = span.parent_id {
                        children
                            .entry(pid.clone())
                            .or_default()
                            .push(span.id.clone());
                    } else if root.is_none() {
                        root = Some(span.id.clone());
                    }
                    names.insert(span.id.clone(), span.operation.clone());
                    span_map.insert(span.id.clone(), (**span).clone());
                    if span.operation.starts_with("gen_ai.chat") {
                        latest_llm_span = Some(span.id.clone());
                    }
                }
                ReplayEvent::Eval(eval) => {
                    metric_scores.retain(|m| m.name != eval.metric);
                    metric_scores.push(MetricScore {
                        name: eval.metric.clone(),
                        score: eval.score,
                        confidence: None,
                    });
                }
                ReplayEvent::Command { .. } => {}
            }
        }

        let collapsed = HashSet::new();
        let mut span_order = root
            .as_ref()
            .map(|r| flatten_tree(r, &children, &collapsed))
            .unwrap_or_default();
        // Spans arrive leaves-first and the root arrives last, so mid-replay
        // most spans are orphans: not yet reachable from any root. They must
        // still render, in arrival order, exactly as the live view shows
        // spans awaiting their parent.
        let reachable: HashSet<&SpanId> = span_order.iter().collect();
        let orphans: Vec<SpanId> = replay
            .emitted()
            .iter()
            .filter_map(|e| match e {
                ReplayEvent::Span(s) if !reachable.contains(&s.id) => Some(s.id.clone()),
                _ => None,
            })
            .collect();
        span_order.extend(orphans.iter().cloned());

        // The gauge replays through the same arithmetic the engine used
        // live; scoring lives in reeve-model for exactly this reuse.
        let score_map: HashMap<&str, f64> = metric_scores
            .iter()
            .map(|m| (m.name.as_str(), m.score))
            .collect();
        self.state.health_score = reeve_model::scoring::compute(&score_map).map(|hs| hs.value);
        self.state.metric_scores = metric_scores;

        // The latest replayed LLM span's captured content shows in the
        // streaming box; a tier 1 recording shows the honest notice instead.
        self.state.streaming.content = match &latest_llm_span {
            Some(span_id) => replay
                .span_content
                .get(span_id.as_str())
                .cloned()
                .unwrap_or_else(|| {
                    "content was not captured for this trace (privacy tier 1)".to_string()
                }),
            None => String::new(),
        };

        self.state.trace = Some(TraceView {
            trace_id: replay.trace_id.clone(),
            root,
            spans: span_map,
            children,
            names,
            span_order,
            scroll: 0,
            selected: None,
            collapsed,
            span_health_scores: HashMap::new(),
            outcome_lines: Vec::new(),
            orphans,
            notes: replay
                .notes
                .iter()
                .map(|(k, v)| (SpanId::from(k.as_str()), v.clone()))
                .collect(),
        });
    }

    /// Opens Focus view on the selected agent: loads its recent trace
    /// history newest-first and shows the newest. Stays in Fleet when no
    /// agent is selected or the agent has no completed traces yet, since a
    /// Focus view with nothing to focus on is just a broken Fleet view.
    async fn enter_focus(&mut self) {
        let Some(agent_id) = self.state.selected_agent_id().cloned() else {
            return;
        };
        let traces = match self.warm.list_recent_traces_for_agent(&agent_id, 50).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "failed to load trace history for focus view");
                return;
            }
        };
        if traces.is_empty() {
            return;
        }
        let first = traces[0].id.clone();
        self.state.focus_traces = traces;
        self.state.focus_selected = 0;
        self.state.view_mode = ViewMode::Focus;
        self.load_trace(first).await;
    }

    async fn focus_step(&mut self, delta: i32) {
        let len = self.state.focus_traces.len();
        if len == 0 {
            return;
        }
        let next = (self.state.focus_selected as i32 + delta).clamp(0, len as i32 - 1) as usize;
        if next == self.state.focus_selected {
            return;
        }
        self.state.focus_selected = next;
        let trace_id = self.state.focus_traces[next].id.clone();
        self.load_trace(trace_id).await;
    }

    async fn load_trace_for_selected(&mut self) {
        let trace_id = self
            .state
            .selected_agent
            .and_then(|i| self.state.agents.get_index(i))
            .and_then(|(_, s)| s.last_trace_id.clone());
        if let Some(tid) = trace_id {
            self.load_trace(tid).await;
        }
    }

    /// Selecting an agent acknowledges its sustained alert.
    fn acknowledge_sustained(&mut self) {
        if let Some(id) = self.state.selected_agent_id().cloned() {
            self.state.sustained_alerts.remove(&id);
        }
    }

    fn move_selection(&mut self, delta: i32) {
        match self.state.panel_focus {
            PanelFocus::Left => {
                let len = self.state.agents.len();
                if len == 0 {
                    return;
                }
                let current = self.state.selected_agent.unwrap_or(0) as i32;
                let next = (current + delta).rem_euclid(len as i32) as usize;
                self.state.selected_agent = Some(next);
                self.acknowledge_sustained();
            }
            PanelFocus::Center => {}
            PanelFocus::Right => {}
        }
    }
}

fn flatten_tree(
    root: &SpanId,
    children: &HashMap<SpanId, Vec<SpanId>>,
    collapsed: &HashSet<SpanId>,
) -> Vec<SpanId> {
    let mut order = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(id) = stack.pop() {
        order.push(id.clone());
        if collapsed.contains(&id) {
            continue;
        }
        if let Some(kids) = children.get(&id) {
            for kid in kids.iter().rev() {
                stack.push(kid.clone());
            }
        }
    }
    order
}

/// Whether a span matches the filter text: substring match against the
/// operation name and the tool name attribute, case-insensitive.
pub fn span_matches_filter(
    span: Option<&InternalSpan>,
    name: Option<&String>,
    filter: &str,
) -> bool {
    let needle = filter.to_lowercase();
    if let Some(n) = name {
        if n.to_lowercase().contains(&needle) {
            return true;
        }
    }
    if let Some(s) = span {
        if s.operation.to_lowercase().contains(&needle) {
            return true;
        }
        if let Some(tool) = s
            .attributes
            .get("gen_ai.tool.name")
            .and_then(|v| v.as_str())
        {
            if tool.to_lowercase().contains(&needle) {
                return true;
            }
        }
    }
    false
}

/// What a health-score change means for an agent's sustained alert:
/// Some(true) starts the pulse, Some(false) clears it, None leaves it.
/// Only a downward band crossing on an unselected agent starts one; any
/// non-worsening update clears, so a recovered agent stops pulsing even
/// if it recovered while the developer was looking elsewhere. A downward
/// crossing on the selected agent neither starts nor clears: the developer
/// is already looking at it, and the one-tick flash covers the change.
fn sustained_transition(prev: f64, score: f64, is_selected: bool) -> Option<bool> {
    if band_rank(score) < band_rank(prev) {
        if is_selected { None } else { Some(true) }
    } else {
        Some(false)
    }
}

/// Readline-style buffer editing shared by every text input: Ctrl+W
/// deletes the last word, Ctrl+U clears the line.
fn edit_buffer(buffer: &mut String, word_only: bool) {
    if !word_only {
        buffer.clear();
        return;
    }
    while buffer.ends_with(' ') {
        buffer.pop();
    }
    while let Some(c) = buffer.pop() {
        if c == ' ' {
            buffer.push(' ');
            break;
        }
    }
}

/// Alert stacking order: the weight of the command the firing rule
/// carries. Lower ranks stack higher in ALERTS.
fn alert_severity_rank(command_type: &str) -> u8 {
    match command_type {
        "kill" => 0,
        "pause" => 1,
        "redirect" => 2,
        "inject_context" => 3,
        _ => 4,
    }
}

/// Health band as an ordinal so crossings compare cleanly: 2 healthy,
/// 1 caution, 0 critical.
fn band_rank(score: f64) -> u8 {
    if score >= 80.0 {
        2
    } else if score >= 50.0 {
        1
    } else {
        0
    }
}

fn current_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn suggestion_for_rule(rule_id: &str) -> Option<SuggestedIntervention> {
    match rule_id {
        "builtin_loop_detected" => Some(SuggestedIntervention {
            command: OverlayCommand::Redirect,
            text: "You are in a loop. Try a different approach.".to_string(),
        }),
        "builtin_high_cost" | "builtin_predicted_cost" => Some(SuggestedIntervention {
            command: OverlayCommand::Redirect,
            text: "Please summarize your findings and stop.".to_string(),
        }),
        _ => None,
    }
}

fn suggestion_to_command_type(s: SuggestedIntervention) -> CommandType {
    match s.command {
        OverlayCommand::Redirect => CommandType::Redirect {
            instruction: s.text,
        },
        OverlayCommand::InjectContext => CommandType::InjectContext { context: s.text },
        OverlayCommand::Pause => CommandType::Pause,
        OverlayCommand::Kill => CommandType::Kill,
    }
}

/// Maps a `PolicyAlert` command type string to a dispatchable command. The
/// strings here must match what the engine's `command_type_str` emits in
/// `reeve-engine/src/policy/mod.rs`; the two crates share no type for this,
/// only the wire string. A mismatch is not an error the developer can see:
/// the confirmation modal accepts the keypress and nothing happens.
fn confirmation_command_type(command_type: &str, description: &str) -> Option<CommandType> {
    match command_type {
        "pause" => Some(CommandType::Pause),
        "resume" => Some(CommandType::Resume),
        "kill" => Some(CommandType::Kill),
        "redirect" => Some(CommandType::Redirect {
            instruction: description.to_string(),
        }),
        "inject_context" => Some(CommandType::InjectContext {
            context: description.to_string(),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins this matcher to the exact strings `command_type_str` in
    /// reeve-engine emits. If a variant is added or renamed there, this
    /// test is the tripwire; without it the confirmation flow fails as a
    /// silent no-op, which is how the original mismatch shipped.
    #[test]
    fn confirmation_matches_every_engine_command_string() {
        assert!(matches!(
            confirmation_command_type("pause", ""),
            Some(CommandType::Pause)
        ));
        assert!(matches!(
            confirmation_command_type("resume", ""),
            Some(CommandType::Resume)
        ));
        assert!(matches!(
            confirmation_command_type("kill", ""),
            Some(CommandType::Kill)
        ));
        match confirmation_command_type("redirect", "go elsewhere") {
            Some(CommandType::Redirect { instruction }) => {
                assert_eq!(instruction, "go elsewhere")
            }
            other => panic!("redirect must map with its instruction, got {other:?}"),
        }
        match confirmation_command_type("inject_context", "extra facts") {
            Some(CommandType::InjectContext { context }) => assert_eq!(context, "extra facts"),
            other => panic!("inject_context must map with its context, got {other:?}"),
        }
    }

    #[test]
    fn a_side_call_cannot_stomp_the_main_stream() {
        // The live-session shape: a title-generation side call streams
        // concurrently with the main response and used to overwrite it
        // in the shared box (the operator watched their own prompt echo
        // replace the generation they were reading).
        let agent: AgentId = "claude-cli:proxy".into();
        let mut s = StreamingState::default();

        s.update("main".into(), agent.clone(), "the fix is".into());
        s.update("side".into(), agent.clone(), "commit this".into());
        s.update("main".into(), agent.clone(), "the fix is ready".into());
        assert_eq!(
            s.display_for(&agent),
            "the fix is ready",
            "the older main stream owns the box"
        );

        // The side call finishes first: the box does not adopt its text.
        s.finish(&"side".into());
        assert_eq!(s.display_for(&agent), "the fix is ready");

        // The main stream finishes: its final text holds between turns.
        s.finish(&"main".into());
        assert_eq!(
            s.display_for(&agent),
            "the fix is ready",
            "the displayed stream's final text lingers, not the side call's"
        );
    }

    #[test]
    fn suggestions_check_the_target_capabilities() {
        // The #192 shape: policy suggested pause for a proxy agent,
        // whose path has no pause by design; confirming dead-lettered.
        let proxy_caps = vec![
            "redirect".to_string(),
            "inject_context".to_string(),
            "kill".to_string(),
        ];
        assert!(!suggestion_supported(&proxy_caps, "pause"));
        assert!(suggestion_supported(&proxy_caps, "redirect"));
        assert!(suggestion_supported(&proxy_caps, "kill"));

        let sdk_caps = vec!["pause".to_string(), "redirect".to_string()];
        assert!(suggestion_supported(&sdk_caps, "pause"));
        assert!(
            suggestion_supported(&sdk_caps, "resume"),
            "resume rides the pause capability"
        );
        assert!(
            !suggestion_supported(&[], "redirect"),
            "no capabilities, no dispatchable suggestions"
        );
    }

    #[test]
    fn side_call_stubs_never_steal_the_panel() {
        // The #201 shape: a 100-span build turn loads, then a trailing
        // 2-span title-generation stub completes and used to mask it.
        assert!(!completed_trace_takes_panel(2, Some(100)));
        assert!(completed_trace_takes_panel(100, Some(2)), "big beats stub");
        assert!(
            completed_trace_takes_panel(2, Some(2)),
            "stub replaces stub"
        );
        assert!(completed_trace_takes_panel(2, None), "anything fills empty");
        assert!(
            completed_trace_takes_panel(100, Some(50)),
            "big replaces big"
        );
    }

    #[test]
    fn a_side_call_cannot_destroy_the_main_live_view() {
        // The real-session shape behind #190: side calls run concurrent
        // conversations under the SAME agent. The old agent-keyed slot
        // let every side-call span replace the main turn's view, and
        // the panel went blank when the side call retired.
        let agent: AgentId = "claude-cli:proxy".into();
        let mut lt = LiveTurns::default();

        lt.observe(
            agent.clone(),
            "main-trace".into(),
            "chat-1".into(),
            Some("root".into()),
            "gen_ai.chat".into(),
        );
        lt.observe(
            agent.clone(),
            "side-trace".into(),
            "side-chat".into(),
            Some("side-root".into()),
            "gen_ai.chat".into(),
        );
        assert_eq!(
            lt.oldest_for(&agent).map(|v| v.trace_id.as_str()),
            Some("main-trace"),
            "the older main turn owns the panel"
        );

        lt.retire(&"side-trace".into());
        assert_eq!(
            lt.oldest_for(&agent).map(|v| v.trace_id.as_str()),
            Some("main-trace"),
            "the side call retiring leaves the main view standing"
        );

        lt.retire(&"main-trace".into());
        assert!(
            lt.oldest_for(&agent).is_none(),
            "all turns closed: the loaded trace takes the panel back"
        );
    }

    #[test]
    fn a_live_turn_folds_observed_spans_into_a_tree() {
        // The interactive-session shape: chats parent to a root that has
        // not arrived, tools parent to their chat, the root lands last.
        let mut v = TraceView::empty("t1".into());
        v.observe("chat-1".into(), Some("root".into()), "gen_ai.chat".into());
        v.observe(
            "tool-1".into(),
            Some("chat-1".into()),
            "gen_ai.tool:Bash".into(),
        );
        v.observe("chat-2".into(), Some("root".into()), "gen_ai.chat".into());

        assert_eq!(
            v.orphans,
            vec![SpanId::from("chat-1"), SpanId::from("chat-2")],
            "chats are top-level rows while the root is unarrived"
        );
        assert_eq!(
            v.children.get(&SpanId::from("chat-1")),
            Some(&vec![SpanId::from("tool-1")]),
            "the tool nests under the chat that requested it"
        );
        assert!(v.root.is_none(), "no root while the turn is open");

        v.observe("root".into(), None, "agent.turn.1".into());
        assert_eq!(
            v.root,
            Some(SpanId::from("root")),
            "the arriving root completes the shape"
        );
    }

    #[test]
    fn edit_buffer_deletes_word_and_clears_line() {
        let mut b = "pause agent research-bot".to_string();
        edit_buffer(&mut b, true);
        assert_eq!(b, "pause agent ", "Ctrl+W removes the last word only");
        edit_buffer(&mut b, true);
        assert_eq!(b, "pause ", "trailing space then the next word");
        edit_buffer(&mut b, false);
        assert_eq!(b, "", "Ctrl+U clears everything");
        edit_buffer(&mut b, true);
        assert_eq!(b, "", "editing an empty buffer holds");
    }

    #[test]
    fn alert_severity_ranks_kill_above_all() {
        assert!(alert_severity_rank("kill") < alert_severity_rank("pause"));
        assert!(alert_severity_rank("pause") < alert_severity_rank("redirect"));
        assert!(alert_severity_rank("redirect") < alert_severity_rank("inject_context"));
        assert!(alert_severity_rank("inject_context") < alert_severity_rank("unknown"));
    }

    #[test]
    fn sustained_pulse_starts_on_downward_crossing_of_unselected_agent() {
        assert_eq!(
            sustained_transition(85.0, 45.0, false),
            Some(true),
            "healthy to critical on an unselected agent starts the pulse"
        );
        assert_eq!(
            sustained_transition(85.0, 75.0, false),
            Some(true),
            "healthy to caution counts as a crossing"
        );
        assert_eq!(
            sustained_transition(85.0, 45.0, true),
            None,
            "the selected agent gets the flash, not a sustained pulse"
        );
        assert_eq!(
            sustained_transition(85.0, 82.0, false),
            Some(false),
            "movement inside a band clears rather than pulses"
        );
        assert_eq!(
            sustained_transition(45.0, 85.0, false),
            Some(false),
            "recovery clears the pulse"
        );
    }

    #[test]
    fn confirmation_rejects_unknown_and_capitalized_strings() {
        assert!(confirmation_command_type("Pause", "").is_none());
        assert!(confirmation_command_type("PAUSE", "").is_none());
        assert!(confirmation_command_type("", "").is_none());
        assert!(confirmation_command_type("shutdown", "").is_none());
    }
}
