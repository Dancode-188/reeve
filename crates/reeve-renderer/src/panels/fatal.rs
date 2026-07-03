use crate::app::FatalError;
use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

pub fn render(frame: &mut Frame, area: Rect, err: &FatalError, theme: &Theme) {
    let hint_rows: u16 = if err.hint.is_some() { 2 } else { 0 };
    let card_w = 52_u16.min(area.width.saturating_sub(4));
    let card_h = (9 + hint_rows).min(area.height.saturating_sub(2));
    let card = centered(card_w, card_h, area);

    let crit = Style::default().fg(theme.health_crit());
    let text_style = Style::default().fg(theme.text());
    let muted = Style::default().fg(theme.subtext());
    let kb = Style::default()
        .fg(theme.get("blue"))
        .add_modifier(Modifier::BOLD);

    let mut lines: Vec<Line> = vec![
        Line::raw(""),
        Line::from(Span::styled("\u{2715}  Fatal Error", crit)).alignment(Alignment::Center),
        Line::raw(""),
        Line::from(Span::styled(format!("  {}", err.message), text_style)),
        Line::raw(""),
    ];

    if let Some(ref hint) = err.hint {
        lines.push(Line::from(Span::styled(format!("  {}", hint), muted)));
        lines.push(Line::raw(""));
    }

    lines.push(
        Line::from(vec![
            Span::styled("[r]", kb),
            Span::styled(" retry    ", muted),
            Span::styled("[q]", kb),
            Span::styled(" quit", muted),
        ])
        .alignment(Alignment::Center),
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focused()));

    frame.render_widget(Clear, card);
    frame.render_widget(Paragraph::new(lines).block(block), card);
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
