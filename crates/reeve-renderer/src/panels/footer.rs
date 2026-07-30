use crate::app::ViewMode;
use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Flex, Layout, Rect},
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
    view_mode: ViewMode,
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

    let mut span_hint = false;
    let groups: Vec<Line> = if view_mode == ViewMode::Focus {
        vec![
            key_group("[\u{5B}/\u{5D}]", "traces", &kb, &action, &bracket),
            key_group("[j/k]", "nav", &kb, &action, &bracket),
            key_group("[1]", "fleet", &kb, &action, &bracket),
            key_group("[?]", "help", &kb, &action, &bracket),
            key_group("[q]", "quit", &kb, &action, &bracket),
        ]
    } else if view_mode == ViewMode::Cost {
        vec![
            key_group("[1]", "fleet", &kb, &action, &bracket),
            key_group("[3]", "history", &kb, &action, &bracket),
            key_group("[?]", "help", &kb, &action, &bracket),
            key_group("[q]", "quit", &kb, &action, &bracket),
        ]
    } else if view_mode == ViewMode::History {
        vec![
            key_group("[j/k]", "nav", &kb, &action, &bracket),
            key_group("[Enter]", "detail", &kb, &action, &bracket),
            key_group("[R]", "replay", &kb, &action, &bracket),
            key_group("[W]", "impact", &kb, &action, &bracket),
            key_group("[d]", "delete", &kb, &action, &bracket),
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
            key_group("[3]", "history", &kb, &action, &bracket),
            key_group("[4]", "cost", &kb, &action, &bracket),
            key_group("[?]", "help", &kb, &action, &bracket),
            key_group("[q]", "quit", &kb, &action, &bracket),
        ];
        if right_hidden {
            g.push(Line::from(Span::styled("SPAN \u{25B7}", warn)));
            span_hint = true;
        }
        g
    };

    // Each group takes the width it actually needs and the spare columns become
    // the gaps between them. Equal shares looked tidier until the terminal
    // narrowed, when they started cutting the longer labels in half.
    //
    // Whole labels are not enough on their own, so one column is reserved after
    // every group but the last. When even that will not fit, groups come off in
    // reverse order of usefulness: the SPAN hint first, being a signpost rather
    // than a key you can press, then the view switches from the right, leaving
    // [?] help and [q] quit standing longest.
    let mut groups = groups;
    if span_hint && required_width(&groups) > area.width {
        groups.pop();
    }
    while groups.len() > 3 && required_width(&groups) > area.width {
        groups.remove(groups.len() - 3);
    }
    while groups.len() > 1 && required_width(&groups) > area.width {
        groups.pop();
    }

    let last = groups.len() - 1;
    let constraints: Vec<Constraint> = groups
        .iter()
        .enumerate()
        .map(|(i, g)| Constraint::Length(g.width() as u16 + u16::from(i != last)))
        .collect();
    let chunks = Layout::horizontal(constraints)
        .flex(Flex::SpaceBetween)
        .split(area);

    for (chunk, line) in chunks.iter().zip(groups) {
        frame.render_widget(
            Paragraph::new(line)
                .alignment(Alignment::Left)
                .style(chrome_style),
            *chunk,
        );
    }
}

/// What the bar needs to show these groups with a column between each pair.
fn required_width(groups: &[Line]) -> u16 {
    let text: u16 = groups.iter().map(|g| g.width() as u16).sum();
    text + groups.len().saturating_sub(1) as u16
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    const JOINS: [&str; 7] = [
        "nav[", "history[", "quitSPAN", "fold[", "focus[", "cost[", "help[",
    ];
    const LABELS: [&str; 8] = [
        "nav", "panels", "fold", "focus", "history", "cost", "help", "quit",
    ];

    fn footer_text(width: u16, right_hidden: bool) -> String {
        let theme = Theme::load();
        let backend = TestBackend::new(width, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    &theme,
                    right_hidden,
                    false,
                    ViewMode::Fleet,
                );
            })
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
    fn groups_never_run_into_each_other() {
        for width in 60u16..=200 {
            let bar = footer_text(width, true);
            for join in JOINS {
                assert!(
                    !bar.contains(join),
                    "{width} columns ran groups together at {join:?}: {bar:?}"
                );
            }
        }
    }

    #[test]
    fn every_label_survives_once_the_bar_is_wide_enough() {
        // The case that used to render "panels" as "panel" and "fold" as "fol".
        for width in 84u16..=200 {
            let bar = footer_text(width, true);
            for label in LABELS {
                assert!(
                    bar.contains(label),
                    "{label} missing at {width} columns: {bar:?}"
                );
            }
        }
    }

    #[test]
    fn a_cramped_bar_drops_groups_instead_of_letters() {
        // 80 is the real cramped case: the left panel appears at 80 and the
        // right one hides below 120, so the nine-group bar has to shed
        // something here. It sheds whole groups, and never the way out.
        let bar = footer_text(80, true);
        assert!(bar.contains("quit"), "quit should survive: {bar:?}");
        assert!(bar.contains("help"), "help should survive: {bar:?}");
        assert!(
            LABELS.iter().filter(|l| bar.contains(**l)).count() < LABELS.len(),
            "expected a group to be dropped at 80 columns: {bar:?}"
        );
    }
}
