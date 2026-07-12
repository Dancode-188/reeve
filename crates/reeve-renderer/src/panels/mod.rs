pub mod center;
pub mod confirm;
pub mod cost_view;
pub mod degraded;
pub mod fatal;
pub mod focus_list;
pub mod footer;
pub mod header;
pub mod help;
pub mod history;
pub mod impact_view;
pub mod left;
pub mod note_input;
pub mod overlay;
pub mod palette;
pub mod right;
pub mod scrubber;
pub mod skeleton;
pub mod toast;

use crate::app::ViewMode;
use crate::{app::AppState, ascii::AsciiMode, layout::Panels, theme::Theme};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    widgets::{Block, Borders},
};

pub fn render(
    frame: &mut Frame,
    panels: &Panels,
    state: &AppState,
    theme: &Theme,
    ascii: &AsciiMode,
) {
    let right_hidden = panels.right.width == 0;
    let left_hidden = panels.left.width == 0;

    let divider_style = Style::default().fg(theme.surface());

    if !left_hidden {
        let border = Block::default()
            .borders(Borders::RIGHT)
            .border_style(divider_style);
        let inner = border.inner(panels.left);
        frame.render_widget(border, panels.left);
        if state.view_mode == ViewMode::Focus {
            focus_list::render(frame, inner, state, theme);
        } else {
            left::render(frame, inner, state, theme);
        }
    }

    // Replay shows the reconstructed tree and the impact view shows its
    // charts, even though both are entered from History; the history list
    // returns when they exit.
    if let Some(ref impact) = state.impact {
        impact_view::render(frame, panels.center, impact, theme);
    } else if state.view_mode == ViewMode::Cost {
        cost_view::render(frame, panels.center, &state.cost_summary, theme);
    } else if state.view_mode == ViewMode::History && state.replay.is_none() {
        history::render(frame, panels.center, state, theme);
    } else {
        center::render(frame, panels.center, state, theme, ascii, right_hidden);
    }

    if !right_hidden {
        let border = Block::default()
            .borders(Borders::LEFT)
            .border_style(divider_style);
        let inner = border.inner(panels.right);
        frame.render_widget(border, panels.right);
        right::render(frame, inner, state, theme, ascii);
    }
}

pub fn render_header(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    header::render(frame, area, state, theme);
}

pub fn render_footer(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    right_hidden: bool,
    left_hidden: bool,
    view_mode: ViewMode,
) {
    footer::render(frame, area, theme, right_hidden, left_hidden, view_mode);
}

pub fn render_help_overlay(frame: &mut Frame, area: Rect, theme: &Theme) {
    help::render(frame, area, theme);
}

pub fn render_fatal(frame: &mut Frame, area: Rect, err: &crate::app::FatalError, theme: &Theme) {
    fatal::render(frame, area, err, theme);
}

pub fn render_degraded(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    degraded::render(frame, area, state, theme);
}

pub fn render_intervention_overlay(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    overlay::render(frame, area, state, theme);
}

pub fn render_confirmation_modal(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    if let Some(ref pc) = state.pending_confirmation {
        confirm::render(frame, area, pc, theme);
    }
}

/// Shows the tail of a one-line input buffer when it outgrows its bar:
/// the cursor and the newest characters stay visible, with a leading
/// ellipsis marking the cut, like any terminal input.
pub fn tail_view(buffer: &str, max: usize) -> String {
    let len = buffer.chars().count();
    if len <= max {
        return buffer.to_string();
    }
    let tail: String = buffer.chars().skip(len - max.saturating_sub(1)).collect();
    format!("\u{2026}{tail}")
}

#[cfg(test)]
mod tests {
    use super::tail_view;

    #[test]
    fn tail_view_keeps_short_buffers_and_tails_long_ones() {
        assert_eq!(tail_view("short", 10), "short");
        let long = "abcdefghijklmnop";
        let shown = tail_view(long, 8);
        assert_eq!(shown, "\u{2026}jklmnop", "ellipsis plus the newest seven");
        assert_eq!(shown.chars().count(), 8);
    }
}
