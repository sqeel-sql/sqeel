use std::collections::BTreeMap;

use crate::search::SearchState;
use crate::{Position, Span, Viewport};

/// In-memory text buffer + cursor + viewport + per-row span cache.
///
/// This is the core type the rest of `sqeel-buffer` builds on.
/// Phase 1 of the migration ships construction + getters only;
/// motion / edit / render APIs land in later phases. The
/// `lines` invariant — at least one entry, never empty — is
/// preserved by every later mutation.
pub struct Buffer {
    /// One entry per visual row. Always non-empty: a freshly
    /// constructed `Buffer` holds a single empty `String` so cursor
    /// positions don't need an "is the buffer empty?" branch.
    lines: Vec<String>,
    /// Charwise cursor. `col` is bound by `lines[row].chars().count()`
    /// in normal mode, one past it in operator-pending / insert.
    cursor: Position,
    /// Last vertical-motion column (vim's `curswant`). `None` until
    /// the first `j`/`k` so the buffer can bootstrap from the live
    /// cursor column.
    sticky_col: Option<usize>,
    /// Where the buffer is scrolled to + how big the visible region
    /// is. Width / height come from the host on each draw.
    viewport: Viewport,
    /// External per-row syntax / marker styling. `spans[row]` is a
    /// `Vec<Span>` for that row; rows beyond `spans.len()` get no
    /// styling (host hasn't published them yet).
    spans: Vec<Vec<Span>>,
    /// Buffer-local marks (`m{a-z}` / `'{a-z}` / `` `{a-z} ``).
    /// `BTreeMap` so iteration is deterministic for snapshot tests.
    marks: BTreeMap<char, Position>,
    /// Bumps on every mutation; render cache keys against this so a
    /// per-row Line gets recomputed when its source row changes.
    dirty_gen: u64,
    /// Lazy per-row match cache for `/` search. Lives next to the
    /// other buffer state so the `Buffer` API can drive `n` / `N`
    /// without the host plumbing a separate index through.
    search: SearchState,
    /// Manual folds — closed ranges hide rows in the render path.
    /// `pub(crate)` so the [`folds`] module can read/write directly.
    pub(crate) folds: Vec<crate::folds::Fold>,
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Buffer {
    /// Construct an empty buffer with one empty row + cursor at
    /// `(0, 0)`. Caller publishes a viewport size on first draw.
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor: Position::default(),
            sticky_col: None,
            viewport: Viewport::default(),
            spans: Vec::new(),
            marks: BTreeMap::new(),
            dirty_gen: 0,
            search: SearchState::new(),
            folds: Vec::new(),
        }
    }

    /// Build a buffer from a flat string. Splits on `\n`; a trailing
    /// `\n` produces a trailing empty line (matches every text
    /// editor's behaviour and keeps `from_text(buf.as_string())` an
    /// identity round-trip in the common case).
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(text: &str) -> Self {
        let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Self {
            lines,
            cursor: Position::default(),
            sticky_col: None,
            viewport: Viewport::default(),
            spans: Vec::new(),
            marks: BTreeMap::new(),
            dirty_gen: 0,
            search: SearchState::new(),
            folds: Vec::new(),
        }
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn line(&self, row: usize) -> Option<&str> {
        self.lines.get(row).map(String::as_str)
    }

    pub fn cursor(&self) -> Position {
        self.cursor
    }

    pub fn sticky_col(&self) -> Option<usize> {
        self.sticky_col
    }

    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    pub fn viewport_mut(&mut self) -> &mut Viewport {
        // Explicit `mut` access — viewport size lives on a separate
        // axis from buffer mutation, so we don't bump `dirty_gen`.
        &mut self.viewport
    }

    pub fn dirty_gen(&self) -> u64 {
        self.dirty_gen
    }

    /// Set cursor without scrolling. Caller is responsible for calling
    /// [`Buffer::ensure_cursor_visible`] when they want viewport
    /// follow. Clamps `row` and `col` to valid positions so motion
    /// helpers don't have to repeat the bound check.
    pub fn set_cursor(&mut self, pos: Position) {
        let last_row = self.lines.len().saturating_sub(1);
        let row = pos.row.min(last_row);
        let line_chars = self.lines[row].chars().count();
        let col = pos.col.min(line_chars);
        self.cursor = Position::new(row, col);
    }

    /// Replace the sticky col (vim's `curswant`). Motion code sets
    /// this after vertical / horizontal moves; the buffer doesn't
    /// touch it on its own.
    pub fn set_sticky_col(&mut self, col: Option<usize>) {
        self.sticky_col = col;
    }

    /// Bring the cursor into the visible viewport, scrolling by the
    /// minimum amount needed. When `viewport.wrap != Wrap::None` and
    /// `viewport.text_width > 0`, scrolling is screen-line aware:
    /// `top_row` is advanced one visible doc row at a time until the
    /// cursor's screen row falls inside the viewport's height.
    pub fn ensure_cursor_visible(&mut self) {
        let cursor = self.cursor;
        let v = self.viewport;
        let wrap_active = !matches!(v.wrap, crate::Wrap::None) && v.text_width > 0;
        if !wrap_active {
            self.viewport.ensure_visible(cursor);
            return;
        }
        if v.height == 0 {
            return;
        }
        // Cursor above the visible region: snap top_row to it.
        if cursor.row < v.top_row {
            self.viewport.top_row = cursor.row;
            self.viewport.top_col = 0;
            return;
        }
        let height = v.height as usize;
        // Push top_row forward (one visible doc row per iteration)
        // until the cursor's screen row sits inside [0, height).
        loop {
            let csr = self.cursor_screen_row_from(self.viewport.top_row);
            match csr {
                Some(row) if row < height => break,
                _ => {}
            }
            // Advance to the next non-folded doc row up to (but not
            // past) the cursor row. Stop if we ran out of room.
            let mut next = self.viewport.top_row + 1;
            while next <= cursor.row && self.folds.iter().any(|f| f.hides(next)) {
                next += 1;
            }
            if next > cursor.row {
                // Last resort — pin top_row to the cursor row so the
                // cursor lands at the top edge.
                self.viewport.top_row = cursor.row;
                break;
            }
            self.viewport.top_row = next;
        }
        self.viewport.top_col = 0;
    }

    /// Cursor's screen row offset (0-based) from `viewport.top_row`
    /// under the current wrap mode + `text_width`. `None` when wrap
    /// is off, the cursor row is hidden by a fold, or the cursor sits
    /// above `top_row`. Used by host-side scrolloff math.
    pub fn cursor_screen_row(&self) -> Option<usize> {
        if matches!(self.viewport.wrap, crate::Wrap::None) || self.viewport.text_width == 0 {
            return None;
        }
        self.cursor_screen_row_from(self.viewport.top_row)
    }

    /// Number of screen rows the doc range `start..=end` occupies
    /// under the current wrap mode. Skips fold-hidden rows. Empty /
    /// past-end ranges return 0. `Wrap::None` returns the visible
    /// doc-row count (one screen row per doc row).
    pub fn screen_rows_between(&self, start: usize, end: usize) -> usize {
        if start > end {
            return 0;
        }
        let last = self.lines.len().saturating_sub(1);
        let end = end.min(last);
        let v = self.viewport;
        let mut total = 0usize;
        for r in start..=end {
            if self.folds.iter().any(|f| f.hides(r)) {
                continue;
            }
            if matches!(v.wrap, crate::Wrap::None) || v.text_width == 0 {
                total += 1;
            } else {
                let line = self.lines.get(r).map(String::as_str).unwrap_or("");
                total += crate::wrap::wrap_segments(line, v.text_width, v.wrap).len();
            }
        }
        total
    }

    /// Earliest `top_row` such that `screen_rows_between(top, last)`
    /// is at least `height`. Lets host-side scrolloff math clamp
    /// `top_row` so the buffer never leaves blank rows below the
    /// content. When the buffer's total screen rows are smaller than
    /// `height` this returns 0.
    pub fn max_top_for_height(&self, height: usize) -> usize {
        if height == 0 {
            return 0;
        }
        let last = self.lines.len().saturating_sub(1);
        let mut total = 0usize;
        let mut row = last;
        loop {
            if !self.folds.iter().any(|f| f.hides(row)) {
                let v = self.viewport;
                total += if matches!(v.wrap, crate::Wrap::None) || v.text_width == 0 {
                    1
                } else {
                    let line = self.lines.get(row).map(String::as_str).unwrap_or("");
                    crate::wrap::wrap_segments(line, v.text_width, v.wrap).len()
                };
            }
            if total >= height {
                return row;
            }
            if row == 0 {
                return 0;
            }
            row -= 1;
        }
    }

    /// Returns the cursor's screen row (0-based, relative to `top`)
    /// under the current wrap mode + text width. `None` when the
    /// cursor row is hidden by a fold or sits above `top`.
    fn cursor_screen_row_from(&self, top: usize) -> Option<usize> {
        let cursor = self.cursor;
        if cursor.row < top {
            return None;
        }
        let v = self.viewport;
        let mut screen = 0usize;
        for r in top..=cursor.row {
            if self.folds.iter().any(|f| f.hides(r)) {
                continue;
            }
            let line = self.lines.get(r).map(String::as_str).unwrap_or("");
            let segs = crate::wrap::wrap_segments(line, v.text_width, v.wrap);
            if r == cursor.row {
                let seg_idx = crate::wrap::segment_for_col(&segs, cursor.col);
                return Some(screen + seg_idx);
            }
            screen += segs.len();
        }
        None
    }

    /// Clamp `pos` to the buffer's content. Out-of-range row gets
    /// pulled to the last row; out-of-range col gets pulled to the
    /// row's char count (one past last char — insertion point).
    pub fn clamp_position(&self, pos: Position) -> Position {
        let last_row = self.lines.len().saturating_sub(1);
        let row = pos.row.min(last_row);
        let line_chars = self.lines[row].chars().count();
        let col = pos.col.min(line_chars);
        Position::new(row, col)
    }

    /// Mutable access to the lines. Crate-internal — edit code uses
    /// this; outside callers go through [`Buffer::apply_edit`].
    pub(crate) fn lines_mut(&mut self) -> &mut Vec<String> {
        &mut self.lines
    }

    /// Crate-internal accessor for the search state. Search code
    /// keeps its lazy match cache here; the public surface is
    /// [`Buffer::set_search_pattern`] etc.
    pub(crate) fn search_state(&self) -> &SearchState {
        &self.search
    }
    pub(crate) fn search_state_mut(&mut self) -> &mut SearchState {
        &mut self.search
    }

    /// Bump the render-cache generation. Crate-internal — every
    /// content mutation calls this so render fingerprints invalidate.
    pub(crate) fn dirty_gen_bump(&mut self) {
        self.dirty_gen = self.dirty_gen.wrapping_add(1);
    }

    /// Replace the per-row syntax span overlay. Used by the host
    /// once tree-sitter (or any other producer) has fresh styling
    /// for the visible window. `spans[row]` corresponds to row
    /// `row`; rows beyond `spans.len()` get no styling.
    pub fn set_spans(&mut self, spans: Vec<Vec<crate::Span>>) {
        self.spans = spans;
        self.dirty_gen_bump();
    }

    /// Replace the buffer's full text in place. Cursor + sticky col
    /// are clamped to the new content; viewport stays put. Used
    /// during the migration off tui-textarea so the buffer can mirror
    /// the textarea's content after every edit without rebuilding
    /// the whole struct.
    pub fn replace_all(&mut self, text: &str) {
        let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        self.lines = lines;
        // Clamp cursor to surviving content.
        let cursor = self.clamp_position(self.cursor);
        self.cursor = cursor;
        self.dirty_gen_bump();
    }

    /// Same as [`Buffer::set_spans`] but exposed for in-crate tests
    /// without crossing the dirty-gen / lifetime boundaries the
    /// pub method advertises.
    #[cfg(test)]
    pub(crate) fn set_spans_for_test(&mut self, spans: Vec<Vec<crate::Span>>) {
        self.spans = spans;
    }

    pub fn marks(&self) -> &BTreeMap<char, Position> {
        &self.marks
    }

    pub fn spans(&self) -> &[Vec<Span>] {
        &self.spans
    }

    /// Concatenate the rows into a single `String` joined by `\n`.
    /// Inverse of [`Buffer::from_str`] for content built without a
    /// trailing newline.
    pub fn as_string(&self) -> String {
        self.lines.join("\n")
    }

    /// Number of rows in the buffer. Always `>= 1`.
    pub fn row_count(&self) -> usize {
        self.lines.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_has_one_empty_row() {
        let b = Buffer::new();
        assert_eq!(b.row_count(), 1);
        assert_eq!(b.line(0), Some(""));
        assert_eq!(b.cursor(), Position::default());
    }

    #[test]
    fn from_str_splits_on_newline() {
        let b = Buffer::from_str("foo\nbar\nbaz");
        assert_eq!(b.row_count(), 3);
        assert_eq!(b.line(0), Some("foo"));
        assert_eq!(b.line(2), Some("baz"));
    }

    #[test]
    fn from_str_trailing_newline_keeps_empty_row() {
        let b = Buffer::from_str("foo\n");
        assert_eq!(b.row_count(), 2);
        assert_eq!(b.line(1), Some(""));
    }

    #[test]
    fn from_str_empty_input_keeps_one_row() {
        let b = Buffer::from_str("");
        assert_eq!(b.row_count(), 1);
        assert_eq!(b.line(0), Some(""));
    }

    #[test]
    fn as_string_round_trips() {
        let b = Buffer::from_str("a\nb\nc");
        assert_eq!(b.as_string(), "a\nb\nc");
    }

    #[test]
    fn dirty_gen_starts_at_zero() {
        assert_eq!(Buffer::new().dirty_gen(), 0);
    }

    #[test]
    fn ensure_cursor_visible_wrap_scrolls_when_cursor_below_screen() {
        let mut b = Buffer::from_str("aaaaaaaaaa\nb\nc");
        {
            let v = b.viewport_mut();
            v.height = 3;
            v.width = 4;
            v.text_width = 4;
            v.wrap = crate::Wrap::Char;
        }
        // Cursor on row 2 col 0. Doc rows 0-2 occupy 3+1+1=5 screen
        // rows; only 3 fit. ensure_cursor_visible should advance
        // top_row past row 0 so cursor lands inside the viewport.
        b.set_cursor(Position::new(2, 0));
        b.ensure_cursor_visible();
        assert_eq!(b.viewport().top_row, 1);
    }

    #[test]
    fn ensure_cursor_visible_wrap_no_scroll_when_visible() {
        let mut b = Buffer::from_str("aaaaaaaaaa\nb");
        {
            let v = b.viewport_mut();
            v.height = 4;
            v.width = 4;
            v.text_width = 4;
            v.wrap = crate::Wrap::Char;
        }
        // Cursor in row 0 segment 1 (col 5). Doc row 0 wraps to 3
        // screen rows; cursor's screen row is 1 (< height). No scroll.
        b.set_cursor(Position::new(0, 5));
        b.ensure_cursor_visible();
        assert_eq!(b.viewport().top_row, 0);
    }

    #[test]
    fn ensure_cursor_visible_wrap_snaps_top_when_cursor_above() {
        let mut b = Buffer::from_str("a\nb\nc\nd\ne");
        {
            let v = b.viewport_mut();
            v.height = 2;
            v.width = 4;
            v.text_width = 4;
            v.wrap = crate::Wrap::Char;
            v.top_row = 3;
        }
        b.set_cursor(Position::new(1, 0));
        b.ensure_cursor_visible();
        assert_eq!(b.viewport().top_row, 1);
    }

    #[test]
    fn screen_rows_between_sums_segments_under_wrap() {
        // 9-char first row + 1-char second row + empty third.
        let mut b = Buffer::from_str("aaaaaaaaa\nb\n");
        {
            let v = b.viewport_mut();
            v.wrap = crate::Wrap::Char;
            v.text_width = 4;
        }
        // Row 0 wraps to 3 segments; row 1 → 1; row 2 (empty) → 1.
        assert_eq!(b.screen_rows_between(0, 0), 3);
        assert_eq!(b.screen_rows_between(0, 1), 4);
        assert_eq!(b.screen_rows_between(0, 2), 5);
        assert_eq!(b.screen_rows_between(1, 2), 2);
    }

    #[test]
    fn screen_rows_between_one_per_doc_row_when_wrap_off() {
        let b = Buffer::from_str("aaaaa\nb\nc");
        assert_eq!(b.screen_rows_between(0, 2), 3);
    }

    #[test]
    fn max_top_for_height_walks_back_until_height_reached() {
        // 5 rows, last row wraps to 3 segments under width 4.
        let mut b = Buffer::from_str("a\nb\nc\nd\neeeeeeee");
        {
            let v = b.viewport_mut();
            v.wrap = crate::Wrap::Char;
            v.text_width = 4;
        }
        // Last row alone = 2 segments; with row 3 added = 3 screen
        // rows; with row 2 = 4. height=4 → max_top = row 2.
        assert_eq!(b.max_top_for_height(4), 2);
        // Larger than total rows → 0.
        assert_eq!(b.max_top_for_height(99), 0);
    }

    #[test]
    fn cursor_screen_row_returns_none_when_wrap_off() {
        let b = Buffer::from_str("a");
        assert!(b.cursor_screen_row().is_none());
    }

    #[test]
    fn cursor_screen_row_under_wrap() {
        let mut b = Buffer::from_str("aaaaaaaaaa\nb");
        {
            let v = b.viewport_mut();
            v.wrap = crate::Wrap::Char;
            v.text_width = 4;
        }
        b.set_cursor(Position::new(0, 5));
        // Cursor on row 0 segment 1 → screen row 1.
        assert_eq!(b.cursor_screen_row(), Some(1));
        b.set_cursor(Position::new(1, 0));
        // Row 0 wraps to 3 segments + row 1's first segment = 3.
        assert_eq!(b.cursor_screen_row(), Some(3));
    }

    #[test]
    fn ensure_cursor_visible_falls_back_when_wrap_disabled() {
        let mut b = Buffer::from_str("a\nb\nc\nd\ne");
        {
            let v = b.viewport_mut();
            v.height = 2;
            v.width = 4;
            v.text_width = 4;
            v.wrap = crate::Wrap::None;
        }
        b.set_cursor(Position::new(4, 0));
        b.ensure_cursor_visible();
        // Without wrap the existing doc-row math runs: cursor at row 4
        // with height 2 → top_row = 3.
        assert_eq!(b.viewport().top_row, 3);
    }
}
