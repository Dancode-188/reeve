pub mod center;
pub mod footer;
pub mod header;
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
    left::render(frame, panels.left, state, theme);
    center::render(frame, panels.center, state, theme, ascii);
    right::render(frame, panels.right, state, theme, ascii);
}

pub fn render_header(frame: &mut Frame, area: Rect, state: &AppState, theme: &Theme) {
    header::render(frame, area, state, theme);
}

pub fn render_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    footer::render(frame, area, theme);
}
