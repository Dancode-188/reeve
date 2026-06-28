pub mod center;
pub mod left;
pub mod right;

use crate::{app::AppState, ascii::AsciiMode, layout::Panels, theme::Theme};
use ratatui::Frame;

pub fn render(frame: &mut Frame, panels: &Panels, state: &AppState, theme: &Theme, ascii: &AsciiMode) {
    left::render(frame, panels.left, state, theme);
    center::render(frame, panels.center, state, theme, ascii);
    right::render(frame, panels.right, state, theme, ascii);
}
