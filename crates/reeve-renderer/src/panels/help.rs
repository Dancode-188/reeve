use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

pub fn render(frame: &mut Frame, area: Rect, theme: &Theme) {
    let popup = centered(48, 14, area);

    let key = Style::default().fg(theme.highlight());
    let desc = Style::default().fg(theme.text());

    let rows: &[(&str, &str)] = &[
        ("j / k", "navigate up/down"),
        ("h / l", "switch panel left/right"),
        ("Tab / Shift-Tab", "switch panel"),
        ("Enter", "expand / fold span"),
        ("PageUp / PageDown", "scroll"),
        ("?", "toggle this overlay"),
        ("Esc", "close overlay"),
        ("q  /  Ctrl-c", "quit"),
    ];

    let mut lines: Vec<Line> = vec![Line::raw("")];
    for (k, d) in rows {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{:<18}", k), key),
            Span::styled(*d, desc),
        ]));
    }
    lines.push(Line::raw(""));

    let block = Block::default()
        .title(" KEYS ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focused()));

    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

fn centered(width: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height),
        Constraint::Fill(1),
    ])
    .split(area);

    Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(width),
        Constraint::Fill(1),
    ])
    .split(vertical[1])[1]
}
