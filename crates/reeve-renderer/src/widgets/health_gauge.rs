use crate::theme::Theme;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, Borders, Gauge, Widget},
};

pub struct HealthGauge<'a> {
    pub score: Option<f64>,
    pub focused: bool,
    pub theme: &'a Theme,
}

impl<'a> Widget for HealthGauge<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let border_style = Style::default().fg(if self.focused {
            self.theme.border_focused()
        } else {
            self.theme.border_idle()
        });

        let block = Block::default()
            .title("HEALTH")
            .borders(Borders::ALL)
            .border_style(border_style);

        let (percent, color, modifier) = match self.score {
            None => (0u16, self.theme.subtext(), Modifier::empty()),
            Some(s) => {
                let pct = s.clamp(0.0, 100.0) as u16;
                let color = if s >= 80.0 {
                    self.theme.health_ok()
                } else if s >= 60.0 {
                    self.theme.health_warn()
                } else if s >= 20.0 {
                    self.theme.health_alert()
                } else {
                    self.theme.health_crit()
                };
                let modifier = if s < 20.0 {
                    Modifier::RAPID_BLINK
                } else {
                    Modifier::empty()
                };
                (pct, color, modifier)
            }
        };

        let label = match self.score {
            None => "N/A".to_string(),
            Some(s) => format!("{:.1}", s),
        };

        Widget::render(
            Gauge::default()
                .block(block)
                .gauge_style(Style::default().fg(color).add_modifier(modifier))
                .percent(percent)
                .label(label),
            area,
            buf,
        );
    }
}
