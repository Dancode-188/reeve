use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};

/// The span annotation input: one row above the footer, same slot the
/// palette uses. Enter saves, Esc cancels.
pub fn render(frame: &mut Frame, area: Rect, buffer: &str, theme: &Theme) {
    if area.height == 0 {
        return;
    }
    frame.render_widget(Clear, area);
    let line = Line::from(vec![
        Span::styled(
            " note: ",
            Style::default()
                .fg(theme.get("blue"))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            // Reserve room for the label, cursor, and hint so the tail
            // of a long note stays visible.
            super::tail_view(buffer, (area.width as usize).saturating_sub(38)),
            Style::default().fg(theme.text()),
        ),
        Span::styled("\u{258C}", Style::default().fg(theme.text())),
        Span::styled(
            "  [Enter] save  [Esc] cancel",
            Style::default().fg(theme.subtext()),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(theme.surface())),
        area,
    );
}
