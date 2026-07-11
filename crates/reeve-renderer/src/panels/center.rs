use crate::app::AppState;
use crate::ascii::AsciiMode;
use crate::theme::Theme;
use crate::widgets::{StreamingBox, TraceTree};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
};
use reeve_model::ids::SpanId;
use std::collections::{HashMap, HashSet};

pub fn render(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    ascii: &AsciiMode,
    right_hidden: bool,
) {
    let has_streaming = !state.streaming.content.is_empty();
    let filter_text = state
        .filter_input
        .as_deref()
        .or(state.filter_applied.as_deref())
        .filter(|f| !f.is_empty());

    let (area, filter_area) = if state.filter_input.is_some() && area.height > 3 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Fill(1)])
            .split(area);
        (chunks[1], Some(chunks[0]))
    } else {
        (area, None)
    };
    if let Some(fa) = filter_area {
        let buffer = state.filter_input.as_deref().unwrap_or("");
        frame.render_widget(
            ratatui::widgets::Paragraph::new(ratatui::text::Line::from(vec![
                ratatui::text::Span::styled(
                    " /",
                    ratatui::style::Style::default().fg(theme.get("blue")),
                ),
                ratatui::text::Span::styled(
                    buffer.to_string(),
                    ratatui::style::Style::default().fg(theme.text()),
                ),
                ratatui::text::Span::styled(
                    "\u{258C}",
                    ratatui::style::Style::default().fg(theme.text()),
                ),
                ratatui::text::Span::styled(
                    "  [Tab] next  [Enter] keep  [Esc] clear",
                    ratatui::style::Style::default().fg(theme.subtext()),
                ),
            ])),
            fa,
        );
    }

    let (tree_area, stream_area) = if has_streaming && area.height > 10 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Fill(1), Constraint::Min(6)])
            .split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    };

    let focused = matches!(state.panel_focus, crate::app::PanelFocus::Center);
    let title = if right_hidden {
        "TRACE [SPAN ▷]"
    } else {
        "TRACE"
    };

    // An open turn outranks the loaded completed trace: the cockpit
    // shows what the agent is doing NOW, and falls back to the finished
    // trace when the turn completes and its live view retires.
    if let Some(lv) = state.live_view_for_selected() {
        frame.render_widget(
            TraceTree {
                children: &lv.children,
                names: &lv.names,
                collapsed: &lv.collapsed,
                span_health_scores: &lv.span_health_scores,
                outcome_lines: &[],
                annotated: &lv.notes,
                filter: filter_text,
                spans: &lv.spans,
                root: lv.root.as_ref(),
                orphans: &lv.orphans,
                selected: None,
                scroll: lv.scroll,
                title: if right_hidden {
                    "TRACE · live [SPAN ▷]"
                } else {
                    "TRACE · live"
                },
                live: true,
                focused,
                theme,
                ascii,
            },
            tree_area,
        );
    } else if let Some(tv) = state.trace.as_ref() {
        frame.render_widget(
            TraceTree {
                children: &tv.children,
                names: &tv.names,
                collapsed: &tv.collapsed,
                span_health_scores: &tv.span_health_scores,
                outcome_lines: &tv.outcome_lines,
                annotated: &tv.notes,
                filter: filter_text,
                spans: &tv.spans,
                root: tv.root.as_ref(),
                orphans: &tv.orphans,
                selected: tv.selected.as_ref(),
                scroll: tv.scroll,
                title,
                live: false,
                focused,
                theme,
                ascii,
            },
            tree_area,
        );
    } else {
        let empty: HashMap<SpanId, Vec<SpanId>> = HashMap::new();
        let empty_names: HashMap<SpanId, String> = HashMap::new();
        let empty_collapsed: HashSet<SpanId> = HashSet::new();
        let empty_scores: HashMap<SpanId, f64> = HashMap::new();
        let empty_notes: HashMap<SpanId, String> = HashMap::new();
        let empty_spans: HashMap<SpanId, reeve_model::entity::span::InternalSpan> = HashMap::new();
        frame.render_widget(
            TraceTree {
                children: &empty,
                names: &empty_names,
                collapsed: &empty_collapsed,
                span_health_scores: &empty_scores,
                outcome_lines: &[],
                annotated: &empty_notes,
                filter: None,
                spans: &empty_spans,
                root: None,
                orphans: &[],
                selected: None,
                scroll: 0,
                title,
                live: false,
                focused,
                theme,
                ascii,
            },
            tree_area,
        );
    }

    if let Some(sa) = stream_area {
        let cursor_on = (state.streaming.cursor_tick / 8) % 2 == 0;
        frame.render_widget(
            StreamingBox {
                content: &state.streaming.content,
                cursor_on,
                scroll: state.streaming.scroll,
                auto_scroll: state.streaming.auto_scroll,
                focused,
                theme,
                ascii,
            },
            sa,
        );
    }
}
