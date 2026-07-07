use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};
use reeve_storage::warm::CostSummary;

/// Cost view: total spend with trace count, then cost per agent and cost
/// per model as horizontal bars. Bars are text-built rather than Ratatui's
/// BarChart because horizontal bars with a name, a scaled bar, and an exact
/// dollar figure per row read better in a narrow panel than vertical bars
/// with truncated axis labels.
pub fn render(frame: &mut Frame, area: Rect, summary: &CostSummary, theme: &Theme) {
    if area.width < 20 || area.height < 4 {
        return;
    }

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Percentage(50),
            Constraint::Fill(1),
        ])
        .split(area);

    let headline = vec![
        Line::from(vec![
            Span::styled("COST", Style::default().fg(theme.get("blue"))),
            Span::styled(
                format!("  ${:.3} total", summary.total),
                Style::default().fg(theme.text()),
            ),
            Span::styled(
                format!("  \u{00B7}  {} traces", summary.trace_count),
                Style::default().fg(theme.subtext()),
            ),
        ]),
        Line::raw(""),
    ];
    frame.render_widget(Paragraph::new(headline), sections[0]);

    bars(frame, sections[1], "BY AGENT", &summary.by_agent, theme);
    bars(frame, sections[2], "BY MODEL", &summary.by_model, theme);
}

fn bars(frame: &mut Frame, area: Rect, title: &str, rows: &[(String, f64)], theme: &Theme) {
    if area.height < 2 {
        return;
    }
    let mut lines = vec![Line::from(Span::styled(
        title.to_string(),
        Style::default().fg(theme.get("blue")),
    ))];

    let max = rows.iter().map(|(_, c)| *c).fold(0.0_f64, f64::max);
    let name_width = 14usize;
    let value_width = 10usize;
    let bar_width = (area.width as usize)
        .saturating_sub(name_width + value_width + 3)
        .max(4);

    for (name, cost) in rows.iter().take(area.height.saturating_sub(1) as usize) {
        let filled = if max > 0.0 {
            ((cost / max) * bar_width as f64).round() as usize
        } else {
            0
        };
        let bar: String = "\u{2587}".repeat(filled) + &"\u{2581}".repeat(bar_width - filled);
        let display_name: String = if name.len() > name_width {
            format!("{}\u{2026}", &name[..name_width - 1])
        } else {
            format!("{name:<name_width$}")
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {display_name} "),
                Style::default().fg(theme.text()),
            ),
            Span::styled(bar, Style::default().fg(theme.get("blue"))),
            Span::styled(format!(" ${cost:.3}"), Style::default().fg(theme.text())),
        ]));
    }
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            " no spend recorded",
            Style::default().fg(theme.subtext()),
        )));
    }

    frame.render_widget(Paragraph::new(lines), area);
}
