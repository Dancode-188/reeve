use crate::replay::ReplayState;
use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
};

/// Replaces the footer during replay: play state, speed, elapsed over
/// total, a position bar with a tick mark per intervention, and the
/// replay keybindings.
pub fn render(frame: &mut Frame, area: Rect, replay: &ReplayState, theme: &Theme) {
    if area.height == 0 {
        return;
    }
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.chrome_bg())),
        area,
    );

    let kb = Style::default()
        .fg(theme.get("blue"))
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(theme.subtext());

    let state_icon = if replay.playing {
        "\u{25B6}"
    } else {
        "\u{23F8}"
    };
    let total_ms = replay.end_ms().saturating_sub(replay.start_ms());
    let elapsed_ms = replay.clock_ms.saturating_sub(replay.start_ms()).max(0);

    let prefix = format!(
        " {state_icon} {:.1}x  {}/{}  ",
        replay.speed,
        fmt_secs(elapsed_ms),
        fmt_secs(total_ms),
    );
    let suffix = "  [Space] play [h/l] step [</>] speed [I] marker [Esc] exit";

    // The bar fills whatever width remains between prefix and suffix.
    let bar_width = (area.width as usize)
        .saturating_sub(prefix.len() + suffix.len())
        .max(8);
    let filled = (replay.progress() * bar_width as f64) as usize;
    let mut bar: Vec<char> = (0..bar_width)
        .map(|i| if i < filled { '\u{2588}' } else { '\u{2591}' })
        .collect();
    for fraction in replay.marker_fractions() {
        let idx = ((fraction * bar_width as f64) as usize).min(bar_width - 1);
        bar[idx] = '\u{25BC}';
    }
    let bar: String = bar.into_iter().collect();

    let line = Line::from(vec![
        Span::styled(prefix, Style::default().fg(theme.text())),
        Span::styled(bar, Style::default().fg(theme.get("blue"))),
        Span::styled(suffix, dim).patch_style(kb),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn fmt_secs(ms: i64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}
