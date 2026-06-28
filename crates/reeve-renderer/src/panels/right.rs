use crate::app::{AppState, PanelFocus};
use crate::ascii::AsciiMode;
use crate::theme::Theme;
use crate::widgets::{CostSparkline, HealthGauge};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Text},
    widgets::{Block, Borders, Paragraph},
};

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme, ascii: &AsciiMode) {
    if area.width == 0 {
        return;
    }

    let _ = ascii;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(5),
            Constraint::Fill(1),
        ])
        .split(area);

    let focused = state.panel_focus == PanelFocus::Right;

    frame.render_widget(
        HealthGauge {
            score: state.health_score,
            focused,
            theme,
        },
        chunks[0],
    );

    let cost_history: &[f64] = state
        .selected_agent
        .and_then(|i| state.agents.get_index(i))
        .map(|(_, s)| s.cost_history.as_slice())
        .unwrap_or(&[]);

    frame.render_widget(
        CostSparkline {
            history: cost_history,
            focused,
            theme,
        },
        chunks[1],
    );

    render_span_detail(frame, chunks[2], state, theme, focused);
}

fn render_span_detail(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    focused: bool,
) {
    let border_style = Style::default().fg(if focused {
        theme.border_focused()
    } else {
        theme.border_idle()
    });

    let block = Block::default()
        .title("SPAN")
        .borders(Borders::ALL)
        .border_style(border_style);

    let text = match state
        .trace
        .as_ref()
        .and_then(|tv| tv.selected.as_ref().and_then(|id| tv.spans.get(id)))
    {
        None => Text::from(Line::styled(
            " select a span",
            Style::default().fg(theme.subtext()),
        )),
        Some(span) => {
            let mut lines = vec![
                Line::styled(
                    format!(" op:     {}", span.operation),
                    Style::default().fg(theme.text()),
                ),
                Line::styled(
                    format!(" status: {:?}", span.status),
                    Style::default().fg(theme.subtext()),
                ),
                Line::styled(
                    format!(" start:  {}", span.start_time),
                    Style::default().fg(theme.subtext()),
                ),
            ];
            if let Some(end) = span.end_time {
                lines.push(Line::styled(
                    format!(" dur:    {}ms", end - span.start_time),
                    Style::default().fg(theme.subtext()),
                ));
            }
            if let Some(cost) = span
                .attributes
                .get("gen_ai.usage.cost")
                .and_then(|v| v.as_f64())
            {
                lines.push(Line::styled(
                    format!(" cost:   ${:.4}", cost),
                    Style::default().fg(theme.get("teal")),
                ));
            }
            Text::from(lines)
        }
    };

    frame.render_widget(Paragraph::new(text).block(block), area);
}
