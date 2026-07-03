use crate::app::AppState;
use crate::panels::left::health_color;
use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
};

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if area.height == 0 {
        return;
    }

    let chrome_style = Style::default().bg(theme.chrome_bg());
    frame.render_widget(Block::default().style(chrome_style), area);

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Fill(1), Constraint::Fill(1)])
        .split(area);

    let title = Paragraph::new(Line::from(vec![
        Span::raw(" "),
        Span::styled(
            "REEVE",
            Style::default()
                .fg(theme.highlight())
                .add_modifier(Modifier::BOLD),
        ),
    ]))
    .style(chrome_style);

    // Right side: selected agent info or agent count
    let right_line = build_right_line(state, theme);
    let right = Paragraph::new(right_line)
        .alignment(Alignment::Right)
        .style(chrome_style);

    frame.render_widget(title, chunks[0]);
    frame.render_widget(right, chunks[1]);
}

fn build_right_line(state: &AppState, theme: &Theme) -> Line<'static> {
    let selected = state
        .selected_agent
        .and_then(|i| state.agents.get_index(i))
        .map(|(_, s)| s);

    match selected {
        Some(agent_state)
            if matches!(
                agent_state.agent.status,
                reeve_model::entity::agent::AgentStatus::Running
                    | reeve_model::entity::agent::AgentStatus::Paused
            ) =>
        {
            let name = agent_state.agent.name.clone();
            let cost_str = format!("${:.3}", agent_state.total_cost);

            let mut spans: Vec<Span<'static>> = vec![
                Span::styled("\u{25CF} ", Style::default().fg(theme.health_ok())),
                Span::styled(name, Style::default().fg(theme.text())),
                Span::raw("  "),
            ];

            if let Some(score) = state.health_score {
                let hcolor = health_color(score, theme);
                let band = if score >= 80.0 {
                    "HEALTHY"
                } else if score >= 50.0 {
                    "CAUTION"
                } else {
                    "CRITICAL"
                };
                spans.push(Span::styled("\u{25C6}", Style::default().fg(hcolor)));
                spans.push(Span::styled(
                    format!("{} {} ", score.round() as u32, band),
                    Style::default().fg(hcolor).add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    "\u{00B7} ",
                    Style::default().fg(theme.subtext()),
                ));
            }

            spans.push(Span::styled(
                format!("{}  ", cost_str),
                Style::default().fg(theme.get("teal")),
            ));

            Line::from(spans)
        }
        _ => {
            let count = state.agents.len();
            if count == 0 {
                Line::from(Span::styled(
                    "\u{25CB} no agents  ",
                    Style::default().fg(theme.subtext()),
                ))
            } else {
                let label = if count == 1 { "agent" } else { "agents" };
                Line::from(vec![
                    Span::styled("\u{25CF} ", Style::default().fg(theme.health_ok())),
                    Span::styled(
                        format!("{}  {}  ", count, label),
                        Style::default().fg(theme.subtext()),
                    ),
                ])
            }
        }
    }
}
