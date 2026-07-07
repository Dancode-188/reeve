use crate::app::AppState;
use crate::ascii::AsciiMode;
use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use reeve_model::signal::EvaluationConfidence;

// Fixed three Tier 2 metrics shown in QUALITY. Tier 1 metrics (loop, cost, latency, fingerprint)
// contribute to SCORE only and have no individual rows.
const TIER2_METRICS: [(&str, &str, bool); 3] = [
    ("faith", "faithfulness", true), // (label, canonical_name, content_gated)
    ("tools", "tool_selection", false),
    ("hallu", "hallucination_detection", true),
];

// Metrics that factor into the composite health score (from health_score.rs WEIGHTS table).
// fingerprint_deviation, hallucination_detection, and intent_action_divergence are NOT in this
// list: they trigger policy alerts but do not factor into the health score.
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

    let sdh = span_detail_height(state);
    let ctx_h = ctx_window_height(state);
    let has_quality = !state.metric_scores.is_empty();

    // The NOTE box takes fixed rows off the bottom when the selected span
    // carries an annotation; everything else stacks above it.
    let note = state.trace.as_ref().and_then(|tv| {
        tv.selected
            .as_ref()
            .and_then(|id| tv.notes.get(id))
            .cloned()
    });
    let area = if let Some(ref content) = note {
        let note_h = note_height(content, area.width);
        if area.height > note_h {
            let chunks =
                Layout::vertical([Constraint::Fill(1), Constraint::Length(note_h)]).split(area);
            render_note(frame, chunks[1], content, theme);
            chunks[0]
        } else {
            area
        }
    } else {
        area
    };

    match (ctx_h, has_quality) {
        (0, false) => {
            render_span_detail(frame, area, state, theme);
        }
        (0, true) => {
            let chunks =
                Layout::vertical([Constraint::Length(sdh), Constraint::Fill(1)]).split(area);
            render_span_detail(frame, chunks[0], state, theme);
            render_quality(frame, chunks[1], state, theme);
        }
        (h, false) => {
            let chunks = Layout::vertical([
                Constraint::Length(sdh),
                Constraint::Length(h),
                Constraint::Fill(1),
            ])
            .split(area);
            render_span_detail(frame, chunks[0], state, theme);
            render_ctx_window(frame, chunks[1], state, theme);
        }
        (h, true) => {
            let chunks = Layout::vertical([
                Constraint::Length(sdh),
                Constraint::Length(h),
                Constraint::Fill(1),
            ])
            .split(area);
            render_span_detail(frame, chunks[0], state, theme);
            render_ctx_window(frame, chunks[1], state, theme);
            render_quality(frame, chunks[2], state, theme);
        }
    }
}

/// Label row plus the bordered box: prose the developer reads, one of the
/// two bordered content boxes the design allows.
fn note_height(content: &str, width: u16) -> u16 {
    let inner = width.saturating_sub(2).max(1) as usize;
    let lines = content.len().div_ceil(inner).max(1) as u16;
    1 + lines.min(4) + 2
}

fn render_note(frame: &mut Frame, area: Rect, content: &str, theme: &Theme) {
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).split(area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "NOTE",
            Style::default().fg(theme.get("blue")),
        ))),
        chunks[0],
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_idle()))
        .style(Style::default().bg(theme.surface()));
    frame.render_widget(
        Paragraph::new(Span::styled(
            content.to_string(),
            Style::default()
                .fg(theme.text())
                .add_modifier(Modifier::ITALIC),
        ))
        .wrap(ratatui::widgets::Wrap { trim: true })
        .block(block),
        chunks[1],
    );
}

fn span_detail_height(state: &AppState) -> u16 {
    let selected_span = state
        .trace
        .as_ref()
        .and_then(|tv| tv.selected.as_ref().and_then(|id| tv.spans.get(id)));

    match selected_span {
        None => 3, // label + "select a span" + divider
        Some(span) => {
            let mut h = 2u16; // label + span name row
            if span.attributes.get("gen_ai.request.model").is_some() {
                h += 1;
            }
            if span.end_time.is_some() {
                h += 1; // dur
            }
            if span.attributes.get("gen_ai.usage.input_tokens").is_some() {
                h += 1;
            }
            if span.attributes.get("gen_ai.usage.output_tokens").is_some() {
                h += 1;
            }
            if span.attributes.get("gen_ai.usage.cost").is_some() {
                h += 1;
            }
            h + 1 // divider
        }
    }
}

fn ctx_window_height(state: &AppState) -> u16 {
    let span = state
        .trace
        .as_ref()
        .and_then(|tv| tv.selected.as_ref().and_then(|id| tv.spans.get(id)));
    let span = match span {
        Some(s) => s,
        None => return 0,
    };
    if span
        .attributes
        .get("gen_ai.usage.input_tokens")
        .and_then(|v| v.as_u64())
        .is_none()
    {
        return 0;
    }
    let model = span
        .attributes
        .get("gen_ai.request.model")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if crate::context_windows::context_window_for_model(model).is_some() {
        4 // label + gauge + count + divider
    } else {
        3 // label + count + divider (no gauge for unknown model)
    }
}

fn render_ctx_window(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let span = match state
        .trace
        .as_ref()
        .and_then(|tv| tv.selected.as_ref().and_then(|id| tv.spans.get(id)))
    {
        Some(s) => s,
        None => return,
    };
    let tok_in = match span
        .attributes
        .get("gen_ai.usage.input_tokens")
        .and_then(|v| v.as_u64())
    {
        Some(t) => t,
        None => return,
    };
    let model = span
        .attributes
        .get("gen_ai.request.model")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let window = crate::context_windows::context_window_for_model(model);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(section_label("CTX WINDOW", theme));

    if let Some(max_tokens) = window {
        let pct = (tok_in as f64 / max_tokens as f64).clamp(0.0, 1.0);
        let (bar_color, warn) = if pct >= 0.95 {
            (theme.health_crit(), " \u{26A0}")
        } else if pct >= 0.85 {
            (theme.health_warn(), " \u{26A0}")
        } else {
            (theme.health_ok(), "")
        };
        lines.push(Line::from(vec![
            Span::styled(score_bar(pct), Style::default().fg(bar_color)),
            Span::raw(" "),
            Span::styled(
                format!("{:.0}%", pct * 100.0),
                Style::default().fg(bar_color),
            ),
            Span::styled(warn, Style::default().fg(bar_color)),
        ]));
        lines.push(Line::styled(
            format!(
                "{} / {} tok",
                format_tokens(tok_in),
                format_tokens(u64::from(max_tokens))
            ),
            Style::default().fg(theme.get("muted")),
        ));
    } else {
        lines.push(Line::styled(
            format!("{} tok", format_tokens(tok_in)),
            Style::default().fg(theme.get("muted")),
        ));
    }

    lines.push(divider(area.width, theme));
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_quality(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let mut lines: Vec<Line> = Vec::new();

    lines.push(section_label("QUALITY", theme));

    let tier2_disabled = state
        .eval_backend
        .as_deref()
        .map(|b| b == "disabled")
        .unwrap_or(false);

    let tick = state.streaming.cursor_tick;
    let pulse_visible = (tick / 4) % 2 == 0;

    // Three fixed Tier 2 metric rows (faith / tools / hallu).
    // Tier 1 metrics (loop, cost, latency, fingerprint) contribute to SCORE only.
    if !tier2_disabled {
        for &(label, canonical, content_gated) in &TIER2_METRICS {
            let unavailable = content_gated && state.privacy_tier < 2;

            if unavailable {
                lines.push(Line::styled(
                    "\u{2013}  enable content capture",
                    Style::default().fg(theme.get("muted")),
                ));
            } else if let Some(entry) = state.metric_scores.iter().find(|e| e.name == canonical) {
                // Resolved: name bar score confidence-icon
                let bar = score_bar(entry.score);
                let bar_color = score_color(entry.score, theme);
                let (icon, icon_color) = quality_icon(entry.score, entry.confidence, theme);
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{:<5}  ", label),
                        Style::default().fg(theme.subtext()),
                    ),
                    Span::styled(bar, Style::default().fg(bar_color)),
                    Span::raw(" "),
                    Span::styled(
                        format!("{:.2}", entry.score),
                        Style::default().fg(theme.text()),
                    ),
                    Span::raw(" "),
                    Span::styled(icon, Style::default().fg(icon_color)),
                ]));
            } else if state.health_tier2_pending {
                // Pending: ⋯ and "scoring" both animate together in chrome (fading pulse)
                let pulse = if pulse_visible {
                    theme.get("blue")
                } else {
                    theme.get("muted")
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{:<5}  ", label),
                        Style::default().fg(theme.subtext()),
                    ),
                    Span::styled("\u{22EF} scoring", Style::default().fg(pulse)),
                ]));
            } else {
                // Tier 2 finished but no score arrived
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{:<5}  ", label),
                        Style::default().fg(theme.subtext()),
                    ),
                    Span::styled(
                        "\u{2013} no result",
                        Style::default().fg(theme.get("muted")),
                    ),
                ]));
            }
        }
    }

    // One empty row separating metric rows from SCORE (not a full divider)
    lines.push(Line::raw(""));

    // SCORE row
    if let Some(score) = state.health_score {
        let pct = score / 100.0;
        let bar = score_bar(pct);
        let bar_color = score_color(pct, theme);
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<5}  ", "SCORE"),
                Style::default().fg(theme.subtext()),
            ),
            Span::styled(bar, Style::default().fg(bar_color)),
            Span::raw(" "),
            Span::styled(
                format!("{}", score.round() as u32),
                Style::default().fg(bar_color).add_modifier(Modifier::BOLD),
            ),
        ]));
    } else {
        lines.push(Line::raw(""));
    }

    // Renormalization note row: always reserved, blank text when all metrics are present
    let note = if state.health_tier2_pending {
        " \u{22EF} tier 2 scoring".to_string()
    } else if state
        .health_weight_coverage
        .map(|w| w < 0.99)
        .unwrap_or(false)
    {
        let active = state
            .metric_scores
            .iter()
            .filter(|e| WEIGHT_METRICS.contains(&e.name.as_str()))
            .count();
        format!("{}/5 metrics \u{00B7} renormalized", active)
    } else {
        String::new()
    };
    lines.push(Line::styled(note, Style::default().fg(theme.subtext())));

    lines.push(divider(area.width, theme));

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_span_detail(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let mut lines: Vec<Line> = Vec::new();

    lines.push(section_label("SPAN DETAIL", theme));

    match state
        .trace
        .as_ref()
        .and_then(|tv| tv.selected.as_ref().and_then(|id| tv.spans.get(id)))
    {
        None => {
            lines.push(Line::from(Span::styled(
                " select a span",
                Style::default().fg(theme.subtext()),
            )));
        }
        Some(span) => {
            // span name row
            if let Some(tv) = state.trace.as_ref() {
                if let Some(sel_id) = tv.selected.as_ref() {
                    if let Some(name) = tv.names.get(sel_id) {
                        let checkmark = if span.end_time.is_some() {
                            " \u{2713}"
                        } else {
                            ""
                        };
                        lines.push(Line::from(Span::styled(
                            format!("{}{}", name, checkmark),
                            Style::default()
                                .fg(theme.text())
                                .add_modifier(Modifier::BOLD),
                        )));
                    }
                }
            }
            // model
            if let Some(model) = span
                .attributes
                .get("gen_ai.request.model")
                .and_then(|v| v.as_str())
            {
                let w = area.width as usize;
                let val_max = w.saturating_sub(11);
                let model_str = truncate(model, val_max);
                lines.push(field_line("model", &model_str, theme.text(), theme));
            }

            // dur
            if let Some(end) = span.end_time {
                let dur_ms = end - span.start_time;
                lines.push(field_line("dur", &fmt_dur(dur_ms), theme.subtext(), theme));
            }

            // tokens in
            if let Some(tok_in) = span
                .attributes
                .get("gen_ai.usage.input_tokens")
                .and_then(|v| v.as_u64())
            {
                lines.push(field_line(
                    "tokens \u{2191}",
                    &format_tokens(tok_in),
                    theme.subtext(),
                    theme,
                ));
            }

            // tokens out
            if let Some(tok_out) = span
                .attributes
                .get("gen_ai.usage.output_tokens")
                .and_then(|v| v.as_u64())
            {
                lines.push(field_line(
                    "tokens \u{2193}",
                    &format_tokens(tok_out),
                    theme.subtext(),
                    theme,
                ));
            }

            // cost
            if let Some(cost) = span
                .attributes
                .get("gen_ai.usage.cost")
                .and_then(|v| v.as_f64())
            {
                lines.push(field_line(
                    "cost",
                    &format!("${:.4}", cost),
                    theme.get("teal"),
                    theme,
                ));
            }
        }
    }

    lines.push(divider(area.width, theme));

    frame.render_widget(Paragraph::new(lines), area);
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn section_label(title: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        title.to_string(),
        Style::default().fg(theme.get("blue")),
    ))
}

fn divider(width: u16, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        "\u{2500}".repeat(width as usize),
        Style::default().fg(theme.surface()),
    ))
}

fn field_line<'a>(
    label: &str,
    value: &str,
    value_color: ratatui::style::Color,
    theme: &Theme,
) -> Line<'a> {
    Line::from(vec![
        Span::styled(
            format!("{:<10}", label),
            Style::default().fg(theme.subtext()),
        ),
        Span::styled(value.to_string(), Style::default().fg(value_color)),
    ])
}

// High confidence: pass (>=0.6) shows checkmark, fail shows cross. Medium/Low shows ? regardless.
fn quality_icon(
    score: f64,
    conf: Option<EvaluationConfidence>,
    theme: &Theme,
) -> (&'static str, ratatui::style::Color) {
    match conf {
        Some(EvaluationConfidence::Medium) | Some(EvaluationConfidence::Low) => {
            ("?", theme.health_warn())
        }
        _ => {
            if score >= 0.6 {
                ("\u{2713}", theme.health_ok())
            } else {
                ("\u{2717}", theme.health_crit())
            }
        }
    }
}

fn score_bar(score: f64) -> String {
    let filled = (score.clamp(0.0, 1.0) * 8.0).round() as usize;
    let empty = 8 - filled;
    format!("{}{}", "\u{2588}".repeat(filled), "\u{2591}".repeat(empty))
}

fn score_color(score: f64, theme: &Theme) -> ratatui::style::Color {
    if score >= 0.8 {
        theme.health_ok()
    } else if score >= 0.6 {
        theme.health_warn()
    } else {
        theme.health_crit()
    }
}

fn fmt_dur(ms: i64) -> String {
    if ms < 1_000 {
        format!("{} ms", ms)
    } else {
        format!("{:.1} s", ms as f64 / 1_000.0)
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!(
            "{},{:03},{:03}",
            n / 1_000_000,
            (n / 1_000) % 1_000,
            n % 1_000
        )
    } else if n >= 1_000 {
        format!("{},{:03}", n / 1_000, n % 1_000)
    } else {
        format!("{}", n)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else if max > 1 {
        format!("{}\u{2026}", &s[..max.saturating_sub(1)])
    } else {
        s[..max].to_string()
    }
}
