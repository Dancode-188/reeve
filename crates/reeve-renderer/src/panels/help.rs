use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

pub fn render(frame: &mut Frame, area: Rect, theme: &Theme) {
    let popup = centered(52, 30, area);

    let key = Style::default().fg(theme.highlight());
    let desc = Style::default().fg(theme.text());

    let rows: &[(&str, &str)] = &[
        ("j / k", "navigate up/down"),
        ("h / l", "switch panel left/right"),
        ("g / G", "jump to top / bottom"),
        ("Ctrl-d / Ctrl-u", "half page down / up"),
        ("Enter", "expand / fold span"),
        ("a / A", "expand all / collapse all"),
        ("z", "zoom focused panel"),
        ("Backspace", "step back one level"),
        ("1 2 3 4", "fleet / focus / history / cost"),
        ("R / W", "replay / impact (history)"),
        ("i", "intervention menu"),
        ("p / P", "pause agent / pause fleet"),
        (":", "command palette"),
        ("/", "filter trace tree"),
        ("n", "annotate span"),
        ("y / Y", "copy span / trace id"),
        ("e", "export trace json"),
        ("T", "cycle theme"),
        ("m", "toggle mouse capture"),
        ("?", "toggle this overlay"),
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
