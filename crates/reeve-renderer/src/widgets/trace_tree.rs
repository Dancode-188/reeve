use crate::ascii::AsciiMode;
use crate::theme::Theme;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};
use reeve_model::ids::SpanId;
use std::collections::HashMap;

pub struct TraceTree<'a> {
    pub children: &'a HashMap<SpanId, Vec<SpanId>>,
    pub names: &'a HashMap<SpanId, String>,
    pub root: Option<&'a SpanId>,
    pub selected: Option<&'a SpanId>,
    pub scroll: u16,
    pub title: &'a str,
    pub focused: bool,
    pub theme: &'a Theme,
    pub ascii: &'a AsciiMode,
}

impl<'a> Widget for TraceTree<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let border_style = Style::default().fg(if self.focused {
            self.theme.border_focused()
        } else {
            self.theme.border_idle()
        });

        let block = Block::default()
            .title(self.title)
            .borders(Borders::ALL)
            .border_style(border_style);

        let inner = block.inner(area);
        Widget::render(block, area, buf);

        let mut lines: Vec<Line<'static>> = Vec::new();
        if let Some(root) = self.root {
            self.build_lines(root, "", true, true, &mut lines);
        } else {
            lines.push(Line::from(Span::styled(
                " no trace selected",
                Style::default().fg(self.theme.subtext()),
            )));
        }

        Widget::render(Paragraph::new(lines).scroll((self.scroll, 0)), inner, buf);
    }
}

impl<'a> TraceTree<'a> {
    fn build_lines(
        &self,
        id: &SpanId,
        prefix: &str,
        is_root: bool,
        is_last: bool,
        lines: &mut Vec<Line<'static>>,
    ) {
        let connector: &str = if is_root {
            self.ascii.tree_open()
        } else if is_last {
            self.ascii.tree_elbow()
        } else {
            self.ascii.tree_tee()
        };

        let is_selected = self.selected == Some(id);
        let display = self
            .names
            .get(id)
            .map(|s| s.as_str())
            .unwrap_or_else(|| id.as_str());
        let label = format!("{}{}{}", prefix, connector, display);

        let style = if is_selected {
            Style::default()
                .bg(self.theme.highlight())
                .fg(self.theme.background())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.theme.text())
        };

        lines.push(Line::from(Span::styled(label, style)));

        let child_prefix = if is_root {
            prefix.to_string()
        } else if is_last {
            format!("{}   ", prefix)
        } else {
            format!("{}{}", prefix, self.ascii.tree_pipe())
        };

        let children = self.children.get(id).map(|v| v.as_slice()).unwrap_or(&[]);
        for (i, child) in children.iter().enumerate() {
            self.build_lines(child, &child_prefix, false, i == children.len() - 1, lines);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn make_theme() -> Theme {
        Theme::load()
    }

    fn make_ascii() -> AsciiMode {
        AsciiMode::new(false)
    }

    #[test]
    fn tree_renders_parent_child_with_box_drawing() {
        let mut children: HashMap<SpanId, Vec<SpanId>> = HashMap::new();
        let mut names: HashMap<SpanId, String> = HashMap::new();
        let root: SpanId = "root-span".into();
        let child: SpanId = "child-span".into();
        children.insert(root.clone(), vec![child.clone()]);
        names.insert(root.clone(), "root-span".to_string());
        names.insert(child.clone(), "child-span".to_string());

        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                let theme = make_theme();
                let ascii = make_ascii();
                let widget = TraceTree {
                    children: &children,
                    names: &names,
                    root: Some(&root),
                    selected: None,
                    scroll: 0,
                    title: "TRACE",
                    focused: false,
                    theme: &theme,
                    ascii: &ascii,
                };
                frame.render_widget(widget, frame.area());
            })
            .unwrap();

        let content = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect::<String>();

        assert!(content.contains("root-span"), "root span must appear");
        assert!(content.contains("child-span"), "child span must appear");
        // Box-drawing connector for the child
        assert!(
            content.contains('└') || content.contains('├'),
            "tree connector must appear"
        );
    }
}
