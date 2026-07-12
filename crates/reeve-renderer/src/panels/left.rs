use crate::app::{AgentBudget, AppState, FlashTarget};
use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Sparkline, Widget},
};
use reeve_model::entity::agent::AgentStatus;
use reeve_model::signal::CostTrend;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if area.width == 0 {
        return;
    }

    let tier2_disabled = state
        .eval_backend
        .as_deref()
        .map(|b| b == "disabled")
        .unwrap_or(false);
    let mini_metric_rows: u16 = if tier2_disabled { 0 } else { 3 };

    let health_height = 1 + 1 + 1 + mini_metric_rows + 1; // label+gauge+status+metrics+divider

    let selected_agent = state.selected_agent.and_then(|i| state.agents.get_index(i));
    let cost_history: &[f64] = selected_agent
        .map(|(_, s)| s.cost_history.as_slice())
        .unwrap_or(&[]);
    let has_predicted = false; // populated when issue #55 ships
    let has_budget = selected_agent
        .map(|(_, s)| s.budget.is_some())
        .unwrap_or(false);
    // label+total+budget+sparkline+predicted+divider
    let cost_height = 1 + 1 + u16::from(has_budget) + 1 + u16::from(has_predicted) + 1;

    let has_alerts = !state.policy_alerts.is_empty();
    let alert_height = if has_alerts {
        // Label + one row per alert plus one per effectiveness note, capped.
        let rows: usize = state
            .policy_alerts
            .iter()
            .map(|a| 1 + usize::from(a.effectiveness.is_some()))
            .sum();
        1 + rows.min(6) as u16
    } else {
        0
    };

    let agent_count = state.agents.len();
    let agents_height: u16 = if agent_count == 0 {
        3 // label + "no agents" + divider
    } else {
        1 + (agent_count as u16) * 2 + 1 // label + 2 rows per agent + divider
    };

    let mut constraints = vec![
        Constraint::Length(agents_height),
        Constraint::Length(health_height),
        Constraint::Length(cost_height),
    ];
    if has_alerts {
        constraints.push(Constraint::Length(alert_height));
    }
    constraints.push(Constraint::Fill(1)); // empty space sink

    let chunks = Layout::vertical(constraints).split(area);

    render_agents(frame, chunks[0], state, theme);
    render_health(frame, chunks[1], state, theme, tier2_disabled);
    render_cost(frame, chunks[2], state, theme, cost_history);
    if has_alerts {
        render_alerts(frame, chunks[3], state, theme);
    }
}

fn render_agents(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let w = area.width as usize;
    let mut lines: Vec<Line> = Vec::new();

    lines.push(section_label("AGENTS", theme));

    for (i, (agent_id, agent_state)) in state.agents.iter().enumerate() {
        let is_selected = state.selected_agent == Some(i);

        let indicator = status_indicator(agent_state.agent.status);
        let indicator_color = match agent_state.agent.status {
            AgentStatus::Running => theme.health_ok(),
            AgentStatus::Idle => theme.subtext(),
            AgentStatus::Paused => theme.health_warn(),
            AgentStatus::Error => theme.health_crit(),
        };

        // A dropped control stream shows on the row: the grace period is
        // exactly when a developer wants to know the agent may come back.
        let offline = state.control_disconnected.contains(agent_id);
        let via_proxy =
            agent_state.agent.integration == reeve_model::entity::IntegrationPath::Proxy;
        let tag_width = if offline { 11 } else { 0 } + if via_proxy { 8 } else { 0 };
        let max_name = w.saturating_sub(3 + tag_width);
        let name = truncate(&agent_state.agent.name, max_name);

        let flash_color = state.flash_color(&FlashTarget::AgentRow(agent_id.clone()), theme);
        // Sustained pulse: an unselected agent that crossed a health band
        // for the worse keeps pulsing in the new band's color until it is
        // selected or recovers. Slow cycle, same cadence as the live dot.
        let sustained = state.sustained_alerts.get(agent_id).copied();
        let pulse_on = (state.streaming.cursor_tick / 7) % 2 == 0;
        let name_style = if is_selected {
            Style::default()
                .fg(theme.background())
                .bg(theme.highlight())
                .add_modifier(Modifier::BOLD)
        } else if let Some(c) = flash_color {
            Style::default().fg(c).add_modifier(Modifier::BOLD)
        } else if let Some(score) = sustained {
            let band = health_color(score, theme);
            if pulse_on {
                Style::default().fg(band).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(band)
                    .add_modifier(Modifier::BOLD | Modifier::DIM)
            }
        } else {
            Style::default()
                .fg(theme.text())
                .add_modifier(Modifier::BOLD)
        };

        let mut row = vec![
            Span::styled(indicator, Style::default().fg(indicator_color)),
            Span::raw(" "),
            Span::styled(name, name_style),
        ];
        if via_proxy {
            row.push(Span::styled(
                " [proxy]",
                Style::default().fg(theme.subtext()),
            ));
        }
        if offline {
            row.push(Span::styled(
                " [offline]",
                Style::default().fg(theme.health_warn()),
            ));
        }
        lines.push(Line::from(row));

        // Sub-line: score + cost + trend for active/paused, idle text for idle
        let sub = match agent_state.agent.status {
            AgentStatus::Running | AgentStatus::Paused => {
                let score_str = state
                    .health_score
                    .filter(|_| is_selected)
                    .map(|s| format!("{}", s.round() as u32))
                    .unwrap_or_else(|| "--".to_string());
                let cost_str = format!("${:.3}", agent_state.display_cost());
                let health_color = state
                    .health_score
                    .filter(|_| is_selected)
                    .map(|s| health_color(s, theme))
                    .unwrap_or(theme.subtext());

                let mut spans = vec![
                    Span::raw("  "),
                    Span::styled("\u{25C6}", Style::default().fg(theme.get("blue"))),
                    Span::styled(
                        score_str,
                        Style::default()
                            .fg(health_color)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(cost_str, Style::default().fg(theme.subtext())),
                ];
                if let Some(trend) = agent_state.cost_trend {
                    let (arrow, color) = trend_arrow(trend, theme);
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(arrow, Style::default().fg(color)));
                }
                Line::from(spans)
            }
            AgentStatus::Error => Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "error",
                    Style::default()
                        .fg(theme.health_crit())
                        .add_modifier(Modifier::DIM),
                ),
            ]),
            AgentStatus::Idle if state.killed.contains(agent_id) => Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("killed \u{00B7} ${:.3}", agent_state.display_cost()),
                    Style::default()
                        .fg(theme.health_crit())
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            AgentStatus::Idle => Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("idle \u{00B7} ${:.3}", agent_state.display_cost()),
                    Style::default()
                        .fg(theme.subtext())
                        .add_modifier(Modifier::DIM),
                ),
            ]),
        };
        lines.push(sub);
    }

    if state.agents.is_empty() {
        lines.push(Line::from(Span::styled(
            " no agents",
            Style::default().fg(theme.subtext()),
        )));
    }

    lines.push(divider(area.width, theme));

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().fg(theme.text())),
        area,
    );
}

fn render_health(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    tier2_disabled: bool,
) {
    let w = area.width as usize;
    let mut lines: Vec<Line> = Vec::new();

    lines.push(section_label("HEALTH", theme));

    let flash_color = state.flash_color(&FlashTarget::HealthGauge, theme);

    // Gauge row
    match state.health_score {
        None => {
            lines.push(Line::from(Span::styled(
                format!("{:\u{2500}<width$}", "", width = w.saturating_sub(1)),
                Style::default().fg(theme.subtext()),
            )));
        }
        Some(score) => {
            let color = flash_color.unwrap_or_else(|| health_color(score, theme));
            let score_str = format!(" {}", score.round() as u32);
            let gauge_w = w.saturating_sub(score_str.len());
            let filled = ((score.clamp(0.0, 100.0) / 100.0) * gauge_w as f64).round() as usize;
            let empty = gauge_w.saturating_sub(filled);

            let mut modifier = Modifier::BOLD;
            if score < 20.0 {
                modifier |= Modifier::SLOW_BLINK;
            }

            lines.push(Line::from(vec![
                Span::styled("\u{2588}".repeat(filled), Style::default().fg(color)),
                Span::styled(
                    "\u{2591}".repeat(empty),
                    Style::default().fg(theme.get("muted")),
                ),
                Span::styled(score_str, Style::default().fg(color).add_modifier(modifier)),
            ]));

            // Status row
            let band = health_band(score);
            let mut status_spans = vec![Span::styled(
                band,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )];
            if state.health_tier2_pending {
                status_spans.push(Span::styled(
                    " \u{00B7} t2 scoring",
                    Style::default().fg(theme.subtext()),
                ));
            }
            lines.push(Line::from(status_spans));
        }
    }

    // Mini-metric rows: indicator first, then name, then value: ✓ faith  0.89 / ⋯ tools  scoring
    if !tier2_disabled {
        let metrics = [
            ("faith", "faithfulness"),
            ("tools", "tool_selection"),
            ("hallu", "hallucination_detection"),
        ];

        let tick = state.streaming.cursor_tick;
        let pulse_visible = (tick / 4) % 2 == 0;

        for (abbrev, metric_name) in metrics {
            let is_content_metric =
                metric_name == "faithfulness" || metric_name == "hallucination_detection";
            let unavailable = is_content_metric && state.privacy_tier < 2;

            if unavailable {
                // dim, non-pulsing; fixed row position identifies which metric
                lines.push(Line::styled(
                    "\u{2013} capture off",
                    Style::default().fg(theme.get("muted")),
                ));
            } else if let Some(entry) = state.metric_scores.iter().find(|e| e.name == metric_name) {
                // ✓  faith  0.89
                let sc = score_color(entry.score, theme);
                lines.push(Line::from(vec![
                    Span::styled("\u{2713}", Style::default().fg(theme.health_ok())),
                    Span::styled(
                        format!(" {:<5}  ", abbrev),
                        Style::default().fg(theme.subtext()),
                    ),
                    Span::styled(format!("{:.2}", entry.score), Style::default().fg(sc)),
                ]));
            } else if state.health_tier2_pending {
                // Pending: ⋯ pulses chrome/muted; label and "scoring" are dim throughout
                let pulse = if pulse_visible {
                    theme.get("blue")
                } else {
                    theme.get("muted")
                };
                lines.push(Line::from(vec![
                    Span::styled("\u{22EF}", Style::default().fg(pulse)),
                    Span::styled(
                        format!(" {:<5}  scoring", abbrev),
                        Style::default().fg(theme.get("muted")),
                    ),
                ]));
            } else {
                // Tier 2 finished but no score arrived (skipped or failed in this run)
                lines.push(Line::styled(
                    "\u{2013} no result",
                    Style::default().fg(theme.get("muted")),
                ));
            }
        }
    }

    lines.push(divider(area.width, theme));

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().fg(theme.text())),
        area,
    );
}

/// The daily-budget bar for the COST section: a filled gauge against the
/// cap with the ceiling labelled, coloured teal under the warn threshold,
/// amber past it, and red once the budget has stopped the agent. Mirrors
/// the HEALTH gauge so the two read as one family.
fn budget_bar(b: AgentBudget, width: usize, theme: &Theme) -> Line<'static> {
    let fraction = if b.cap > 0.0 {
        (b.spent_today / b.cap).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let color = if b.over {
        theme.health_crit()
    } else if fraction >= 0.8 {
        theme.health_warn()
    } else {
        theme.get("teal")
    };
    // A whole-dollar cap reads cleaner without cents; a fractional one keeps
    // them so the ceiling shown is the ceiling set.
    let cap_str = if b.cap.fract() == 0.0 {
        format!(" /${:.0}", b.cap)
    } else {
        format!(" /${:.2}", b.cap)
    };
    let gauge_w = width.saturating_sub(cap_str.len());
    let filled = (fraction * gauge_w as f64).round() as usize;
    let empty = gauge_w.saturating_sub(filled);
    Line::from(vec![
        Span::styled("\u{2588}".repeat(filled), Style::default().fg(color)),
        Span::styled(
            "\u{2591}".repeat(empty),
            Style::default().fg(theme.get("muted")),
        ),
        Span::styled(cap_str, Style::default().fg(theme.subtext())),
    ])
}

fn render_cost(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    cost_history: &[f64],
) {
    if area.height == 0 {
        return;
    }

    let selected_agent = state
        .selected_agent
        .and_then(|i| state.agents.get_index(i))
        .map(|(_, s)| s);

    let total_cost = selected_agent.map(|s| s.display_cost()).unwrap_or(0.0);
    let cost_trend = selected_agent.and_then(|s| s.cost_trend);
    let budget = selected_agent.and_then(|s| s.budget);

    // Layout: label(1) + total(1) + [budget(1)] + sparkline(1) + divider(1)
    let n_rows = area.height;
    // The budget bar, when present, sits between the total and the
    // sparkline, pushing the sparkline down a row.
    let sparkline_row = 2u16 + u16::from(budget.is_some());

    // Label + total lines
    let label_line = section_label("COST", theme);

    let flash_color = state.flash_color(&FlashTarget::CostTotal, theme);
    let cost_color = flash_color.unwrap_or_else(|| theme.get("teal"));

    let total_line = Line::from(vec![
        Span::styled(
            format!("${:.3}", total_cost),
            Style::default().fg(cost_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" today", Style::default().fg(theme.subtext())),
    ]);

    let mut top_lines = vec![label_line, total_line];
    if let Some(b) = budget {
        top_lines.push(budget_bar(b, area.width as usize, theme));
    }
    let top_height = top_lines.len() as u16;

    // Render label, total, and the budget bar as one paragraph.
    let top_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: top_height,
    };
    frame.render_widget(Paragraph::new(top_lines), top_area);

    // Sparkline row
    let spark_area = Rect {
        x: area.x,
        y: area.y + sparkline_row,
        width: area.width,
        height: 1,
    };

    if !cost_history.is_empty() {
        let data: Vec<u64> = cost_history
            .iter()
            .map(|&c| (c * 10_000.0) as u64)
            .collect();
        Sparkline::default()
            .style(Style::default().fg(theme.get("teal")))
            .data(data.as_slice())
            .render(spark_area, frame.buffer_mut());
    }

    // Trend arrow after sparkline (rendered as a single char at right edge of spark row)
    if let Some(trend) = cost_trend {
        let (arrow, color) = trend_arrow(trend, theme);
        let arrow_x = area.x + area.width.saturating_sub(2);
        if arrow_x < area.x + area.width {
            let arrow_area = Rect {
                x: arrow_x,
                y: area.y + sparkline_row,
                width: 1,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(arrow, Style::default().fg(color)))),
                arrow_area,
            );
        }
    }

    // Divider on last row
    let divider_y = area.y + n_rows.saturating_sub(1);
    if divider_y < area.y + area.height {
        let div_area = Rect {
            x: area.x,
            y: divider_y,
            width: area.width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(divider(area.width, theme)), div_area);
    }
}

fn render_alerts(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let mut lines: Vec<Line> = Vec::new();

    let flash_color = state.flash_color(&FlashTarget::AlertSection, theme);
    let label_color = flash_color.unwrap_or_else(|| theme.get("blue"));

    lines.push(Line::from(Span::styled(
        "ALERTS",
        Style::default().fg(label_color),
    )));

    let max_label = (area.width as usize).saturating_sub(3);
    let max_rows = area.height.saturating_sub(1) as usize;

    let mut rows_left = max_rows;
    for alert in state.policy_alerts.iter().rev() {
        if rows_left == 0 {
            break;
        }
        let label = truncate(&alert.description, max_label);
        lines.push(Line::from(vec![
            Span::styled("\u{26A0} ", Style::default().fg(theme.health_warn())),
            Span::styled(label, Style::default().fg(theme.text())),
        ]));
        rows_left -= 1;
        // What has historically worked for this failure, from measured
        // outcomes. Indented under its alert, dim: context, not a new alarm.
        if let Some(ref note) = alert.effectiveness {
            if rows_left == 0 {
                break;
            }
            lines.push(Line::from(Span::styled(
                format!("  {}", truncate(note, max_label)),
                Style::default().fg(theme.subtext()),
            )));
            rows_left -= 1;
        }
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().fg(theme.text())),
        area,
    );
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn section_label<'a>(title: &'static str, theme: &Theme) -> Line<'a> {
    Line::from(Span::styled(title, Style::default().fg(theme.get("blue"))))
}

fn divider(width: u16, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        "\u{2500}".repeat(width as usize),
        Style::default().fg(theme.surface()),
    ))
}

pub fn health_color(score: f64, theme: &Theme) -> ratatui::style::Color {
    if score >= 80.0 {
        theme.health_ok()
    } else if score >= 50.0 {
        theme.health_warn()
    } else {
        theme.health_crit()
    }
}

fn health_band(score: f64) -> &'static str {
    if score >= 80.0 {
        "HEALTHY"
    } else if score >= 50.0 {
        "CAUTION"
    } else {
        "CRITICAL"
    }
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

fn trend_arrow(trend: CostTrend, theme: &Theme) -> (&'static str, ratatui::style::Color) {
    match trend {
        CostTrend::Accelerating => ("\u{2191}", theme.health_warn()),
        CostTrend::Stable => ("", theme.subtext()),
        CostTrend::Decelerating => ("\u{2193}", theme.health_ok()),
    }
}

fn status_indicator(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Running => "\u{25CF}",
        AgentStatus::Idle => "\u{25CB}",
        AgentStatus::Paused => "\u{23F8}",
        AgentStatus::Error => "\u{2717}",
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
