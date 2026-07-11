//! The live generation box. Wrapping is done here, not by Paragraph:
//! auto-scroll needs the exact wrapped line count to pin the newest
//! text to the bottom, and owning the wrap is the only way the count
//! and the rendering can never disagree.

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
    /// When set, the box pins to the bottom so the newest text is
    /// always visible; manual scrolling clears it upstream.
    pub auto_scroll: bool,
    pub focused: bool,
    pub theme: &'a Theme,
    pub ascii: &'a AsciiMode,
}

/// Greedy word wrap to a fixed width in characters. Words longer than
/// the width hard-split. Empty logical lines survive as blank rows so
/// paragraph breaks in the generation stay visible.
fn wrap_lines(content: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for logical in content.lines() {
        if logical.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut row = String::new();
        let mut row_len = 0usize;
        for word in logical.split(' ') {
            let mut word = word;
            let mut wlen = word.chars().count();
            // A word that cannot fit on any row hard-splits.
            while wlen > width {
                if row_len > 0 {
                    out.push(std::mem::take(&mut row));
                    row_len = 0;
                }
                let cut = word
                    .char_indices()
                    .nth(width)
                    .map(|(i, _)| i)
                    .unwrap_or(word.len());
                out.push(word[..cut].to_string());
                word = &word[cut..];
                wlen = word.chars().count();
            }
            let sep = usize::from(row_len > 0);
            if row_len + sep + wlen > width {
                out.push(std::mem::take(&mut row));
                row_len = 0;
            }
            if row_len > 0 {
                row.push(' ');
                row_len += 1;
            }
            row.push_str(word);
            row_len += wlen;
        }
        out.push(row);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
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

        let inner_width = area.width.saturating_sub(2) as usize;
        let inner_height = area.height.saturating_sub(2) as usize;

        let cursor = if self.cursor_on {
            self.ascii.cursor()
        } else {
            " "
        };
        let cursor_style = Style::default().fg(self.theme.get("cursor"));

        // Reserve one cell on the last row so the cursor never wraps.
        let mut wrapped = wrap_lines(self.content, inner_width.saturating_sub(1).max(1));
        let last = wrapped.pop().unwrap_or_default();

        let mut lines: Vec<Line<'static>> = wrapped.into_iter().map(Line::from).collect();
        lines.push(Line::from(vec![
            Span::raw(last),
            Span::styled(cursor.to_string(), cursor_style),
        ]));

        let max_scroll = lines.len().saturating_sub(inner_height) as u16;
        let scroll = if self.auto_scroll {
            max_scroll
        } else {
            self.scroll.min(max_scroll)
        };

        Widget::render(
            Paragraph::new(Text::from(lines))
                .block(block)
                .scroll((scroll, 0)),
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

    fn rendered(content: &str, auto_scroll: bool, w: u16, h: u16) -> String {
        let theme = Theme::load();
        let ascii = AsciiMode::new(false);
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    StreamingBox {
                        content,
                        cursor_on: false,
                        scroll: 0,
                        auto_scroll,
                        focused: false,
                        theme: &theme,
                        ascii: &ascii,
                    },
                    frame.area(),
                );
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect()
    }

    #[test]
    fn cursor_on_at_tick_0_off_at_tick_8() {
        let theme = Theme::load();
        let ascii = AsciiMode::new(false);
        let backend = TestBackend::new(30, 6);
        let mut terminal = Terminal::new(backend).unwrap();

        for (cursor_on, expect) in [(true, true), (false, false)] {
            terminal
                .draw(|frame| {
                    frame.render_widget(
                        StreamingBox {
                            content: "hello",
                            cursor_on,
                            scroll: 0,
                            auto_scroll: true,
                            focused: false,
                            theme: &theme,
                            ascii: &ascii,
                        },
                        frame.area(),
                    );
                })
                .unwrap();
            let screen: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol().to_string())
                .collect();
            assert_eq!(
                screen.contains('▌'),
                expect,
                "cursor visibility must follow the blink tick"
            );
        }
    }

    #[test]
    fn long_paragraphs_wrap_instead_of_truncating() {
        // One logical line much wider than the box: every word must
        // still land somewhere on screen. This clipped to a single
        // truncated line in a real session.
        let content = "the quick brown fox jumps over the lazy dog again and again until it wraps";
        let screen = rendered(content, false, 24, 10);
        for word in ["quick", "lazy", "wraps"] {
            assert!(screen.contains(word), "{word} must survive the wrap");
        }
    }

    #[test]
    fn auto_scroll_pins_the_newest_text() {
        // More wrapped lines than the box can show: with auto-scroll the
        // tail is visible and the head has scrolled away.
        let content = (0..30)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let screen = rendered(&content, true, 14, 6);
        assert!(screen.contains("word29"), "newest text must be visible");
        assert!(!screen.contains("word0 "), "oldest text has scrolled away");
    }

    #[test]
    fn wrap_lines_hard_splits_oversized_words() {
        let rows = wrap_lines("abcdefghij", 4);
        assert_eq!(rows, vec!["abcd", "efgh", "ij"]);
        let rows = wrap_lines("a\n\nb", 10);
        assert_eq!(rows, vec!["a", "", "b"], "paragraph breaks survive");
    }
}
