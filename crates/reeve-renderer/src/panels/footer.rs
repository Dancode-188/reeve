use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

pub fn render(frame: &mut Frame, area: Rect, theme: &Theme, right_hidden: bool, left_hidden: bool) {
    if area.height == 0 {
        return;
    }

    let key = Style::default().fg(theme.text());
    let sep = Style::default().fg(theme.subtext());
    let warn = Style::default().fg(theme.health_warn());

    let line = if left_hidden {
        // Both panels are hidden — terminal is very narrow, keep hints minimal.
        Line::from(vec![
            Span::raw(" "),
            Span::styled("j/k", key),
            Span::styled(" navigate", sep),
            Span::styled("   ?", key),
            Span::styled(" help", sep),
            Span::styled("   q", key),
            Span::styled(" quit", sep),
            Span::styled("   [AGENTS ◁  SPAN ▷]", warn),
        ])
    } else {
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
            spans.push(Span::styled("   [SPAN ▷]", warn));
        }
        Line::from(spans)
    };

    frame.render_widget(Paragraph::new(line), area);
}
