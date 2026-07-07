use crate::app::OutcomeLine;
use crate::ascii::AsciiMode;
use crate::panels::left::health_color;
use crate::theme::Theme;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};
use reeve_model::ids::SpanId;
use std::collections::{HashMap, HashSet};

pub struct TraceTree<'a> {
    pub children: &'a HashMap<SpanId, Vec<SpanId>>,
    pub names: &'a HashMap<SpanId, String>,
    pub collapsed: &'a HashSet<SpanId>,
    pub span_health_scores: &'a HashMap<SpanId, f64>,
    pub outcome_lines: &'a [OutcomeLine],
    /// Spans carrying a developer note; they get the annotation indicator.
    pub annotated: &'a HashMap<SpanId, String>,
    /// Active filter text. Non-matching rows dim rather than disappear so
    /// tree connectors stay truthful.
    pub filter: Option<&'a str>,
    /// Span attribute lookup for filter matching.
    pub spans: &'a HashMap<SpanId, reeve_model::entity::span::InternalSpan>,
    pub root: Option<&'a SpanId>,
    /// Spans not reachable from the root: arrived before their parent. They
    /// render as flat rows labeled as awaiting it, per the live-view rule
    /// that an arrived span is never invisible.
    pub orphans: &'a [SpanId],
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
        } else if self.orphans.is_empty() {
            lines.push(Line::from(Span::styled(
                " no trace selected",
                Style::default().fg(self.theme.subtext()),
            )));
        }
        for orphan in self.orphans {
            let name = self.names.get(orphan).map(String::as_str).unwrap_or("span");
            lines.push(Line::from(vec![
                Span::styled(format!(" {name} "), Style::default().fg(self.theme.text())),
                Span::styled(
                    "awaiting parent span",
                    Style::default().fg(self.theme.subtext()),
                ),
            ]));
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
        let has_children = self
            .children
            .get(id)
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        let is_collapsed = self.collapsed.contains(id);

        let connector: &str = if is_root {
            self.ascii.tree_open()
        } else if is_last {
            self.ascii.tree_elbow()
        } else {
            self.ascii.tree_tee()
        };

        let is_selected = self.selected == Some(id);
        let dimmed = self.filter.is_some_and(|f| {
            !crate::app::span_matches_filter(self.spans.get(id), self.names.get(id), f)
        });
        let display = self
            .names
            .get(id)
            .map(|s| s.as_str())
            .unwrap_or_else(|| id.as_str());
        let fold = if has_children && is_collapsed {
            if self.ascii.enabled() {
                " >"
            } else {
                " \u{25B8}"
            }
        } else {
            ""
        };
        let label = format!("{}{}{}{}", prefix, connector, display, fold);

        let label_style = if is_selected {
            Style::default()
                .bg(self.theme.highlight())
                .fg(self.theme.background())
                .add_modifier(Modifier::BOLD)
        } else if dimmed {
            Style::default()
                .fg(self.theme.subtext())
                .add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(self.theme.text())
        };

        let mut spans: Vec<Span<'static>> = vec![Span::styled(label, label_style)];

        // Quality badge for spans that have a health score
        if let Some(&score) = self.span_health_scores.get(id) {
            let hcolor = health_color(score, self.theme);
            let star = if self.ascii.enabled() {
                "*"
            } else {
                "\u{2605}"
            };
            let score_str = format!("{}", score.round() as u32);
            spans.push(Span::styled(
                " [",
                Style::default().fg(self.theme.subtext()),
            ));
            spans.push(Span::styled(
                format!("{}{}", star, score_str),
                Style::default().fg(hcolor).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled("]", Style::default().fg(self.theme.subtext())));
        }

        if self.annotated.contains_key(id) {
            let diamond = if self.ascii.enabled() {
                " [n]"
            } else {
                " \u{2666}"
            };
            spans.push(Span::styled(
                diamond.to_string(),
                Style::default().fg(self.theme.get("blue")),
            ));
        }

        lines.push(Line::from(spans));

        // Outcome lines for this span (or root-level outcomes when span_id is None and is_root).
        let outcome_style = Style::default()
            .fg(self.theme.span_active())
            .add_modifier(Modifier::ITALIC);
        for ol in self.outcome_lines {
            let matches = match &ol.span_id {
                Some(sid) => sid == id,
                None => is_root,
            };
            if matches {
                let arrow = if self.ascii.enabled() {
                    "\\>"
                } else {
                    "\u{21B3}"
                };
                let indent = format!("{}  ", prefix);
                lines.push(Line::from(vec![
                    Span::styled(indent, outcome_style),
                    Span::styled(arrow, outcome_style),
                    Span::styled(" ", outcome_style),
                    Span::styled(ol.text.clone(), outcome_style),
                ]));
            }
        }

        if is_collapsed {
            return;
        }

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

        let collapsed = HashSet::new();
        let scores: HashMap<SpanId, f64> = HashMap::new();
        terminal
            .draw(|frame| {
                let theme = make_theme();
                let ascii = make_ascii();
                let annotated: HashMap<SpanId, String> = HashMap::new();
                let empty_spans: HashMap<SpanId, reeve_model::entity::span::InternalSpan> =
                    HashMap::new();
                let widget = TraceTree {
                    annotated: &annotated,
                    filter: None,
                    spans: &empty_spans,
                    children: &children,
                    names: &names,
                    collapsed: &collapsed,
                    span_health_scores: &scores,
                    outcome_lines: &[],
                    root: Some(&root),
                    orphans: &[],
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
        assert!(
            content.contains('└') || content.contains('├'),
            "tree connector must appear"
        );
    }

    #[test]
    fn outcome_line_renders_below_root_when_span_id_is_none() {
        let mut children: HashMap<SpanId, Vec<SpanId>> = HashMap::new();
        let mut names: HashMap<SpanId, String> = HashMap::new();
        let root: SpanId = "root-span".into();
        children.insert(root.clone(), vec![]);
        names.insert(root.clone(), "root-span".to_string());

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).unwrap();

        let collapsed = HashSet::new();
        let scores: HashMap<SpanId, f64> = HashMap::new();
        let outcomes = vec![OutcomeLine {
            span_id: None,
            text: "redirect +0.35 quality \u{00B7} 3 spans".to_string(),
        }];

        terminal
            .draw(|frame| {
                let theme = make_theme();
                let ascii = make_ascii();
                let annotated: HashMap<SpanId, String> = HashMap::new();
                let empty_spans: HashMap<SpanId, reeve_model::entity::span::InternalSpan> =
                    HashMap::new();
                let widget = TraceTree {
                    annotated: &annotated,
                    filter: None,
                    spans: &empty_spans,
                    children: &children,
                    names: &names,
                    collapsed: &collapsed,
                    span_health_scores: &scores,
                    outcome_lines: &outcomes,
                    root: Some(&root),
                    orphans: &[],
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
        assert!(
            content.contains("redirect") && content.contains("+0.35"),
            "outcome line must appear below root span"
        );
    }

    #[test]
    fn orphans_render_without_a_root() {
        // Mid-replay reality: children arrive before their parent, so spans
        // exist with no root. They must render as flat awaiting rows, not
        // vanish behind "no trace selected".
        let children: HashMap<SpanId, Vec<SpanId>> = HashMap::new();
        let mut names: HashMap<SpanId, String> = HashMap::new();
        let orphan: SpanId = "tool-span".into();
        names.insert(orphan.clone(), "gen_ai.tool:search".to_string());
        let orphans = vec![orphan];

        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).unwrap();

        let collapsed = HashSet::new();
        let scores: HashMap<SpanId, f64> = HashMap::new();
        terminal
            .draw(|frame| {
                let theme = make_theme();
                let ascii = make_ascii();
                let annotated: HashMap<SpanId, String> = HashMap::new();
                let empty_spans: HashMap<SpanId, reeve_model::entity::span::InternalSpan> =
                    HashMap::new();
                let widget = TraceTree {
                    annotated: &annotated,
                    filter: None,
                    spans: &empty_spans,
                    children: &children,
                    names: &names,
                    collapsed: &collapsed,
                    span_health_scores: &scores,
                    outcome_lines: &[],
                    root: None,
                    orphans: &orphans,
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

        assert!(content.contains("gen_ai.tool:search"), "orphan must appear");
        assert!(
            content.contains("awaiting parent span"),
            "orphan must be labeled as awaiting its parent"
        );
        assert!(
            !content.contains("no trace selected"),
            "arrived spans mean the empty-state message is wrong"
        );
    }
}
