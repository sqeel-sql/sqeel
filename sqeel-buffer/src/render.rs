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
    /// Optional left-side line-number gutter. `width` includes the
    /// trailing space separating the number from text. Pass `None`
    /// to disable. Numbers are 1-based, right-aligned.
    pub gutter: Option<Gutter>,
    /// Bg painted under cells covered by an active `/` search match
    /// (read from [`Buffer::search_pattern`]). `Style::default()` to
    /// disable.
    pub search_bg: Style,
    /// Per-row gutter signs (LSP diagnostic dots, git diff markers,
    /// …). Painted into the leftmost gutter column after the line
    /// number, so they overwrite the leading space tui-style gutters
    /// reserve. Highest-priority sign per row wins.
    pub signs: &'a [Sign],
}

/// Configuration for the line-number gutter rendered to the left of
/// the text area. `width` is the total cell count reserved
/// (including any trailing spacer); the renderer right-aligns the
/// 1-based row number into the leftmost `width - 1` cells.
#[derive(Debug, Clone, Copy)]
pub struct Gutter {
    pub width: u16,
    pub style: Style,
}

/// Single-cell marker painted into the leftmost gutter column for a
/// document row. Used by hosts to surface LSP diagnostics, git diff
/// signs, etc. Higher `priority` wins when multiple signs land on
/// the same row.
#[derive(Debug, Clone, Copy)]
pub struct Sign {
    pub row: usize,
    pub ch: char,
    pub style: Style,
    pub priority: u8,
}

impl<R: StyleResolver> Widget for BufferView<'_, R> {
    fn render(self, area: Rect, term_buf: &mut TermBuffer) {
        let viewport = self.buffer.viewport();
        let cursor = self.buffer.cursor();
        let lines = self.buffer.lines();
        let spans = self.buffer.spans();
        let folds = self.buffer.folds();
        let top_row = viewport.top_row;
        let top_col = viewport.top_col;

        let gutter_width = self.gutter.map(|g| g.width).unwrap_or(0);
        let text_area = Rect {
            x: area.x.saturating_add(gutter_width),
            y: area.y,
            width: area.width.saturating_sub(gutter_width),
            height: area.height,
        };

        let total_rows = lines.len();
        let mut doc_row = top_row;
        let mut screen_row: u16 = 0;
        // Walk the document forward, skipping rows hidden by closed
        // folds. Emit the start row of a closed fold as a marker
        // line instead of its actual content.
        while doc_row < total_rows && screen_row < area.height {
            // Skip rows hidden by a closed fold (any row past start
            // of a closed fold).
            if folds.iter().any(|f| f.hides(doc_row)) {
                doc_row += 1;
                continue;
            }
            let folded_at_start = folds
                .iter()
                .find(|f| f.closed && f.start_row == doc_row)
                .copied();
            let line = &lines[doc_row];
            let row_spans = spans.get(doc_row).map(Vec::as_slice).unwrap_or(&[]);
            let sel_range = self.selection.and_then(|s| s.row_span(doc_row));
            let is_cursor_row = doc_row == cursor.row;
            if let Some(gutter) = self.gutter {
                self.paint_gutter(term_buf, area, screen_row, doc_row, gutter);
                self.paint_signs(term_buf, area, screen_row, doc_row);
            }
            if let Some(fold) = folded_at_start {
                self.paint_fold_marker(term_buf, text_area, screen_row, fold, is_cursor_row);
                screen_row += 1;
                doc_row = fold.end_row + 1;
                continue;
            }
            let search_ranges = self.row_search_ranges(line);
            self.paint_row(
                term_buf,
                text_area,
                screen_row,
                line,
                row_spans,
                sel_range,
                &search_ranges,
                is_cursor_row,
                cursor.col,
                top_col,
            );
            screen_row += 1;
            doc_row += 1;
        }
    }
}

impl<R: StyleResolver> BufferView<'_, R> {
    /// Run the active search regex against `line` and return the
    /// charwise `(start_col, end_col_exclusive)` ranges that need
    /// the search bg painted. Empty when no pattern is set.
    fn row_search_ranges(&self, line: &str) -> Vec<(usize, usize)> {
        let Some(re) = self.buffer.search_pattern() else {
            return Vec::new();
        };
        re.find_iter(line)
            .map(|m| {
                let start = line[..m.start()].chars().count();
                let end = line[..m.end()].chars().count();
                (start, end)
            })
            .collect()
    }

    fn paint_fold_marker(
        &self,
        term_buf: &mut TermBuffer,
        area: Rect,
        screen_row: u16,
        fold: crate::Fold,
        is_cursor_row: bool,
    ) {
        let y = area.y + screen_row;
        let style = if is_cursor_row && self.cursor_line_bg != Style::default() {
            self.cursor_line_bg
        } else {
            Style::default()
        };
        // Bg the whole row first so the marker reads like one cell.
        for x in area.x..(area.x + area.width) {
            if let Some(cell) = term_buf.cell_mut((x, y)) {
                cell.set_style(style);
            }
        }
        let label = format!("+-- {} lines folded --", fold.line_count());
        let mut x = area.x;
        let row_end_x = area.x + area.width;
        for ch in label.chars() {
            if x >= row_end_x {
                break;
            }
            let width = ch.width().unwrap_or(1) as u16;
            if x + width > row_end_x {
                break;
            }
            if let Some(cell) = term_buf.cell_mut((x, y)) {
                cell.set_char(ch);
                cell.set_style(style);
            }
            x = x.saturating_add(width);
        }
    }

    fn paint_signs(&self, term_buf: &mut TermBuffer, area: Rect, screen_row: u16, doc_row: usize) {
        let Some(sign) = self
            .signs
            .iter()
            .filter(|s| s.row == doc_row)
            .max_by_key(|s| s.priority)
        else {
            return;
        };
        let y = area.y + screen_row;
        let x = area.x;
        if let Some(cell) = term_buf.cell_mut((x, y)) {
            cell.set_char(sign.ch);
            cell.set_style(sign.style);
        }
    }

    fn paint_gutter(
        &self,
        term_buf: &mut TermBuffer,
        area: Rect,
        screen_row: u16,
        doc_row: usize,
        gutter: Gutter,
    ) {
        let y = area.y + screen_row;
        // Total gutter cells, leaving one trailing spacer column.
        let number_width = gutter.width.saturating_sub(1) as usize;
        let label = format!("{:>width$}", doc_row + 1, width = number_width);
        let mut x = area.x;
        for ch in label.chars() {
            if x >= area.x + gutter.width.saturating_sub(1) {
                break;
            }
            if let Some(cell) = term_buf.cell_mut((x, y)) {
                cell.set_char(ch);
                cell.set_style(gutter.style);
            }
            x = x.saturating_add(1);
        }
        // Spacer cell — same gutter style so the background is
        // continuous when a bg colour is set.
        let spacer_x = area.x + gutter.width.saturating_sub(1);
        if let Some(cell) = term_buf.cell_mut((spacer_x, y)) {
            cell.set_char(' ');
            cell.set_style(gutter.style);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_row(
        &self,
        term_buf: &mut TermBuffer,
        area: Rect,
        screen_row: u16,
        line: &str,
        row_spans: &[crate::Span],
        sel_range: crate::RowSpan,
        search_ranges: &[(usize, usize)],
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
            if self.search_bg != Style::default()
                && search_ranges
                    .iter()
                    .any(|&(s, e)| col_idx >= s && col_idx < e)
            {
                style = style.patch(self.search_bg);
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
            gutter: None,
            search_bg: Style::default(),
            signs: &[],
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
            gutter: None,
            search_bg: Style::default(),
            signs: &[],
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
            gutter: None,
            search_bg: Style::default(),
            signs: &[],
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
            gutter: None,
            search_bg: Style::default(),
            signs: &[],
        };
        let term = run_render(view, 20, 1);
        for x in 0..6 {
            assert_eq!(term.cell((x, 0)).unwrap().fg, Color::Red);
        }
    }

    #[test]
    fn gutter_renders_right_aligned_line_numbers() {
        let mut b = Buffer::from_str("a\nb\nc");
        b.viewport_mut().width = 10;
        b.viewport_mut().height = 3;
        let view = BufferView {
            buffer: &b,
            selection: None,
            resolver: &(no_styles as fn(u32) -> Style),
            cursor_line_bg: Style::default(),
            selection_bg: Style::default().bg(Color::Blue),
            cursor_style: Style::default().add_modifier(Modifier::REVERSED),
            gutter: Some(Gutter {
                width: 4,
                style: Style::default().fg(Color::Yellow),
            }),
            search_bg: Style::default(),
            signs: &[],
        };
        let term = run_render(view, 10, 3);
        // Width 4 = 3 number cells + 1 spacer; right-aligned "  1".
        assert_eq!(term.cell((2, 0)).unwrap().symbol(), "1");
        assert_eq!(term.cell((2, 0)).unwrap().fg, Color::Yellow);
        assert_eq!(term.cell((2, 1)).unwrap().symbol(), "2");
        assert_eq!(term.cell((2, 2)).unwrap().symbol(), "3");
        // Text shifted right past the gutter.
        assert_eq!(term.cell((4, 0)).unwrap().symbol(), "a");
    }

    #[test]
    fn search_bg_paints_match_cells() {
        use regex::Regex;
        let mut b = Buffer::from_str("foo bar foo");
        b.viewport_mut().width = 20;
        b.viewport_mut().height = 1;
        b.set_search_pattern(Some(Regex::new("foo").unwrap()));
        let view = BufferView {
            buffer: &b,
            selection: None,
            resolver: &(no_styles as fn(u32) -> Style),
            cursor_line_bg: Style::default(),
            selection_bg: Style::default().bg(Color::Blue),
            cursor_style: Style::default().add_modifier(Modifier::REVERSED),
            gutter: None,
            search_bg: Style::default().bg(Color::Magenta),
            signs: &[],
        };
        let term = run_render(view, 20, 1);
        for x in 0..3 {
            assert_eq!(term.cell((x, 0)).unwrap().bg, Color::Magenta);
        }
        // " bar " between matches stays default bg.
        assert_ne!(term.cell((3, 0)).unwrap().bg, Color::Magenta);
        for x in 8..11 {
            assert_eq!(term.cell((x, 0)).unwrap().bg, Color::Magenta);
        }
    }

    #[test]
    fn highest_priority_sign_wins_per_row_and_overwrites_gutter() {
        let mut b = Buffer::from_str("a\nb\nc");
        b.viewport_mut().width = 10;
        b.viewport_mut().height = 3;
        let signs = [
            Sign {
                row: 0,
                ch: 'W',
                style: Style::default().fg(Color::Yellow),
                priority: 1,
            },
            Sign {
                row: 0,
                ch: 'E',
                style: Style::default().fg(Color::Red),
                priority: 2,
            },
        ];
        let view = BufferView {
            buffer: &b,
            selection: None,
            resolver: &(no_styles as fn(u32) -> Style),
            cursor_line_bg: Style::default(),
            selection_bg: Style::default().bg(Color::Blue),
            cursor_style: Style::default().add_modifier(Modifier::REVERSED),
            gutter: Some(Gutter {
                width: 3,
                style: Style::default().fg(Color::DarkGray),
            }),
            search_bg: Style::default(),
            signs: &signs,
        };
        let term = run_render(view, 10, 3);
        assert_eq!(term.cell((0, 0)).unwrap().symbol(), "E");
        assert_eq!(term.cell((0, 0)).unwrap().fg, Color::Red);
        // Row 1 has no sign — leftmost cell stays as gutter content.
        assert_ne!(term.cell((0, 1)).unwrap().symbol(), "E");
    }

    #[test]
    fn closed_fold_collapses_rows_and_paints_marker() {
        let mut b = Buffer::from_str("a\nb\nc\nd\ne");
        b.viewport_mut().width = 30;
        b.viewport_mut().height = 5;
        // Fold rows 1-3 closed. Visible should be: 'a', marker, 'e'.
        b.add_fold(1, 3, true);
        let view = BufferView {
            buffer: &b,
            selection: None,
            resolver: &(no_styles as fn(u32) -> Style),
            cursor_line_bg: Style::default(),
            selection_bg: Style::default().bg(Color::Blue),
            cursor_style: Style::default().add_modifier(Modifier::REVERSED),
            gutter: None,
            search_bg: Style::default(),
            signs: &[],
        };
        let term = run_render(view, 30, 5);
        // Row 0: "a"
        assert_eq!(term.cell((0, 0)).unwrap().symbol(), "a");
        // Row 1: fold marker "+--"
        assert_eq!(term.cell((0, 1)).unwrap().symbol(), "+");
        assert_eq!(term.cell((1, 1)).unwrap().symbol(), "-");
        // Row 2: "e" (the 5th doc row, after the collapsed range)
        assert_eq!(term.cell((0, 2)).unwrap().symbol(), "e");
    }

    #[test]
    fn open_fold_renders_normally() {
        let mut b = Buffer::from_str("a\nb\nc");
        b.viewport_mut().width = 5;
        b.viewport_mut().height = 3;
        b.add_fold(0, 2, false); // open
        let view = BufferView {
            buffer: &b,
            selection: None,
            resolver: &(no_styles as fn(u32) -> Style),
            cursor_line_bg: Style::default(),
            selection_bg: Style::default().bg(Color::Blue),
            cursor_style: Style::default().add_modifier(Modifier::REVERSED),
            gutter: None,
            search_bg: Style::default(),
            signs: &[],
        };
        let term = run_render(view, 5, 3);
        assert_eq!(term.cell((0, 0)).unwrap().symbol(), "a");
        assert_eq!(term.cell((0, 1)).unwrap().symbol(), "b");
        assert_eq!(term.cell((0, 2)).unwrap().symbol(), "c");
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
            gutter: None,
            search_bg: Style::default(),
            signs: &[],
        };
        let term = run_render(view, 4, 1);
        assert_eq!(term.cell((0, 0)).unwrap().symbol(), "d");
        assert_eq!(term.cell((3, 0)).unwrap().symbol(), "g");
    }
}
