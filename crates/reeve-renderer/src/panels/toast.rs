use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};
use std::collections::VecDeque;

/// Transient notices in the bottom-right corner, stacked newest at the
/// bottom, each fading out as its deadline nears. Never shifts other content.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    toasts: &VecDeque<(String, std::time::Instant)>,
    theme: &Theme,
) {
    if toasts.is_empty() || area.height < 3 {
        return;
    }
    let now = std::time::Instant::now();
    for (i, (text, deadline)) in toasts.iter().rev().enumerate() {
        let width = (text.len() as u16 + 4).min(area.width);
        let row = Rect {
            x: area.x + area.width - width,
            y: area.y + area.height - 2 - i as u16,
            width,
            height: 1,
        };
        if row.y <= area.y {
            break;
        }
        // The last second renders dim: the fade-out.
        let style = if deadline.saturating_duration_since(now).as_millis() < 1000 {
            Style::default().fg(theme.subtext()).bg(theme.surface())
        } else {
            Style::default().fg(theme.text()).bg(theme.surface())
        };
        frame.render_widget(Clear, row);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(format!("  {text}  "), style))),
            row,
        );
    }
}
