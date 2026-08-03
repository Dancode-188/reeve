use crate::app::PendingConfirmation;
use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, Paragraph},
};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn render(
    frame: &mut Frame,
    area: Rect,
    pc: &PendingConfirmation,
    has_alternatives: bool,
    theme: &Theme,
) {
    let has_countdown = pc.auto_confirm_after_secs.is_some();
    let height =
        if has_countdown { 13 } else { 10 } + if pc.command_type.is_some() { 0 } else { 2 };
    let popup = centered(58, height, area);

    let warn = Style::default().fg(theme.health_warn());
    let text = Style::default().fg(theme.text());
    let hint = Style::default().fg(theme.subtext());

    // Truncate description to popup inner width (58 - 4 = 54).
    let description = if pc.description.len() > 54 {
        format!("{}…", &pc.description[..53])
    } else {
        pc.description.clone()
    };

    let rule_display = if pc.rule_id.len() > 54 {
        format!("{}…", &pc.rule_id[..53])
    } else {
        pc.rule_id.clone()
    };

    let mut lines: Vec<Line> = vec![
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(&rule_display, warn.add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![Span::raw("  "), Span::styled(&description, text)]),
        Line::raw(""),
    ];

    // The condition matched either way. What changes is whether there is
    // anything to offer about it: the engine attaches a command only when
    // this agent can run one, so an absent command is reported as the
    // absence it is, never as a suggestion the next line takes back.
    // ADR-0045.
    match &pc.command_type {
        Some(cmd) => lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("Suggested action: ", hint),
            Span::styled(cmd, text.add_modifier(Modifier::BOLD)),
        ])),
        None => {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("No action suggested", warn.add_modifier(Modifier::BOLD)),
            ]));
            lines.push(Line::raw(""));
            let why = if has_alternatives {
                "this rule's command is not one this agent takes"
            } else {
                "Reeve has no control channel to this agent"
            };
            lines.push(Line::from(vec![Span::raw("  "), Span::styled(why, hint)]));
        }
    }
    lines.push(Line::raw(""));

    if has_countdown {
        lines.push(Line::raw(""));
    }

    if pc.command_type.is_some() {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("[Enter]", hint),
            Span::styled(" confirm", hint),
            Span::raw("    "),
            Span::styled("[Esc]", hint),
            Span::styled(" dismiss", hint),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("[Enter]", hint),
            Span::styled(
                if has_alternatives {
                    " see what it does take"
                } else {
                    " open intervene"
                },
                hint,
            ),
            Span::raw("    "),
            Span::styled("[Esc]", hint),
            Span::styled(" dismiss", hint),
        ]));
    }
    lines.push(Line::raw(""));

    let block = Block::default()
        .title(" POLICY ALERT ")
        .title_style(warn.add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(warn);

    frame.render_widget(Clear, popup);

    if has_countdown {
        if let Some(secs_total) = pc.auto_confirm_after_secs {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            let elapsed_ms = (now_ms - pc.arrived_at_ms).max(0);
            let total_ms = secs_total as i64 * 1000;
            let remaining_ms = (total_ms - elapsed_ms).max(0);
            let remaining_secs = (remaining_ms / 1000) as u64;
            let ratio = if total_ms > 0 {
                remaining_ms as f64 / total_ms as f64
            } else {
                0.0
            };
            let ratio = ratio.clamp(0.0, 1.0);

            // Split popup into text area and gauge row.
            let inner = block.inner(popup);
            frame.render_widget(block, popup);

            let [text_area, gauge_area] =
                Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);

            frame.render_widget(Paragraph::new(lines), text_area);

            let label = format!("auto-confirm in {remaining_secs}s");
            let gauge = Gauge::default()
                .gauge_style(Style::default().fg(theme.health_warn()))
                .ratio(ratio)
                .label(label);
            frame.render_widget(gauge, gauge_area);
        }
    } else {
        frame.render_widget(Paragraph::new(lines).block(block), popup);
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use reeve_model::ids::AgentId;

    fn modal(command_type: Option<&str>, has_alternatives: bool) -> String {
        let pc = PendingConfirmation {
            agent_id: AgentId::from("agent-1"),
            rule_id: "builtin_low_health".to_string(),
            description: "Agent health score is critical.".to_string(),
            command_type: command_type.map(str::to_string),
            auto_confirm_after_secs: None,
            arrived_at_ms: 0,
        };
        let theme = Theme::load();
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &pc, has_alternatives, &theme))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn an_available_command_is_offered_for_confirmation() {
        let m = modal(Some("pause"), true);
        assert!(m.contains("Suggested action:"), "{m:?}");
        assert!(m.contains("pause"), "{m:?}");
        assert!(m.contains("confirm"), "{m:?}");
    }

    /// Issues #274 and #296. The modal used to propose an action and then
    /// deny it one line later; whatever else it says, it must never do both.
    #[test]
    fn an_unavailable_command_is_never_both_suggested_and_denied() {
        for alternatives in [true, false] {
            let m = modal(None, alternatives);
            assert!(
                !m.contains("Suggested action:"),
                "nothing is available, so nothing may be suggested: {m:?}"
            );
            assert!(m.contains("No action suggested"), "{m:?}");
        }
    }

    #[test]
    fn the_reason_matches_what_the_agent_can_still_do() {
        // A proxy agent cannot be paused but can be redirected or killed,
        // so telling it there is no control channel would be a lie.
        let with_options = modal(None, true);
        assert!(
            with_options.contains("not one this agent takes"),
            "{with_options:?}"
        );
        assert!(
            !with_options.contains("no control channel"),
            "this agent has one: {with_options:?}"
        );

        // An agent that never opened a control channel has nothing at all.
        let without = modal(None, false);
        assert!(without.contains("no control channel"), "{without:?}");
    }
}
