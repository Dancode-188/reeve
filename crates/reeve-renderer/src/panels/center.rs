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

    if let Some(tv) = state.trace.as_ref() {
        frame.render_widget(
            TraceTree {
                children: &tv.children,
                names: &tv.names,
                collapsed: &tv.collapsed,
                span_health_scores: &tv.span_health_scores,
                outcome_lines: &tv.outcome_lines,
                annotated: &tv.notes,
                root: tv.root.as_ref(),
                orphans: &tv.orphans,
                selected: tv.selected.as_ref(),
                scroll: tv.scroll,
                title,
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
        frame.render_widget(
            TraceTree {
                children: &empty,
                names: &empty_names,
                collapsed: &empty_collapsed,
                span_health_scores: &empty_scores,
                outcome_lines: &[],
                annotated: &empty_notes,
                root: None,
                orphans: &[],
                selected: None,
                scroll: 0,
                title,
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
                focused,
                theme,
                ascii,
            },
            sa,
        );
    }
}
