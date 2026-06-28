use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

pub fn render(frame: &mut Frame, area: Rect, theme: &Theme) {
    if area.height == 0 {
        return;
    }

    let key = Style::default().fg(theme.text());
    let sep = Style::default().fg(theme.subtext());

    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled("j/k", key),
        Span::styled(" navigate", sep),
        Span::styled("   Tab", key),
        Span::styled(" switch panel", sep),
        Span::styled("   ?", key),
        Span::styled(" help", sep),
        Span::styled("   q", key),
        Span::styled(" quit", sep),
    ]);

    frame.render_widget(Paragraph::new(line), area);
}
