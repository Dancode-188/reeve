use crate::theme::Theme;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{Block, Borders, Sparkline, Widget},
};

pub struct CostSparkline<'a> {
    pub history: &'a [f64],
    pub focused: bool,
    pub theme: &'a Theme,
}

impl<'a> Widget for CostSparkline<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let border_style = Style::default().fg(if self.focused {
            self.theme.border_focused()
        } else {
            self.theme.border_idle()
        });

        let block = Block::default()
            .title("COST")
            .borders(Borders::ALL)
            .border_style(border_style);

        // Scale to millidollars so fractional cents survive the u64 conversion.
        let data: Vec<u64> = self
            .history
            .iter()
            .map(|&c| (c * 10_000.0) as u64)
            .collect();

        Widget::render(
            Sparkline::default()
                .block(block)
                .style(Style::default().fg(self.theme.get("teal")))
                .data(data.as_slice()),
            area,
            buf,
        );
    }
}
