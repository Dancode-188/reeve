use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};

/// The command palette: one input row directly above the footer, with the
/// highlighted completion inline. Drawn over the bottom row of the body so
/// nothing below it shifts.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    buffer: &str,
    matches: &[String],
    selected: usize,
    confirm_kill: bool,
    theme: &Theme,
) {
    if area.height == 0 {
        return;
    }
    frame.render_widget(Clear, area);

    let line = if confirm_kill {
        Line::from(vec![
            Span::styled(" :kill all ", Style::default().fg(theme.text())),
            Span::styled(
                "\u{2014} kill every running agent? [y] yes  [any] no",
                Style::default().fg(theme.health_crit()),
            ),
        ])
    } else {
        let mut spans = vec![
            Span::styled(
                " :",
                Style::default()
                    .fg(theme.get("blue"))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(buffer.to_string(), Style::default().fg(theme.text())),
            Span::styled("\u{258C}", Style::default().fg(theme.text())),
        ];
        if let Some(hit) = matches.get(selected) {
            // Show the rest of the highlighted completion after the cursor,
            // plus how many other matches Tab cycles through.
            let rest = hit.strip_prefix(buffer).unwrap_or(hit);
            spans.push(Span::styled(
                rest.to_string(),
                Style::default().fg(theme.subtext()),
            ));
            if matches.len() > 1 {
                spans.push(Span::styled(
                    format!("  ({} of {}, Tab cycles)", selected + 1, matches.len()),
                    Style::default().fg(theme.subtext()),
                ));
            }
        }
        Line::from(spans)
    };

    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(theme.surface())),
        area,
    );
}
