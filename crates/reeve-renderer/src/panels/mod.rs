pub mod center;
pub mod footer;
pub mod header;
pub mod help;
pub mod left;
pub mod right;

use crate::{app::AppState, ascii::AsciiMode, layout::Panels, theme::Theme};
use ratatui::{Frame, layout::Rect};

pub fn render(
    frame: &mut Frame,
    panels: &Panels,
    state: &AppState,
    theme: &Theme,
    ascii: &AsciiMode,
) {
    let right_hidden = panels.right.width == 0;
    left::render(frame, panels.left, state, theme);
    center::render(frame, panels.center, state, theme, ascii, right_hidden);
    right::render(frame, panels.right, state, theme, ascii);
}

pub fn render_header(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    header::render(frame, area, state, theme);
}

pub fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme, right_hidden: bool) {
    footer::render(frame, area, theme, right_hidden);
}

pub fn render_help_overlay(frame: &mut Frame, area: Rect, theme: &Theme) {
    help::render(frame, area, theme);
}
