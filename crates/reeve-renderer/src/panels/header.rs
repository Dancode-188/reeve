use crate::app::AppState;
use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if area.height == 0 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Fill(1), Constraint::Fill(1)])
        .split(area);

    let title = Paragraph::new(Line::from(vec![
        Span::raw(" "),
        Span::styled(
            "reeve",
            Style::default()
                .fg(theme.highlight())
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    let count = state.agents.len();
    let status_line = if count == 0 {
        Line::from(Span::styled(
            "○ no agents  ",
            Style::default().fg(theme.subtext()),
        ))
    } else {
        let label = if count == 1 { "agent" } else { "agents" };
        Line::from(vec![
            Span::styled("● ", Style::default().fg(theme.health_ok())),
            Span::styled(
                format!("{}  {}  ", count, label),
                Style::default().fg(theme.subtext()),
            ),
        ])
    };
    let status = Paragraph::new(status_line).alignment(Alignment::Right);

    frame.render_widget(title, chunks[0]);
    frame.render_widget(status, chunks[1]);
}
