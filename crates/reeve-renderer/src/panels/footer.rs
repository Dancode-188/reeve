use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

pub fn render(frame: &mut Frame, area: Rect, theme: &Theme, right_hidden: bool) {
    if area.height == 0 {
        return;
    }

    let key = Style::default().fg(theme.text());
    let sep = Style::default().fg(theme.subtext());

    let mut spans = vec![
        Span::raw(" "),
        Span::styled("j/k", key),
        Span::styled(" navigate", sep),
        Span::styled("   h/l", key),
        Span::styled(" switch panel", sep),
        Span::styled("   Enter", key),
        Span::styled(" expand/fold", sep),
        Span::styled("   ?", key),
        Span::styled(" help", sep),
        Span::styled("   q", key),
        Span::styled(" quit", sep),
    ];

    if right_hidden {
        spans.push(Span::styled("   ", sep));
        spans.push(Span::styled(
            "[SPAN panel hidden, widen terminal to show]",
            Style::default().fg(theme.health_warn()),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}
