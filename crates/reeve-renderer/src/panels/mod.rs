pub mod center;
pub mod degraded;
pub mod fatal;
pub mod footer;
pub mod header;
pub mod help;
pub mod left;
pub mod overlay;
pub mod right;

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
        left::render(frame, inner, state, theme);
    }

    center::render(frame, panels.center, state, theme, ascii, right_hidden);

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
) {
    footer::render(frame, area, theme, right_hidden, left_hidden);
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
