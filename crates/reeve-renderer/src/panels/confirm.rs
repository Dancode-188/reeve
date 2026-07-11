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

pub fn render(frame: &mut Frame, area: Rect, pc: &PendingConfirmation, theme: &Theme) {
    let has_countdown = pc.auto_confirm_after_secs.is_some();
    let height = if has_countdown { 13 } else { 10 } + if pc.supported { 0 } else { 2 };
    let popup = centered(58, height, area);

    let warn = Style::default().fg(theme.health_warn());
    let text = Style::default().fg(theme.text());
    let hint = Style::default().fg(theme.subtext());

    let cmd_display = &pc.command_type;

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
        Line::from(vec![
            Span::raw("  "),
            Span::styled("Suggested action: ", hint),
            Span::styled(cmd_display, text.add_modifier(Modifier::BOLD)),
        ]),
        Line::raw(""),
    ];

    // The target cannot apply the suggested command (a proxy agent has
    // no pause): say so, and offer the intervention menu instead of a
    // confirm that would dispatch a dead letter.
    if !pc.supported {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("this agent does not support {cmd_display}"), warn),
        ]));
        lines.push(Line::raw(""));
    }

    if has_countdown {
        lines.push(Line::raw(""));
    }

    if pc.supported {
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
            Span::styled(" intervene instead", hint),
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
