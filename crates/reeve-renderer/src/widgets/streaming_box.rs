use crate::ascii::AsciiMode;
use crate::theme::Theme;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Widget},
};

pub struct StreamingBox<'a> {
    pub content: &'a str,
    /// True on even ticks (cursor blink on), false on odd ticks.
    pub cursor_on: bool,
    pub scroll: u16,
    pub focused: bool,
    pub theme: &'a Theme,
    pub ascii: &'a AsciiMode,
}

impl<'a> Widget for StreamingBox<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let border_style = Style::default().fg(if self.focused {
            self.theme.border_focused()
        } else {
            self.theme.border_idle()
        });

        let block = Block::default()
            .title("STREAM")
            .borders(Borders::ALL)
            .border_style(border_style);

        let cursor = if self.cursor_on {
            self.ascii.cursor()
        } else {
            " "
        };

        let cursor_style = Style::default().fg(self.theme.get("cursor"));

        let mut lines: Vec<Line<'static>> = self
            .content
            .lines()
            .map(|l| Line::from(l.to_string()))
            .collect();

        if let Some(last) = lines.last_mut() {
            last.spans
                .push(Span::styled(cursor.to_string(), cursor_style));
        } else {
            lines.push(Line::from(Span::styled(cursor.to_string(), cursor_style)));
        }

        Widget::render(
            Paragraph::new(Text::from(lines))
                .block(block)
                .scroll((self.scroll, 0)),
            area,
            buf,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn cursor_on_at_tick_0_off_at_tick_8() {
        let theme = Theme::load();
        let ascii = AsciiMode::new(false);

        let backend = TestBackend::new(30, 6);
        let mut terminal = Terminal::new(backend).unwrap();

        // cursor_on = true -> blinking block should appear
        terminal
            .draw(|frame| {
                frame.render_widget(
                    StreamingBox {
                        content: "hello",
                        cursor_on: true,
                        scroll: 0,
                        focused: false,
                        theme: &theme,
                        ascii: &ascii,
                    },
                    frame.area(),
                );
            })
            .unwrap();

        let with_cursor: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();

        // cursor_on = false -> cursor replaced with space
        terminal
            .draw(|frame| {
                frame.render_widget(
                    StreamingBox {
                        content: "hello",
                        cursor_on: false,
                        scroll: 0,
                        focused: false,
                        theme: &theme,
                        ascii: &ascii,
                    },
                    frame.area(),
                );
            })
            .unwrap();

        let without_cursor: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();

        assert!(
            with_cursor.contains('▌'),
            "cursor must show when cursor_on is true"
        );
        assert!(
            !without_cursor.contains('▌'),
            "cursor must hide when cursor_on is false"
        );
    }
}
