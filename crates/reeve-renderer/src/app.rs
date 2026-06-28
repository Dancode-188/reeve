use crate::input::Action;
use indexmap::IndexMap;
use reeve_model::entity::agent::Agent;
use reeve_model::entity::span::InternalSpan;
use reeve_model::ids::{AgentId, SpanId, TraceId};
use reeve_model::signal::EngineSignal;
use reeve_storage::warm::WarmStore;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct AgentState {
    pub agent: Agent,
    pub last_trace_id: Option<TraceId>,
    pub trace_count: u32,
    pub total_cost: f64,
    pub cost_history: Vec<f64>,
}

impl AgentState {
    fn new(agent: Agent) -> Self {
        Self {
            agent,
            last_trace_id: None,
            trace_count: 0,
            total_cost: 0.0,
            cost_history: Vec::new(),
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

pub struct AppState {
    pub agents: IndexMap<AgentId, AgentState>,
    pub selected_agent: Option<usize>,
    pub trace: Option<TraceView>,
    pub streaming: StreamingState,
    pub health_score: Option<f64>,
    pub panel_focus: PanelFocus,
    pub errors: Vec<String>,
}

pub struct App {
    pub engine_rx: broadcast::Receiver<EngineSignal>,
    pub warm: Arc<WarmStore>,
    pub should_quit: bool,
    pub state: AppState,
}

impl App {
    pub async fn new(engine_rx: broadcast::Receiver<EngineSignal>, warm: Arc<WarmStore>) -> Self {
        let mut agents: IndexMap<AgentId, AgentState> = IndexMap::new();
        match warm.list_agents().await {
            Ok(list) => {
                for mut agent in list {
                    // No agent is actively streaming at process startup.
                    // Warm store status reflects the last session and may be stale.
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
            engine_rx,
            warm,
            should_quit: false,
            state: AppState {
                agents,
                selected_agent,
                trace: None,
                streaming: StreamingState::default(),
                health_score: None,
                panel_focus: PanelFocus::default(),
                errors: Vec::new(),
            },
        }
    }

    pub async fn handle_signal(&mut self, signal: EngineSignal) {
        match signal {
            EngineSignal::AgentConnected { agent } => {
                let id = agent.id.clone();
                self.state
                    .agents
                    .entry(id)
                    .or_insert_with(|| AgentState::new(agent));
                if self.state.selected_agent.is_none() && !self.state.agents.is_empty() {
                    self.state.selected_agent = Some(0);
                }
            }
            EngineSignal::AgentStatusChanged { agent_id, status } => {
                if let Some(s) = self.state.agents.get_mut(&agent_id) {
                    s.agent.status = status;
                }
            }
            EngineSignal::TraceCompleted {
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
                // Load the trace if the selected agent just completed it.
                let is_selected = self
                    .state
                    .selected_agent
                    .and_then(|i| self.state.agents.get_index(i))
                    .map(|(id, _)| id == &agent_id)
                    .unwrap_or(false);
                if is_selected {
                    self.load_trace(trace_id).await;
                }
            }
            EngineSignal::StreamingUpdate { content, .. } => {
                self.state.streaming.content.push_str(&content);
            }
            EngineSignal::SpanCompleted { .. } => {}
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

                let span_order = root
                    .as_ref()
                    .map(|r| flatten_tree(r, &children))
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
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to load trace spans");
            }
        }
    }

    pub async fn handle_action(&mut self, action: Action) {
        match action {
            Action::Quit => self.should_quit = true,
            Action::MoveUp => match self.state.panel_focus {
                PanelFocus::Left => {
                    self.move_selection(-1);
                    self.load_trace_for_selected().await;
                }
                PanelFocus::Center => self.move_center_selection(-1),
                _ => {}
            },
            Action::MoveDown => match self.state.panel_focus {
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
            Action::Select => {}
            Action::Resize(_, _) => {}
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

fn flatten_tree(root: &SpanId, children: &HashMap<SpanId, Vec<SpanId>>) -> Vec<SpanId> {
    let mut order = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(id) = stack.pop() {
        order.push(id.clone());
        if let Some(kids) = children.get(&id) {
            for kid in kids.iter().rev() {
                stack.push(kid.clone());
            }
        }
    }
    order
}
