use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
};

pub fn render(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    right_hidden: bool,
    left_hidden: bool,
    focus_mode: bool,
) {
    if area.height == 0 {
        return;
    }

    let chrome_style = Style::default().bg(theme.chrome_bg());
    frame.render_widget(Block::default().style(chrome_style), area);

    let kb = Style::default()
        .fg(theme.get("blue"))
        .add_modifier(Modifier::BOLD);
    let action = Style::default().fg(theme.subtext());
    let bracket = Style::default().fg(theme.subtext());
    let warn = Style::default().fg(theme.health_warn());

    let groups: Vec<Line> = if focus_mode {
        vec![
            key_group("[\u{5B}/\u{5D}]", "traces", &kb, &action, &bracket),
            key_group("[j/k]", "nav", &kb, &action, &bracket),
            key_group("[1]", "fleet", &kb, &action, &bracket),
            key_group("[?]", "help", &kb, &action, &bracket),
            key_group("[q]", "quit", &kb, &action, &bracket),
        ]
    } else if left_hidden {
        vec![
            key_group("[j/k]", "nav", &kb, &action, &bracket),
            key_group("[?]", "help", &kb, &action, &bracket),
            key_group("[q]", "quit", &kb, &action, &bracket),
        ]
    } else {
        let mut g = vec![
            key_group("[j/k]", "nav", &kb, &action, &bracket),
            key_group("[h/l]", "panels", &kb, &action, &bracket),
            key_group("[Enter]", "fold", &kb, &action, &bracket),
            key_group("[2]", "focus", &kb, &action, &bracket),
            key_group("[?]", "help", &kb, &action, &bracket),
            key_group("[q]", "quit", &kb, &action, &bracket),
        ];
        if right_hidden {
            g.push(Line::from(Span::styled("SPAN \u{25B7}", warn)));
        }
        g
    };

    let n = groups.len();
    let constraints: Vec<Constraint> = (0..n).map(|_| Constraint::Fill(1)).collect();
    let chunks = Layout::horizontal(constraints).split(area);

    for (chunk, line) in chunks.iter().zip(groups) {
        frame.render_widget(
            Paragraph::new(line)
                .alignment(Alignment::Center)
                .style(chrome_style),
            *chunk,
        );
    }
}

fn key_group<'a>(
    key: &'a str,
    label: &'a str,
    kb: &'a Style,
    action: &'a Style,
    bracket: &'a Style,
) -> Line<'a> {
    let open = &key[..1];
    let close = &key[key.len() - 1..];
    let inner = &key[1..key.len() - 1];
    Line::from(vec![
        Span::styled(open, *bracket),
        Span::styled(inner, *kb),
        Span::styled(close, *bracket),
        Span::raw(" "),
        Span::styled(label, *action),
    ])
}
