use crate::app::{AppState, ViewMode};
use crate::layout::Panels;
use reeve_model::ids::SpanId;

/// What a mouse event should do, resolved against the current layout.
/// Hit-testing lives here, away from both the input task (which knows no
/// layout) and the render code (which should not handle input).
#[derive(Debug, PartialEq)]
pub enum MouseTarget {
    SelectAgent(usize),
    SelectSpan(SpanId),
    /// The already-selected span clicked again: fold/unfold.
    ToggleSpan(SpanId),
    SelectHistoryRow(usize),
    ScrollPanel {
        left: bool,
        center: bool,
        up: bool,
    },
    /// Scrubber click during replay: seek to this fraction of the timeline.
    Seek(f64),
    None,
}

/// Resolves a click at (x, y) against the panel rects and current state.
pub fn click_target(
    state: &AppState,
    panels: &Panels,
    footer_y: u16,
    x: u16,
    y: u16,
) -> MouseTarget {
    // Scrubber first: during replay the footer row is the timeline.
    if state.replay.is_some() && y == footer_y {
        let width =
            f64::from((panels.left.width + panels.center.width + panels.right.width).max(1));
        return MouseTarget::Seek((f64::from(x) / width).clamp(0.0, 1.0));
    }

    if in_rect(&panels.left, x, y) && state.view_mode != ViewMode::Focus {
        // Left panel rows: AGENTS label on the first row, then two rows per
        // agent.
        let row = y.saturating_sub(panels.left.y);
        if row >= 1 {
            let idx = ((row - 1) / 2) as usize;
            if idx < state.agents.len() {
                return MouseTarget::SelectAgent(idx);
            }
        }
        return MouseTarget::None;
    }

    if in_rect(&panels.center, x, y) {
        if state.view_mode == ViewMode::History && state.replay.is_none() {
            // HISTORY header on the first row, entries follow, window
            // scrolled to keep the selection visible.
            let row = y.saturating_sub(panels.center.y) as usize;
            if row >= 1 {
                let visible = panels.center.height.saturating_sub(1) as usize;
                let start = state
                    .history_selected
                    .saturating_sub(visible.saturating_sub(1));
                let idx = start + row - 1;
                if idx < state.history_entries.len() {
                    return MouseTarget::SelectHistoryRow(idx);
                }
            }
            return MouseTarget::None;
        }
        // Trace tree: border+title row, then one line per visible row.
        // Outcome annotation lines interleave with span rows, so the click
        // row maps through the same row list the tree draws. Hits resolve
        // against whichever tree the panel is showing, live or loaded.
        let Some(tv) = state.center_view() else {
            return MouseTarget::None;
        };
        if y <= panels.center.y {
            return MouseTarget::None;
        }
        let inner_row = y - panels.center.y - 1;
        let line = inner_row as usize + tv.scroll as usize;
        let rows = visible_tree_rows(tv);
        if let Some(Some(span_id)) = rows.get(line) {
            if tv.selected.as_ref() == Some(span_id) {
                return MouseTarget::ToggleSpan(span_id.clone());
            }
            return MouseTarget::SelectSpan(span_id.clone());
        }
        return MouseTarget::None;
    }

    MouseTarget::None
}

/// Wheel scroll resolves to whichever panel is under the cursor.
pub fn scroll_target(panels: &Panels, x: u16, y: u16, up: bool) -> MouseTarget {
    if in_rect(&panels.left, x, y) {
        MouseTarget::ScrollPanel {
            left: true,
            center: false,
            up,
        }
    } else if in_rect(&panels.center, x, y) {
        MouseTarget::ScrollPanel {
            left: false,
            center: true,
            up,
        }
    } else if in_rect(&panels.right, x, y) {
        MouseTarget::ScrollPanel {
            left: false,
            center: false,
            up,
        }
    } else {
        MouseTarget::None
    }
}

/// The rows the trace tree draws, in order, each carrying the span it
/// belongs to (None for outcome annotation lines). Mirrors the emission
/// order of the tree widget: every span row is followed by its matching
/// outcome lines, and orphans trail at the end via span_order.
pub fn visible_tree_rows(tv: &crate::app::TraceView) -> Vec<Option<SpanId>> {
    let mut rows = Vec::new();
    for id in &tv.span_order {
        rows.push(Some(id.clone()));
        let is_root = tv.root.as_ref() == Some(id);
        for ol in &tv.outcome_lines {
            let matches = match &ol.span_id {
                Some(sid) => sid == id,
                None => is_root,
            };
            if matches {
                rows.push(None);
            }
        }
    }
    rows
}

fn in_rect(rect: &ratatui::layout::Rect, x: u16, y: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && x >= rect.x
        && x < rect.x + rect.width
        && y >= rect.y
        && y < rect.y + rect.height
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{OutcomeLine, TraceView};
    use ratatui::layout::Rect;
    use std::collections::{HashMap, HashSet};

    fn panels() -> Panels {
        Panels {
            left: Rect::new(0, 1, 22, 40),
            center: Rect::new(22, 1, 100, 40),
            right: Rect::new(122, 1, 28, 40),
        }
    }

    fn tree_view() -> TraceView {
        let root: SpanId = "root".into();
        let child: SpanId = "child".into();
        TraceView {
            trace_id: "t1".into(),
            root: Some(root.clone()),
            spans: HashMap::new(),
            children: HashMap::new(),
            names: HashMap::new(),
            span_order: vec![root.clone(), child],
            scroll: 0,
            selected: None,
            collapsed: HashSet::new(),
            span_health_scores: HashMap::new(),
            outcome_lines: vec![OutcomeLine {
                span_id: None,
                text: "redirect +0.4".to_string(),
            }],
            orphans: Vec::new(),
            notes: HashMap::new(),
        }
    }

    #[test]
    fn tree_rows_interleave_outcome_lines() {
        let tv = tree_view();
        let rows = visible_tree_rows(&tv);
        // root, its root-level outcome line, then child.
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].as_ref().map(|s| s.as_str()), Some("root"));
        assert!(
            rows[1].is_none(),
            "outcome line belongs to no clickable span"
        );
        assert_eq!(rows[2].as_ref().map(|s| s.as_str()), Some("child"));
    }

    #[test]
    fn wheel_scroll_targets_the_panel_under_the_cursor() {
        let p = panels();
        assert_eq!(
            scroll_target(&p, 5, 10, true),
            MouseTarget::ScrollPanel {
                left: true,
                center: false,
                up: true
            }
        );
        assert_eq!(
            scroll_target(&p, 50, 10, false),
            MouseTarget::ScrollPanel {
                left: false,
                center: true,
                up: false
            }
        );
        assert_eq!(scroll_target(&p, 200, 10, true), MouseTarget::None);
    }
}
