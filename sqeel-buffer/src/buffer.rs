use std::collections::BTreeMap;

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
    /// minimum amount needed.
    pub fn ensure_cursor_visible(&mut self) {
        let cursor = self.cursor;
        self.viewport.ensure_visible(cursor);
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
}
