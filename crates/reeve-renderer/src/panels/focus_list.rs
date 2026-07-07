use crate::app::AppState;
use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

/// Focus view's trace-history strip: the selected agent's recent traces,
/// newest first, one row each. Replaces the agent fleet sections while
/// Focus is active. The selected row is what the tree beside it shows.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        "TRACES",
        Style::default().fg(theme.get("blue")),
    ))];

    let visible = area.height.saturating_sub(1) as usize;
    // Keep the selection on screen: scroll the window, not the cursor.
    let start = state
        .focus_selected
        .saturating_sub(visible.saturating_sub(1));

    for (i, trace) in state
        .focus_traces
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
    {
        let selected = i == state.focus_selected;
        let short_id: String = trace.id.chars().take(8).collect();
        let score = trace
            .final_health_score
            .map(|s| format!("\u{25C6}{}", s.round() as u32))
            .unwrap_or_else(|| "\u{25C6}--".to_string());

        let score_color = match trace.final_health_score {
            Some(s) if s >= 80.0 => theme.health_ok(),
            Some(s) if s >= 50.0 => theme.health_warn(),
            Some(_) => theme.health_crit(),
            None => theme.subtext(),
        };

        let row_style = if selected {
            Style::default().bg(theme.get("selected"))
        } else {
            Style::default()
        };
        lines.push(
            Line::from(vec![
                Span::styled(format!("{} ", short_id), Style::default().fg(theme.text())),
                Span::styled(score, Style::default().fg(score_color)),
            ])
            .style(row_style),
        );
    }

    frame.render_widget(Paragraph::new(lines), area);
}
