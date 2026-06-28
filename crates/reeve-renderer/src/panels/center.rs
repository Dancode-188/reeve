use crate::app::AppState;
use crate::ascii::AsciiMode;
use crate::theme::Theme;
use crate::widgets::{StreamingBox, TraceTree};
use reeve_model::ids::SpanId;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
};
use std::collections::HashMap;

pub fn render(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme, ascii: &AsciiMode) {
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

    if let Some(tv) = state.trace.as_ref() {
        frame.render_widget(
            TraceTree {
                children: &tv.children,
                root: tv.root.as_ref(),
                selected: tv.selected.as_ref(),
                scroll: tv.scroll,
                title: "TRACE",
                focused,
                theme,
                ascii,
            },
            tree_area,
        );
    } else {
        let empty: HashMap<SpanId, Vec<SpanId>> = HashMap::new();
        frame.render_widget(
            TraceTree {
                children: &empty,
                root: None,
                selected: None,
                scroll: 0,
                title: "TRACE",
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
