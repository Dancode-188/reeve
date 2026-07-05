use crate::input::Action;
use indexmap::IndexMap;
use reeve_intervention::dispatcher::Dispatcher;
use reeve_model::entity::agent::Agent;
use reeve_model::entity::intervention::{CommandStatus, CommandType, InterventionCommand};
use reeve_model::entity::span::InternalSpan;
use reeve_model::ids::{AgentId, CommandId, SpanId, TraceId};
use reeve_model::signal::{CostTrend, EngineEvent, EvaluationConfidence, IngestionEvent};
use reeve_storage::warm::WarmStore;
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
    pub cost_history: Vec<f64>,
    pub cost_trend: Option<CostTrend>,
}

impl AgentState {
    fn new(agent: Agent) -> Self {
        Self {
            agent,
            last_trace_id: None,
            trace_count: 0,
            total_cost: 0.0,
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
}

pub struct StreamingState {
    pub content: String,
    pub scroll: u16,
    pub auto_scroll: bool,
    pub cursor_tick: u8,
}

impl Default for StreamingState {
    fn default() -> Self {
        Self {
            content: String::new(),
            scroll: 0,
            auto_scroll: true,
            cursor_tick: 0,
        }
    }
}

#[derive(Default, Clone, Copy, PartialEq)]
pub enum PanelFocus {
    #[default]
    Left,
    Center,
    Right,
}

pub struct FatalError {
    pub message: String,
    pub hint: Option<String>,
}

/// What the overlay is currently waiting for.
#[derive(Debug, Clone, PartialEq)]
pub enum OverlayMode {
    Menu,
    TextInput { command: OverlayCommand, buffer: String },
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

pub struct AppState {
    pub agents: IndexMap<AgentId, AgentState>,
    pub selected_agent: Option<usize>,
    pub trace: Option<TraceView>,
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
}

impl AppState {
    /// Decrement all flash TTLs by one tick and remove expired entries.
    pub fn advance_flash(&mut self) {
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
            state: AppState {
                agents,
                selected_agent,
                trace: None,
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
                show_help: false,
                errors: Vec::new(),
                fatal_error: None,
                degraded_dismissed: false,
                agent_capabilities: HashMap::new(),
                pending_commands: HashMap::new(),
                overlay: None,
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
                if let Some(s) = self.state.agents.get_mut(&agent_id) {
                    s.agent.status = status;
                }
            }
            IngestionEvent::TraceCompleted {
                trace_id,
                agent_id,
                span_count: _,
                cost,
            } => {
                if let Some(s) = self.state.agents.get_mut(&agent_id) {
                    s.last_trace_id = Some(trace_id.clone());
                    s.trace_count += 1;
                    s.total_cost += cost;
                    s.cost_history.push(cost);
                    if s.cost_history.len() > 60 {
                        s.cost_history.remove(0);
                    }
                }
                let is_selected = self
                    .state
                    .selected_agent
                    .and_then(|i| self.state.agents.get_index(i))
                    .map(|(id, _)| id == &agent_id)
                    .unwrap_or(false);
                if is_selected {
                    self.state.metric_scores.clear();
                    self.state.health_tier2_pending = false;
                    self.load_trace(trace_id).await;
                }
            }
            IngestionEvent::StreamingUpdate { content, .. } => {
                self.state.streaming.content.push_str(&content);
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
                        name: metric,
                        score,
                        confidence,
                    });
                }
            }
            EngineEvent::PolicyAlert {
                description,
                command_type,
                ..
            } => {
                if self.state.policy_alerts.len() >= 5 {
                    self.state.policy_alerts.pop_front();
                }
                self.state.policy_alerts.push_back(PolicyAlertEntry {
                    description,
                    command_type,
                });
                self.state
                    .flash_targets
                    .insert(FlashTarget::AlertSection, (FlashDirection::Alert, 2));
            }
            EngineEvent::AgentControlConnected {
                agent_id,
                capabilities,
            } => {
                self.state
                    .agent_capabilities
                    .insert(agent_id, capabilities);
            }
            EngineEvent::AgentControlDisconnected { agent_id } => {
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
            Ok(spans) => {
                let mut span_map: HashMap<SpanId, InternalSpan> = HashMap::new();
                let mut children: HashMap<SpanId, Vec<SpanId>> = HashMap::new();
                let mut names: HashMap<SpanId, String> = HashMap::new();
                let mut root: Option<SpanId> = None;

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
                    span_map.insert(span.id.clone(), span);
                }

                let collapsed = HashSet::new();
                let span_order = root
                    .as_ref()
                    .map(|r| flatten_tree(r, &children, &collapsed))
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
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to load trace spans");
            }
        }
    }

    pub async fn handle_action(&mut self, action: Action) {
        // When overlay is open, consume most actions before they reach the cockpit.
        if self.state.overlay.is_some() {
            self.handle_overlay_action(action).await;
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
                    // Clear known degraded state; engine reprobe not yet wired
                    self.state.eval_backend = None;
                    self.state.eval_backend_reason = None;
                    self.state.degraded_dismissed = false;
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
                    self.dispatch_command(agent_id, CommandType::Pause).await;
                }
            }
            Action::Resize(_, _) | Action::Char(_) | Action::Backspace => {}
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
                Action::Select => {
                    let Some(ov) = self.state.overlay.take() else {
                        return;
                    };
                    if let OverlayMode::TextInput { command, buffer } = ov.mode {
                        let cmd_type = match command {
                            OverlayCommand::Redirect => {
                                CommandType::Redirect { instruction: buffer }
                            }
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
                let caps = self
                    .state
                    .agent_capabilities
                    .get(&agent_id)
                    .cloned()
                    .unwrap_or_default();

                match action {
                    Action::Dismiss => {
                        self.state.overlay = None;
                    }
                    // [p] in overlay = pause
                    Action::QuickPause if caps.contains(&"pause".to_string()) => {
                        self.state.overlay = None;
                        self.dispatch_command(agent_id, CommandType::Pause).await;
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
                    // [k] in overlay = kill
                    Action::VimUp if caps.contains(&"kill".to_string()) => {
                        if let Some(ref mut ov) = self.state.overlay {
                            ov.mode = OverlayMode::KillConfirm;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    async fn dispatch_command(&mut self, agent_id: AgentId, command_type: CommandType) {
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
            issued_by: "human".to_string(),
            valid_until_ms: now_ms + 60_000,
        };

        let dispatched = self.dispatcher.dispatch(&agent_id, command).await;
        if dispatched {
            self.state.pending_commands.insert(command_id, agent_id);
        }
    }

    fn move_center_selection(&mut self, delta: i32) {
        let Some(tv) = self.state.trace.as_mut() else {
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

fn current_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
