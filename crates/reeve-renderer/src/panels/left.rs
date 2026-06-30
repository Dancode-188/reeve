use crate::app::{AppState, PanelFocus};
use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem},
};
use reeve_model::entity::agent::AgentStatus;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if area.width == 0 {
        return;
    }

    let focused = state.panel_focus == PanelFocus::Left;

    let (agents_area, alerts_area) = if state.policy_alerts.is_empty() {
        (area, None)
    } else {
        let alert_rows = (state.policy_alerts.len() as u16 + 2).min(5);
        let chunks =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(alert_rows)]).split(area);
        (chunks[0], Some(chunks[1]))
    };

    render_agents(frame, agents_area, state, theme, focused);

    if let Some(alerts_area) = alerts_area {
        render_alerts(frame, alerts_area, state, theme, focused);
    }
}

fn render_agents(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme, focused: bool) {
    let border_style = Style::default().fg(if focused {
        theme.border_focused()
    } else {
        theme.border_idle()
    });

    let block = Block::default()
        .title("AGENTS")
        .borders(Borders::ALL)
        .border_style(border_style);

    let max_name = (area.width as usize).saturating_sub(5);

    let items: Vec<ListItem> = state
        .agents
        .iter()
        .enumerate()
        .map(|(i, (_, agent_state))| {
            let indicator = status_indicator(agent_state.agent.status);
            let indicator_color = match agent_state.agent.status {
                AgentStatus::Running => theme.health_ok(),
                AgentStatus::Idle => theme.subtext(),
                AgentStatus::Paused => theme.health_warn(),
                AgentStatus::Error => theme.health_crit(),
            };

            let is_selected = state.selected_agent == Some(i);
            let name = truncate(&agent_state.agent.name, max_name);

            let name_style = if is_selected {
                Style::default()
                    .fg(theme.background())
                    .bg(theme.highlight())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text())
            };

            ListItem::new(Line::from(vec![
                Span::styled(indicator, Style::default().fg(indicator_color)),
                Span::raw(" "),
                Span::styled(name, name_style),
            ]))
        })
        .collect();

    frame.render_widget(List::new(items).block(block), area);
}

fn render_alerts(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme, focused: bool) {
    let border_style = Style::default().fg(if focused {
        theme.border_focused()
    } else {
        theme.border_idle()
    });

    let block = Block::default()
        .title("ALERTS")
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let max_label = (inner.width as usize).saturating_sub(2);

    let items: Vec<ListItem> = state
        .policy_alerts
        .iter()
        .rev()
        .take(inner.height as usize)
        .map(|(rule_id, _)| {
            let name = rule_id.strip_prefix("builtin_").unwrap_or(rule_id);
            let label = if name.len() > max_label {
                format!("{:.max$}", name, max = max_label)
            } else {
                name.to_string()
            };
            ListItem::new(Line::from(vec![
                Span::styled("\u{26A0} ", Style::default().fg(theme.health_crit())),
                Span::styled(label, Style::default().fg(theme.text())),
            ]))
        })
        .collect();

    frame.render_widget(List::new(items), inner);
}

fn status_indicator(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Running => "●",
        AgentStatus::Idle => "○",
        AgentStatus::Paused => "⏸",
        AgentStatus::Error => "✗",
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{:.max$}", s, max = max.saturating_sub(1)) + "…"
    }
}
