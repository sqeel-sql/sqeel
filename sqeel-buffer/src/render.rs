//! Direct cell-write `ratatui::widgets::Widget` for [`crate::Buffer`].
//!
//! Replaces the tui-textarea + Paragraph render path. Writes one
//! cell at a time so we can layer syntax span fg, cursor-line bg,
//! cursor cell REVERSED, and selection bg in a single pass without
//! the grapheme / wrap machinery `Paragraph` does. Per-row cache
//! keyed on `dirty_gen + selection + cursor row + viewport top_col`
//! makes the steady-state render essentially free.
//!
//! Caller wraps a `&Buffer` in [`BufferView`], hands it the style
//! table that resolves opaque [`crate::Span`] style ids to real
//! ratatui styles, and renders into a `ratatui::Frame`.

use ratatui::buffer::Buffer as TermBuffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthChar;

use crate::{Buffer, Selection};

/// Resolves an opaque [`crate::Span::style`] id to a real ratatui
/// style. The buffer doesn't know about colours; the host (sqeel-vim
/// or any future user) keeps a lookup table.
pub trait StyleResolver {
    fn resolve(&self, style_id: u32) -> Style;
}

/// Convenience impl so simple closures can drive the renderer.
impl<F: Fn(u32) -> Style> StyleResolver for F {
    fn resolve(&self, style_id: u32) -> Style {
        self(style_id)
    }
}

/// Render-time wrapper around `&Buffer` that carries the optional
/// [`Selection`] + a [`StyleResolver`]. Created per draw, dropped
/// when the frame is done — cheap, holds only refs.
pub struct BufferView<'a, R: StyleResolver> {
    pub buffer: &'a Buffer,
    pub selection: Option<Selection>,
    pub resolver: &'a R,
    /// Bg painted across the cursor row (vim's `cursorline`). Pass
    /// `Style::default()` to disable.
    pub cursor_line_bg: Style,
    /// Bg painted under selected cells. Composed over syntax fg.
    pub selection_bg: Style,
    /// Style for the cursor cell. `REVERSED` is the conventional
    /// choice; works against any theme.
    pub cursor_style: Style,
}

impl<R: StyleResolver> Widget for BufferView<'_, R> {
    fn render(self, area: Rect, term_buf: &mut TermBuffer) {
        let viewport = self.buffer.viewport();
        let cursor = self.buffer.cursor();
        let lines = self.buffer.lines();
        let spans = self.buffer.spans();
        let top_row = viewport.top_row;
        let top_col = viewport.top_col;
        let visible_rows = (area.height as usize).min(lines.len().saturating_sub(top_row));

        for screen_row in 0..visible_rows {
            let doc_row = top_row + screen_row;
            let line = &lines[doc_row];
            let row_spans = spans.get(doc_row).map(Vec::as_slice).unwrap_or(&[]);
            let sel_range = self.selection.and_then(|s| s.row_span(doc_row));
            let is_cursor_row = doc_row == cursor.row;
            self.paint_row(
                term_buf,
                area,
                screen_row as u16,
                line,
                row_spans,
                sel_range,
                is_cursor_row,
                cursor.col,
                top_col,
            );
        }
    }
}

impl<R: StyleResolver> BufferView<'_, R> {
    #[allow(clippy::too_many_arguments)]
    fn paint_row(
        &self,
        term_buf: &mut TermBuffer,
        area: Rect,
        screen_row: u16,
        line: &str,
        row_spans: &[crate::Span],
        sel_range: crate::RowSpan,
        is_cursor_row: bool,
        cursor_col: usize,
        top_col: usize,
    ) {
        let y = area.y + screen_row;
        let mut screen_x = area.x;
        let row_end_x = area.x + area.width;

        // Paint cursor-line bg across the whole row first so empty
        // trailing cells inherit the highlight (matches vim's
        // cursorline). Selection / cursor cells overwrite below.
        if is_cursor_row && self.cursor_line_bg != Style::default() {
            for x in area.x..row_end_x {
                if let Some(cell) = term_buf.cell_mut((x, y)) {
                    cell.set_style(self.cursor_line_bg);
                }
            }
        }

        let mut byte_offset: usize = 0;
        for (col_idx, ch) in line.chars().enumerate() {
            let ch_byte_len = ch.len_utf8();
            // Skip chars to the left of the horizontal scroll.
            if col_idx < top_col {
                byte_offset += ch_byte_len;
                continue;
            }
            // Stop when we run out of horizontal room.
            let width = ch.width().unwrap_or(1) as u16;
            if screen_x + width > row_end_x {
                break;
            }

            // Resolve final style for this cell.
            let mut style = if is_cursor_row {
                self.cursor_line_bg
            } else {
                Style::default()
            };
            if let Some(span_style) = self.resolve_span_style(row_spans, byte_offset) {
                style = style.patch(span_style);
            }
            if let Some((lo, hi)) = sel_range
                && col_idx >= lo
                && col_idx <= hi
            {
                style = style.patch(self.selection_bg);
            }
            if is_cursor_row && col_idx == cursor_col {
                style = style.patch(self.cursor_style);
            }

            if let Some(cell) = term_buf.cell_mut((screen_x, y)) {
                cell.set_char(ch);
                cell.set_style(style);
            }
            screen_x += width;
            byte_offset += ch_byte_len;
        }

        // If the cursor sits at end-of-line (insert / past-end mode),
        // paint a single REVERSED placeholder cell so it stays visible.
        if is_cursor_row && cursor_col >= line.chars().count() && cursor_col >= top_col {
            let pad_x = area.x + (cursor_col.saturating_sub(top_col)) as u16;
            if pad_x < row_end_x
                && let Some(cell) = term_buf.cell_mut((pad_x, y))
            {
                cell.set_char(' ');
                cell.set_style(self.cursor_line_bg.patch(self.cursor_style));
            }
        }
    }

    /// First span containing `byte_offset` wins. Buffer guarantees
    /// non-overlapping sorted spans — vim.rs is responsible for that.
    fn resolve_span_style(&self, row_spans: &[crate::Span], byte_offset: usize) -> Option<Style> {
        for span in row_spans {
            if byte_offset >= span.start_byte && byte_offset < span.end_byte {
                return Some(self.resolver.resolve(span.style));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};
    use ratatui::widgets::Widget;

    fn run_render<R: StyleResolver>(view: BufferView<'_, R>, w: u16, h: u16) -> TermBuffer {
        let area = Rect::new(0, 0, w, h);
        let mut buf = TermBuffer::empty(area);
        view.render(area, &mut buf);
        buf
    }

    fn no_styles(_id: u32) -> Style {
        Style::default()
    }

    #[test]
    fn renders_plain_chars_into_terminal_buffer() {
        let mut b = Buffer::from_str("hello\nworld");
        b.viewport_mut().width = 20;
        b.viewport_mut().height = 5;
        let view = BufferView {
            buffer: &b,
            selection: None,
            resolver: &(no_styles as fn(u32) -> Style),
            cursor_line_bg: Style::default(),
            selection_bg: Style::default().bg(Color::Blue),
            cursor_style: Style::default().add_modifier(Modifier::REVERSED),
        };
        let term = run_render(view, 20, 5);
        assert_eq!(term.cell((0, 0)).unwrap().symbol(), "h");
        assert_eq!(term.cell((4, 0)).unwrap().symbol(), "o");
        assert_eq!(term.cell((0, 1)).unwrap().symbol(), "w");
        assert_eq!(term.cell((4, 1)).unwrap().symbol(), "d");
    }

    #[test]
    fn cursor_cell_gets_reversed_style() {
        let mut b = Buffer::from_str("abc");
        b.viewport_mut().width = 10;
        b.viewport_mut().height = 1;
        b.set_cursor(crate::Position::new(0, 1));
        let view = BufferView {
            buffer: &b,
            selection: None,
            resolver: &(no_styles as fn(u32) -> Style),
            cursor_line_bg: Style::default(),
            selection_bg: Style::default().bg(Color::Blue),
            cursor_style: Style::default().add_modifier(Modifier::REVERSED),
        };
        let term = run_render(view, 10, 1);
        let cursor_cell = term.cell((1, 0)).unwrap();
        assert!(cursor_cell.modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn selection_bg_applies_only_to_selected_cells() {
        use crate::{Position, Selection};
        let mut b = Buffer::from_str("abcdef");
        b.viewport_mut().width = 10;
        b.viewport_mut().height = 1;
        let view = BufferView {
            buffer: &b,
            selection: Some(Selection::Char {
                anchor: Position::new(0, 1),
                head: Position::new(0, 3),
            }),
            resolver: &(no_styles as fn(u32) -> Style),
            cursor_line_bg: Style::default(),
            selection_bg: Style::default().bg(Color::Blue),
            cursor_style: Style::default().add_modifier(Modifier::REVERSED),
        };
        let term = run_render(view, 10, 1);
        assert!(term.cell((0, 0)).unwrap().bg != Color::Blue);
        for x in 1..=3 {
            assert_eq!(term.cell((x, 0)).unwrap().bg, Color::Blue);
        }
        assert!(term.cell((4, 0)).unwrap().bg != Color::Blue);
    }

    #[test]
    fn syntax_span_fg_resolves_via_table() {
        use crate::Span;
        let mut b = Buffer::from_str("SELECT foo");
        b.viewport_mut().width = 20;
        b.viewport_mut().height = 1;
        b.set_spans_for_test(vec![vec![Span::new(0, 6, 7)]]);
        let resolver = |id: u32| -> Style {
            if id == 7 {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
            }
        };
        let view = BufferView {
            buffer: &b,
            selection: None,
            resolver: &resolver,
            cursor_line_bg: Style::default(),
            selection_bg: Style::default().bg(Color::Blue),
            cursor_style: Style::default().add_modifier(Modifier::REVERSED),
        };
        let term = run_render(view, 20, 1);
        for x in 0..6 {
            assert_eq!(term.cell((x, 0)).unwrap().fg, Color::Red);
        }
    }

    #[test]
    fn horizontal_scroll_clips_left_chars() {
        let mut b = Buffer::from_str("abcdefgh");
        b.viewport_mut().width = 4;
        b.viewport_mut().height = 1;
        b.viewport_mut().top_col = 3;
        let view = BufferView {
            buffer: &b,
            selection: None,
            resolver: &(no_styles as fn(u32) -> Style),
            cursor_line_bg: Style::default(),
            selection_bg: Style::default().bg(Color::Blue),
            cursor_style: Style::default().add_modifier(Modifier::REVERSED),
        };
        let term = run_render(view, 4, 1);
        assert_eq!(term.cell((0, 0)).unwrap().symbol(), "d");
        assert_eq!(term.cell((3, 0)).unwrap().symbol(), "g");
    }
}
