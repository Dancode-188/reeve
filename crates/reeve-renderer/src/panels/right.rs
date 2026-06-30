use crate::app::{AppState, PanelFocus};
use crate::ascii::AsciiMode;
use crate::theme::Theme;
use crate::widgets::{CostSparkline, HealthGauge};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
};
use reeve_model::signal::EvaluationConfidence;

const WEIGHT_METRICS: &[&str] = &[
    "faithfulness",
    "tool_selection",
    "loop_detection",
    "cost_efficiency",
    "latency_normality",
];

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme, ascii: &AsciiMode) {
    if area.width == 0 {
        return;
    }

    let _ = ascii;

    let focused = state.panel_focus == PanelFocus::Right;

    let constraints: Vec<Constraint> = if state.metric_scores.is_empty() {
        vec![
            Constraint::Length(3),
            Constraint::Length(5),
            Constraint::Fill(1),
        ]
    } else {
        vec![
            Constraint::Length(3),
            Constraint::Length(5),
            Constraint::Length(quality_height(state)),
            Constraint::Fill(1),
        ]
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    frame.render_widget(
        HealthGauge {
            score: state.health_score,
            tier2_pending: state.health_tier2_pending,
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

    if !state.metric_scores.is_empty() {
        render_quality(frame, chunks[2], state, theme, focused);
        render_span_detail(frame, chunks[3], state, theme, focused);
    } else {
        render_span_detail(frame, chunks[2], state, theme, focused);
    }
}

fn quality_height(state: &AppState) -> u16 {
    let metric_rows = state.metric_scores.len() as u16;
    let has_note = state.health_tier2_pending
        || state
            .health_weight_coverage
            .map(|w| w < 0.99)
            .unwrap_or(false);
    let note_row: u16 = if has_note { 1 } else { 0 };
    (2 + metric_rows + note_row).min(9)
}

fn render_quality(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme, focused: bool) {
    let border_style = Style::default().fg(if focused {
        theme.border_focused()
    } else {
        theme.border_idle()
    });

    let block = Block::default()
        .title("QUALITY")
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let max_rows = inner.height as usize;
    let mut lines: Vec<Line> = Vec::new();

    for entry in &state.metric_scores {
        if lines.len() >= max_rows {
            break;
        }
        let name = abbrev_metric(&entry.name);
        let bar = score_bar(entry.score);
        let score_str = format!("{:.2}", entry.score);
        let bar_color = score_color(entry.score, theme);

        let mut spans = vec![
            Span::styled(name, Style::default().fg(theme.subtext())),
            Span::raw(" "),
            Span::styled(bar, Style::default().fg(bar_color)),
            Span::raw(" "),
            Span::styled(score_str, Style::default().fg(theme.text())),
        ];

        if let Some(conf) = entry.confidence {
            let (badge, color) = conf_badge(conf, theme);
            spans.push(Span::raw(" "));
            spans.push(Span::styled(badge, Style::default().fg(color)));
        }

        lines.push(Line::from(spans));
    }

    if lines.len() < max_rows {
        if state.health_tier2_pending {
            lines.push(Line::styled(
                " \u{22EF} tier 2 scoring",
                Style::default().fg(theme.subtext()),
            ));
        } else if let Some(w) = state.health_weight_coverage {
            if w < 0.99 {
                let active = state
                    .metric_scores
                    .iter()
                    .filter(|e| {
                        WEIGHT_METRICS.contains(&e.name.as_str())
                            && e.confidence != Some(EvaluationConfidence::Low)
                    })
                    .count();
                let note = format!("{}/5 metrics \u{00B7} renormalized", active);
                lines.push(Line::styled(note, Style::default().fg(theme.subtext())));
            }
        }
    }

    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn abbrev_metric(name: &str) -> String {
    let abbrev = match name {
        "faithfulness" => "faithful",
        "tool_selection" => "tool_sel",
        "loop_detection" => "loop_det",
        "cost_efficiency" => "cost_eff",
        "latency_normality" => "latency",
        "hallucination_detection" => "hallucin",
        "fingerprint_deviation" => "fingerpr",
        "intent_action_divergence" => "intent",
        other => {
            let end = other.len().min(8);
            return format!("{:<8}", &other[..end]);
        }
    };
    format!("{:<8}", abbrev)
}

fn score_bar(score: f64) -> String {
    let filled = (score.clamp(0.0, 1.0) * 8.0).round() as usize;
    let empty = 8 - filled;
    format!("{}{}", "\u{2588}".repeat(filled), "\u{2591}".repeat(empty))
}

fn score_color(score: f64, theme: &Theme) -> Color {
    if score >= 0.8 {
        theme.health_ok()
    } else if score >= 0.6 {
        theme.health_warn()
    } else {
        theme.health_crit()
    }
}

fn conf_badge(conf: EvaluationConfidence, theme: &Theme) -> (&'static str, Color) {
    match conf {
        EvaluationConfidence::High => ("H", theme.health_ok()),
        EvaluationConfidence::Medium => ("M", theme.health_warn()),
        EvaluationConfidence::Low => ("L", theme.health_crit()),
    }
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
            let inner_w = area.width.saturating_sub(2) as usize;
            let op_max = inner_w.saturating_sub(9);
            let op = if span.operation.len() > op_max && op_max > 3 {
                format!("{}...", &span.operation[..op_max - 3])
            } else {
                span.operation.clone()
            };
            let mut lines = vec![
                Line::styled(
                    format!(" op:     {}", op),
                    Style::default().fg(theme.text()),
                ),
                Line::styled(
                    format!(" status: {:?}", span.status),
                    Style::default().fg(theme.subtext()),
                ),
                Line::styled(
                    format!(" start:  {}", fmt_ts(span.start_time)),
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

fn fmt_ts(ms: i64) -> String {
    let secs = (ms / 1000).unsigned_abs();
    let ms_part = (ms % 1000).unsigned_abs();
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    format!("{h:02}:{m:02}:{s:02}.{ms_part:03}")
}
