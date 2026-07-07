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
    /// Pre-formatted outcome annotations loaded from the warm store on trace load.
    pub outcome_lines: Vec<OutcomeLine>,
    /// Spans not reachable from the root because their parent has not
    /// arrived yet. Rendered as flat rows awaiting the parent. Populated
    /// during replay, where arrival order routinely puts children first.
    pub orphans: Vec<SpanId>,
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
    /// Theme name the palette or T selected; the render loop applies it.
    pub pending_theme: Option<String>,
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
                pending_theme: None,
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
                    self.state.active_suggestion = None;
                    self.state.metric_scores.clear();
                    self.state.health_tier2_pending = false;
                    self.load_trace(trace_id).await;
                    self.update_ctx_suggestion();
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
                self.state
                    .flash_targets
                    .insert(FlashTarget::AlertSection, (FlashDirection::Alert, 2));
                if let Some(s) = suggestion_for_rule(&rule_id) {
                    self.state.active_suggestion = Some(s);
                }
                if requires_confirmation {
                    if let Some(agent_id) = self.state.selected_agent_id().cloned() {
                        self.state.pending_confirmation = Some(PendingConfirmation {
                            agent_id,
                            rule_id,
                            description,
                            command_type,
                            auto_confirm_after_secs,
                            arrived_at_ms: current_ms(),
                        });
                    }
                }
            }
            EngineEvent::AgentControlConnected {
                agent_id,
                capabilities,
            } => {
                self.state.agent_capabilities.insert(agent_id, capabilities);
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
                    orphans: Vec::new(),
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
                let caps = self
                    .state
                    .agent_capabilities
                    .get(&agent_id)
                    .cloned()
                    .unwrap_or_default();

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
                    // [k] in overlay = kill
                    Action::VimUp if caps.contains(&"kill".to_string()) => {
                        if let Some(ref mut ov) = self.state.overlay {
                            ov.mode = OverlayMode::KillConfirm;
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
        self.state.palette.is_some()
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
            Action::Select => {
                if let Some((trace, _)) =
                    self.state.history_entries.get(self.state.history_selected)
                {
                    let id = trace.id.clone();
                    self.load_trace(id).await;
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
                for id in ids {
                    self.dispatch_command(id, CommandType::Pause).await;
                }
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
                    self.load_trace_for_selected().await;
                }
            }
            MouseTarget::SelectSpan(span_id) => {
                if let Some(ref mut tv) = self.state.trace {
                    tv.selected = Some(span_id);
                }
            }
            MouseTarget::ToggleSpan(span_id) => {
                if let Some(ref mut tv) = self.state.trace {
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
        let mut replay = ReplayState::new(trace_id, spans, evals, commands);
        replay.span_content = span_content;
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
    fn confirmation_rejects_unknown_and_capitalized_strings() {
        assert!(confirmation_command_type("Pause", "").is_none());
        assert!(confirmation_command_type("PAUSE", "").is_none());
        assert!(confirmation_command_type("", "").is_none());
        assert!(confirmation_command_type("shutdown", "").is_none());
    }
}
