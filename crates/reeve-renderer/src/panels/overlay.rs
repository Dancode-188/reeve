use crate::app::{
    AppState, InterventionOverlayState, OverlayCommand, OverlayMode, SuggestedIntervention,
    TEMPLATES,
};
use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    let Some(ref ov) = state.overlay else {
        return;
    };

    let caps = state.effective_capabilities(&ov.agent_id);

    match &ov.mode {
        OverlayMode::Menu => render_menu(
            frame,
            area,
            ov,
            &caps,
            state.active_suggestion.as_ref(),
            theme,
        ),
        OverlayMode::TextInput { command, buffer } => {
            render_text_input(frame, area, ov, *command, buffer, theme)
        }
        OverlayMode::KillConfirm => render_kill_confirm(frame, area, ov, state, theme),
    }
}

fn render_menu(
    frame: &mut Frame,
    area: Rect,
    ov: &InterventionOverlayState,
    caps: &[String],
    suggestion: Option<&SuggestedIntervention>,
    theme: &Theme,
) {
    let height = if suggestion.is_some() { 20 } else { 15 };
    let popup = centered(54, height, area);
    let has = |cap: &str| caps.contains(&cap.to_string());

    let key_style = Style::default().fg(theme.highlight());
    let active = Style::default().fg(theme.text());
    let dimmed = Style::default().fg(theme.subtext());
    let hint = Style::default().fg(theme.subtext());

    let cmd_row = |key: &'static str, label: &'static str, cap: &str| {
        let style = if has(cap) { active } else { dimmed };
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("[{key}]"), key_style),
            Span::raw("  "),
            Span::styled(label, style),
        ])
    };

    let mut lines: Vec<Line> = Vec::new();

    if let Some(s) = suggestion {
        let cmd_label = match s.command {
            OverlayCommand::Redirect => "Redirect",
            OverlayCommand::InjectContext => "Inject Context",
            OverlayCommand::Pause => "Pause",
            OverlayCommand::Kill => "Kill",
        };
        let warn = Style::default().fg(theme.health_warn());
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("SUGGESTED", warn.add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(cmd_label, Style::default().fg(theme.subtext())),
        ]));
        // Truncate text to fit popup width (54 - 4 border/padding = 50 chars).
        let display_text = if s.text.len() > 50 {
            format!("{}…", &s.text[..49])
        } else {
            s.text.clone()
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(display_text, Style::default().fg(theme.text())),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("[Enter]", hint),
            Span::styled(" apply", hint),
            Span::raw("  "),
            Span::styled("[Tab]", hint),
            Span::styled(" edit", hint),
            Span::raw("  "),
            Span::styled("[Esc]", hint),
            Span::styled(" skip", hint),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("─".repeat(48), Style::default().fg(theme.surface())),
        ]));
    }

    lines.push(Line::raw(""));
    lines.push(cmd_row("p", "Pause / Resume", "pause"));
    lines.push(cmd_row("r", "Redirect", "redirect"));
    lines.push(cmd_row("c", "Inject Context", "inject_context"));
    lines.push(cmd_row("k", "Kill", "kill"));
    lines.push(Line::raw(""));
    for t in TEMPLATES {
        let style = if has("redirect") { active } else { dimmed };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("[{}]", t.key), key_style),
            Span::raw("  "),
            Span::styled(t.label, style),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("[Enter]", hint),
        Span::styled(" dispatch", hint),
        Span::raw("    "),
        Span::styled("[Esc]", hint),
        Span::styled(" cancel", hint),
    ]));
    lines.push(Line::raw(""));

    let title = format!(" INTERVENE: {} ", ov.agent_id);
    let block = Block::default()
        .title(title)
        .title_style(
            Style::default()
                .fg(theme.highlight())
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focused()));

    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

fn render_text_input(
    frame: &mut Frame,
    area: Rect,
    ov: &InterventionOverlayState,
    command: OverlayCommand,
    buffer: &str,
    theme: &Theme,
) {
    let label = match command {
        OverlayCommand::Redirect => "Redirect instruction:",
        OverlayCommand::InjectContext => "Context to inject:",
        _ => "Input:",
    };

    // The buffer wraps to the popup width and the popup grows with it:
    // the operator must see everything they are about to send. Inner
    // width is 54 minus borders and the two-space indent.
    let prompt = format!("> {buffer}_");
    let inner = 54usize - 2 - 2;
    let prompt_rows: Vec<String> = {
        let chars: Vec<char> = prompt.chars().collect();
        chars.chunks(inner).map(|c| c.iter().collect()).collect()
    };
    let popup = centered(54, 10 + prompt_rows.len().saturating_sub(1) as u16, area);

    let hint = Style::default().fg(theme.subtext());
    let text_style = Style::default().fg(theme.text());

    let mut lines = vec![
        Line::raw(""),
        Line::from(vec![Span::raw("  "), Span::styled(label, text_style)]),
    ];
    for row in &prompt_rows {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(row.clone(), Style::default().fg(theme.highlight())),
        ]));
    }
    let mut rest = vec![
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("[Enter]", hint),
            Span::styled(" send", hint),
            Span::raw("    "),
            Span::styled("[Esc]", hint),
            Span::styled(" back", hint),
        ]),
        Line::raw(""),
    ];
    lines.append(&mut rest);

    let title = format!(" INTERVENE: {} ", ov.agent_id);
    let block = Block::default()
        .title(title)
        .title_style(
            Style::default()
                .fg(theme.highlight())
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focused()));

    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

fn render_kill_confirm(
    frame: &mut Frame,
    area: Rect,
    ov: &InterventionOverlayState,
    state: &AppState,
    theme: &Theme,
) {
    let popup = centered(54, 8, area);

    let warn = Style::default().fg(theme.health_crit());
    let hint = Style::default().fg(theme.subtext());

    // On the proxy path kill is a circuit breaker: it stops API access,
    // not the process, and the confirmation should say which one the
    // developer is about to do.
    let prompt = if state.is_proxy_agent(&ov.agent_id) {
        "Kill this agent? Its API requests will be refused."
    } else {
        "Kill this agent? This cannot be undone."
    };

    let lines = vec![
        Line::raw(""),
        Line::from(vec![Span::raw("  "), Span::styled(prompt, warn)]),
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("[y]", Style::default().fg(theme.health_crit())),
            Span::styled(" confirm", hint),
            Span::raw("    "),
            Span::styled("[n]", hint),
            Span::styled(" cancel", hint),
        ]),
        Line::raw(""),
    ];

    let title = format!(" INTERVENE: {} ", ov.agent_id);
    let block = Block::default()
        .title(title)
        .title_style(
            Style::default()
                .fg(theme.health_crit())
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.health_crit()));

    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
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
