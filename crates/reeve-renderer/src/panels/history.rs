use crate::app::AppState;
use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

/// History view's center panel: completed traces newest-first, one row
/// each, with score, cost, duration, and completion time. The selected
/// row's trace loads into the right panel on Enter.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        format!("HISTORY \u{2500} {} traces", state.history_entries.len()),
        Style::default().fg(theme.get("blue")),
    ))];

    if state.history_entries.is_empty() {
        lines.push(Line::from(Span::styled(
            " no completed traces",
            Style::default().fg(theme.subtext()),
        )));
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }

    let visible = area.height.saturating_sub(1) as usize;
    let start = state
        .history_selected
        .saturating_sub(visible.saturating_sub(1));

    for (i, (trace, cost)) in state
        .history_entries
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
    {
        let selected = i == state.history_selected;
        let short_id: String = trace.id.chars().take(8).collect();

        let (score_text, score_color) = match trace.final_health_score {
            Some(s) => (
                format!("\u{25C6}{:>3}", s.round() as u32),
                match s {
                    s if s >= 80.0 => theme.health_ok(),
                    s if s >= 50.0 => theme.health_warn(),
                    _ => theme.health_crit(),
                },
            ),
            None => ("\u{25C6} --".to_string(), theme.subtext()),
        };

        let duration = match trace.end_time {
            Some(end) => format_duration_ms(end.saturating_sub(trace.start_time)),
            None => "--".to_string(),
        };
        let completed = trace
            .end_time
            .map(format_clock_time)
            .unwrap_or_else(|| "--:--:--".to_string());

        let row_style = if selected {
            Style::default().bg(theme.get("selected"))
        } else {
            Style::default()
        };

        if selected && state.history_confirm_delete {
            lines.push(
                Line::from(vec![
                    Span::styled(format!(" {short_id}  "), Style::default().fg(theme.text())),
                    Span::styled(
                        "delete? [y] yes  [any] no",
                        Style::default().fg(theme.health_crit()),
                    ),
                ])
                .style(row_style),
            );
            continue;
        }

        lines.push(
            Line::from(vec![
                Span::styled(format!(" {short_id}  "), Style::default().fg(theme.text())),
                Span::styled(score_text, Style::default().fg(score_color)),
                Span::styled(
                    format!("  ${cost:.3}  {duration:>8}  "),
                    Style::default().fg(theme.text()),
                ),
                Span::styled(completed, Style::default().fg(theme.subtext())),
            ])
            .style(row_style),
        );
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// Millisecond duration as a compact human figure: 850ms, 12.4s, 3m05s.
fn format_duration_ms(ms: i64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// Unix millisecond timestamp as UTC HH:MM:SS. Local timezone rendering
/// needs a tz database dependency; UTC is honest until that is worth it.
fn format_clock_time(ms: i64) -> String {
    let secs_of_day = (ms / 1000).rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_format_compactly() {
        assert_eq!(format_duration_ms(850), "850ms");
        assert_eq!(format_duration_ms(12_400), "12.4s");
        assert_eq!(format_duration_ms(185_000), "3m05s");
    }
}
