use crate::app::{AppState, InterventionOverlayState, OverlayCommand, OverlayMode};
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

    let caps = state
        .agent_capabilities
        .get(&ov.agent_id)
        .cloned()
        .unwrap_or_default();

    match &ov.mode {
        OverlayMode::Menu => render_menu(frame, area, ov, &caps, theme),
        OverlayMode::TextInput { command, buffer } => {
            render_text_input(frame, area, ov, *command, buffer, theme)
        }
        OverlayMode::KillConfirm => render_kill_confirm(frame, area, ov, theme),
    }
}

fn render_menu(
    frame: &mut Frame,
    area: Rect,
    ov: &InterventionOverlayState,
    caps: &[String],
    theme: &Theme,
) {
    let popup = centered(52, 12, area);
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

    let lines = vec![
        Line::raw(""),
        cmd_row("p", "Pause / Resume", "pause"),
        cmd_row("r", "Redirect", "redirect"),
        cmd_row("c", "Inject Context", "inject_context"),
        cmd_row("k", "Kill", "kill"),
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("[Enter]", hint),
            Span::styled(" dispatch", hint),
            Span::raw("    "),
            Span::styled("[Esc]", hint),
            Span::styled(" cancel", hint),
        ]),
        Line::raw(""),
    ];

    let title = format!(" INTERVENE: {} ", ov.agent_id);
    let block = Block::default()
        .title(title)
        .title_style(Style::default().fg(theme.highlight()).add_modifier(Modifier::BOLD))
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
    let popup = centered(52, 10, area);

    let label = match command {
        OverlayCommand::Redirect => "Redirect instruction:",
        OverlayCommand::InjectContext => "Context to inject:",
        _ => "Input:",
    };

    let prompt = format!("> {buffer}_");
    let hint = Style::default().fg(theme.subtext());
    let text_style = Style::default().fg(theme.text());

    let lines = vec![
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(label, text_style),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(&prompt, Style::default().fg(theme.highlight())),
        ]),
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

    let title = format!(" INTERVENE: {} ", ov.agent_id);
    let block = Block::default()
        .title(title)
        .title_style(Style::default().fg(theme.highlight()).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focused()));

    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

fn render_kill_confirm(
    frame: &mut Frame,
    area: Rect,
    ov: &InterventionOverlayState,
    theme: &Theme,
) {
    let popup = centered(52, 8, area);

    let warn = Style::default().fg(theme.health_crit());
    let hint = Style::default().fg(theme.subtext());

    let lines = vec![
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("Kill this agent? This cannot be undone.", warn),
        ]),
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
        .title_style(Style::default().fg(theme.health_crit()).add_modifier(Modifier::BOLD))
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
