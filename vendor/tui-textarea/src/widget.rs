use crate::ratatui::buffer::Buffer;
use crate::ratatui::layout::Rect;
use crate::ratatui::text::{Span, Text};
use crate::ratatui::widgets::{Paragraph, Widget};
use crate::textarea::TextArea;
use crate::util::num_digits;
#[cfg(feature = "ratatui")]
use ratatui::text::Line;
use std::cmp;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
#[cfg(feature = "tuirs")]
use tui::text::Spans as Line;

// &mut 'a (u16, u16, u16, u16) is not available since `render` method takes immutable reference of TextArea
// instance. In the case, the TextArea instance cannot be accessed from any other objects since it is mutablly
// borrowed.
//
// `ratatui::Frame::render_stateful_widget` would be an assumed way to render a stateful widget. But at this
// point we stick with using `ratatui::Frame::render_widget` because it is simpler API. Users don't need to
// manage states of textarea instances separately.
// https://docs.rs/ratatui/latest/ratatui/terminal/struct.Frame.html#method.render_stateful_widget
// Row and col use usize so files/lines with more than u16::MAX lines/chars work correctly.
// Width and height remain u16 — they are terminal screen dimensions.
#[derive(Default, Debug)]
pub struct Viewport {
    row: AtomicUsize,
    col: AtomicUsize,
    dims: AtomicU64, // bits 0-15: width, bits 16-31: height
}

impl Clone for Viewport {
    fn clone(&self) -> Self {
        Viewport {
            row: AtomicUsize::new(self.row.load(Ordering::Relaxed)),
            col: AtomicUsize::new(self.col.load(Ordering::Relaxed)),
            dims: AtomicU64::new(self.dims.load(Ordering::Relaxed)),
        }
    }
}

impl Viewport {
    pub fn scroll_top(&self) -> (usize, usize) {
        let row = self.row.load(Ordering::Relaxed);
        let col = self.col.load(Ordering::Relaxed);
        (row, col)
    }

    pub fn rect(&self) -> (usize, usize, u16, u16) {
        let row = self.row.load(Ordering::Relaxed);
        let col = self.col.load(Ordering::Relaxed);
        let dims = self.dims.load(Ordering::Relaxed);
        let width = dims as u16;
        let height = (dims >> 16) as u16;
        (row, col, width, height)
    }

    pub fn position(&self) -> (usize, usize, usize, usize) {
        let (row_top, col_top, width, height) = self.rect();
        let row_bottom = row_top.saturating_add(height as usize).saturating_sub(1);
        let col_bottom = col_top.saturating_add(width as usize).saturating_sub(1);
        (row_top, col_top, row_top.max(row_bottom), col_top.max(col_bottom))
    }

    fn store(&self, row: usize, col: usize, width: u16, height: u16) {
        self.row.store(row, Ordering::Relaxed);
        self.col.store(col, Ordering::Relaxed);
        let dims = (width as u64) | ((height as u64) << 16);
        self.dims.store(dims, Ordering::Relaxed);
    }

    pub fn scroll(&mut self, rows: isize, cols: i16) {
        let row = self.row.get_mut();
        if rows >= 0 {
            *row = row.saturating_add(rows as usize);
        } else {
            *row = row.saturating_sub(rows.unsigned_abs());
        }
        let col = self.col.get_mut();
        if cols >= 0 {
            *col = col.saturating_add(cols as usize);
        } else {
            *col = col.saturating_sub(cols.unsigned_abs() as usize);
        }
    }
}

#[inline]
fn next_scroll_top(prev_top: usize, cursor: usize, len: usize) -> usize {
    if cursor < prev_top {
        cursor
    } else if prev_top.saturating_add(len) <= cursor {
        cursor.saturating_add(1).saturating_sub(len)
    } else {
        prev_top
    }
}

impl<'a> TextArea<'a> {
    fn text_widget(&'a self, top_row: usize, height: usize, col_offset: usize) -> Text<'a> {
        let lines_len = self.lines().len();
        let lnum_len = num_digits(lines_len);
        let bottom_row = cmp::min(top_row + height, lines_len);
        let mut lines = Vec::with_capacity(bottom_row - top_row);
        for (i, line) in self.lines()[top_row..bottom_row].iter().enumerate() {
            lines.push(self.line_spans(line.as_str(), top_row + i, lnum_len, col_offset));
        }
        Text::from(lines)
    }

    fn placeholder_widget(&'a self) -> Text<'a> {
        let cursor = Span::styled(" ", self.cursor_style);
        let text = Span::raw(self.placeholder.as_str());
        Text::from(Line::from(vec![cursor, text]))
    }

    fn scroll_top_row(&self, prev_top: usize, height: u16) -> usize {
        next_scroll_top(prev_top, self.cursor().0, height as usize)
    }

    fn scroll_top_col(&self, prev_top: usize, width: u16) -> usize {
        let mut cursor = self.cursor().1;
        // Adjust the cursor position due to the width of line number.
        if self.line_number_style().is_some() {
            let lnum = num_digits(self.lines().len()) as usize + 2; // `+ 2` for margins
            if cursor <= lnum {
                cursor *= 2; // Smoothly slide the line number into the screen on scrolling left
            } else {
                cursor += lnum; // The cursor position is shifted by the line number part
            };
        }
        next_scroll_top(prev_top, cursor, width as usize)
    }
}

impl Widget for &TextArea<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let Rect { width, height, .. } = if let Some(b) = self.block() {
            b.inner(area)
        } else {
            area
        };

        let (top_row, top_col) = self.viewport.scroll_top();
        let top_row = self.scroll_top_row(top_row, height);
        let top_col = self.scroll_top_col(top_col, width);

        // When top_col > u16::MAX, Paragraph::scroll can't express the offset.
        // Instead, compute the char offset into the line text and pre-clip in text_widget.
        let large_col_offset = if top_col > u16::MAX as usize {
            let lnum_width = if self.line_number_style().is_some() {
                num_digits(self.lines().len()) as usize + 2
            } else {
                0
            };
            top_col.saturating_sub(lnum_width)
        } else {
            0
        };

        let (text, style) = if !self.placeholder.is_empty() && self.is_empty() {
            (self.placeholder_widget(), self.placeholder_style)
        } else {
            (self.text_widget(top_row, height as _, large_col_offset), self.style())
        };

        // To get fine control over the text color and the surrrounding block they have to be rendered separately
        // see https://github.com/ratatui/ratatui/issues/144
        let mut text_area = area;
        let mut inner = Paragraph::new(text)
            .style(style)
            .alignment(self.alignment());
        if let Some(b) = self.block() {
            text_area = b.inner(area);
            // ratatui does not need `clone()` call because `Block` implements `WidgetRef` and `&T` implements `Widget`
            // where `T: WidgetRef`. So `b.render` internally calls `b.render_ref` and it doesn't move out `self`.
            #[cfg(feature = "tuirs")]
            let b = b.clone();
            b.render(area, buf)
        }
        // Only use Paragraph::scroll for offsets that fit in u16; larger offsets are handled
        // by pre-clipping the line text in text_widget (large_col_offset > 0).
        if top_col != 0 && large_col_offset == 0 {
            inner = inner.scroll((0, top_col as u16));
        }

        self.viewport.store(top_row, top_col, width, height);

        inner.render(text_area, buf);
    }
}
