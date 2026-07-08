use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

const WORDMARK: &[&str] = &[
    "██████╗ ███████╗███████╗██╗   ██╗███████╗",
    "██╔══██╗██╔════╝██╔════╝██║   ██║██╔════╝",
    "██████╔╝█████╗  █████╗  ██║   ██║█████╗  ",
    "██╔══██╗██╔══╝  ██╔══╝  ╚██╗ ██╔╝██╔══╝  ",
    "██║  ██║███████╗███████╗ ╚████╔╝ ███████╗",
    "╚═╝  ╚═╝╚══════╝╚══════╝  ╚═══╝  ╚══════╝",
];

/// The waiting state before the first agent connects: the wordmark,
/// the listening address, and a pulsing ellipsis, centered where the
/// cockpit will appear. Dissolves the moment the first span arrives.
pub fn render(frame: &mut Frame, area: Rect, tick: u8, ascii: bool, theme: &Theme) {
    if area.height < 4 {
        return;
    }
    let mark_height = if ascii { 1 } else { WORDMARK.len() as u16 };
    let top = area.y + (area.height.saturating_sub(mark_height + 3)) / 2;

    let mut lines: Vec<Line> = Vec::new();
    if ascii {
        lines.push(Line::from(Span::styled(
            "R E E V E",
            Style::default()
                .fg(theme.get("blue"))
                .add_modifier(Modifier::BOLD),
        )));
    } else {
        for row in WORDMARK {
            lines.push(Line::from(Span::styled(
                *row,
                Style::default().fg(theme.get("blue")),
            )));
        }
    }
    lines.push(Line::raw(""));
    let dots = match (tick / 5) % 3 {
        0 => ".",
        1 => "..",
        _ => "...",
    };
    lines.push(Line::from(Span::styled(
        format!("listening on :4317 · connect an agent to begin{dots}"),
        Style::default().fg(theme.subtext()),
    )));

    let rect = Rect {
        x: area.x,
        y: top,
        width: area.width,
        height: mark_height + 3,
    };
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), rect);
}
