use crate::app::AppState;
use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let warn = Style::default().fg(theme.health_warn());
    let muted = Style::default().fg(theme.subtext());
    let kb = Style::default()
        .fg(theme.get("blue"))
        .add_modifier(Modifier::BOLD);
    let bg = Style::default().bg(theme.chrome_bg());

    let reason = state
        .eval_backend_reason
        .as_deref()
        .unwrap_or("Ollama not reachable");

    let row1 = Line::from(vec![
        Span::styled(" \u{26A0}  Tier 2 evaluation unavailable", warn),
        Span::styled(format!(" \u{00B7} {}", reason), muted),
    ]);

    let row2 = Line::from(vec![
        Span::raw("   "),
        Span::styled("[r]", kb),
        Span::styled(" retry  ", muted),
        Span::styled("[d]", kb),
        Span::styled(" dim", muted),
    ]);

    frame.render_widget(Paragraph::new(vec![row1, row2]).style(bg), area);
}
