//! `/` search over a [`crate::Buffer`].
//!
//! Pattern compiles once via [`Buffer::set_search_pattern`]; matches
//! are computed lazily per row and cached against `dirty_gen` so a
//! steady-state buffer doesn't re-scan rows on every `n` / `N`.

use regex::Regex;

use crate::{Buffer, Position};

/// Per-row match cache. `gen` is the [`Buffer::dirty_gen`] at the
/// time the row was scanned; mismatch means the row's text changed
/// underneath us and we re-scan.
#[derive(Debug, Default, Clone)]
pub(crate) struct SearchState {
    pub(crate) pattern: Option<Regex>,
    /// `matches[row]` is the cached `(byte_start, byte_end)` runs on
    /// that row, captured at `gen[row]`. Length grows lazily as
    /// rows get queried.
    pub(crate) matches: Vec<Vec<(usize, usize)>>,
    pub(crate) generations: Vec<u64>,
}

impl SearchState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn set_pattern(&mut self, re: Option<Regex>) {
        self.pattern = re;
        self.matches.clear();
        self.generations.clear();
    }

    /// Refresh `matches[row]` if either the row's gen has rolled or
    /// we never scanned it. Returns the cached slice on success.
    pub(crate) fn matches_for(
        &mut self,
        row: usize,
        line: &str,
        dirty_gen: u64,
    ) -> &[(usize, usize)] {
        let Some(ref re) = self.pattern else {
            return &[];
        };
        if self.matches.len() <= row {
            self.matches.resize_with(row + 1, Vec::new);
            self.generations.resize(row + 1, u64::MAX);
        }
        if self.generations[row] != dirty_gen {
            self.matches[row] = re.find_iter(line).map(|m| (m.start(), m.end())).collect();
            self.generations[row] = dirty_gen;
        }
        &self.matches[row]
    }
}

impl Buffer {
    /// Set the active search pattern. Pass `None` to clear.
    /// Subsequent [`Buffer::search_forward`] / `search_backward`
    /// calls use the new pattern; `n` / `N` repeat the last set
    /// pattern.
    pub fn set_search_pattern(&mut self, re: Option<Regex>) {
        self.search_state_mut().set_pattern(re);
    }

    pub fn search_pattern(&self) -> Option<&Regex> {
        self.search_state().pattern.as_ref()
    }

    /// Move the cursor to the next match starting from (or just
    /// after, when `skip_current = true`) the cursor. Wraps end-of-
    /// buffer to row 0. Returns `true` when a match was found.
    pub fn search_forward(&mut self, skip_current: bool) -> bool {
        if self.search_pattern().is_none() {
            return false;
        }
        let cursor = self.cursor();
        let start_byte = self
            .line(cursor.row)
            .map(|l| cursor.byte_offset(l))
            .unwrap_or(0);
        // Search current row from cursor onward.
        if let Some(pos) =
            self.find_match_in_row(cursor.row, start_byte, skip_current, /*forward=*/ true)
        {
            self.set_cursor(pos);
            self.ensure_cursor_visible();
            return true;
        }
        // Scan rows after cursor, then wrap to rows before.
        let total = self.row_count();
        for offset in 1..=total {
            let row = (cursor.row + offset) % total;
            if let Some(pos) = self.find_match_in_row(row, 0, false, true) {
                self.set_cursor(pos);
                self.ensure_cursor_visible();
                return true;
            }
            if row == cursor.row {
                break;
            }
        }
        false
    }

    /// Move to the previous match. Symmetric with `search_forward`
    /// but walks rows backwards and picks the rightmost match in
    /// each row.
    pub fn search_backward(&mut self, skip_current: bool) -> bool {
        if self.search_pattern().is_none() {
            return false;
        }
        let cursor = self.cursor();
        let cursor_byte = self
            .line(cursor.row)
            .map(|l| cursor.byte_offset(l))
            .unwrap_or(0);
        if let Some(pos) = self.find_match_in_row(
            cursor.row,
            cursor_byte,
            skip_current,
            /*forward=*/ false,
        ) {
            self.set_cursor(pos);
            self.ensure_cursor_visible();
            return true;
        }
        let total = self.row_count();
        for offset in 1..=total {
            // Walk backwards with wrap.
            let row = (cursor.row + total - offset) % total;
            if let Some(pos) =
                self.find_match_in_row(row, usize::MAX, false, /*forward=*/ false)
            {
                self.set_cursor(pos);
                self.ensure_cursor_visible();
                return true;
            }
            if row == cursor.row {
                break;
            }
        }
        false
    }

    /// Match positions on `row` as `(byte_start, byte_end)`. Used
    /// by the render layer to paint search-match bg.
    pub fn search_matches(&mut self, row: usize) -> Vec<(usize, usize)> {
        let line = self.line(row).unwrap_or("").to_string();
        let dgen = self.dirty_gen();
        self.search_state_mut()
            .matches_for(row, &line, dgen)
            .to_vec()
    }

    fn find_match_in_row(
        &mut self,
        row: usize,
        anchor_byte: usize,
        skip_current: bool,
        forward: bool,
    ) -> Option<Position> {
        let line = self.line(row)?.to_string();
        let dgen = self.dirty_gen();
        let matches = self.search_state_mut().matches_for(row, &line, dgen);
        if matches.is_empty() {
            return None;
        }
        let m = if forward {
            matches
                .iter()
                .find(|(s, _)| {
                    if skip_current {
                        *s > anchor_byte
                    } else {
                        *s >= anchor_byte
                    }
                })
                .copied()
        } else {
            matches
                .iter()
                .rev()
                .find(|(s, _)| {
                    if skip_current {
                        *s < anchor_byte
                    } else {
                        *s <= anchor_byte
                    }
                })
                .copied()
        }?;
        // Convert byte offset back to char column.
        let col = line[..m.0].chars().count();
        Some(Position::new(row, col))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn re(pat: &str) -> Regex {
        Regex::new(pat).unwrap()
    }

    #[test]
    fn no_pattern_returns_false() {
        let mut b = Buffer::from_str("anything");
        assert!(!b.search_forward(false));
        assert!(!b.search_backward(false));
    }

    #[test]
    fn forward_finds_first_match_on_cursor_row() {
        let mut b = Buffer::from_str("foo bar foo baz");
        b.set_search_pattern(Some(re("foo")));
        assert!(b.search_forward(false));
        assert_eq!(b.cursor(), Position::new(0, 0));
    }

    #[test]
    fn forward_skip_current_walks_past() {
        let mut b = Buffer::from_str("foo bar foo baz");
        b.set_search_pattern(Some(re("foo")));
        b.search_forward(false);
        b.search_forward(true);
        assert_eq!(b.cursor(), Position::new(0, 8));
    }

    #[test]
    fn forward_wraps_to_top() {
        let mut b = Buffer::from_str("zzz\nfoo");
        b.set_cursor(Position::new(1, 2));
        b.set_search_pattern(Some(re("zzz")));
        assert!(b.search_forward(true));
        assert_eq!(b.cursor(), Position::new(0, 0));
    }

    #[test]
    fn backward_picks_rightmost_match_on_row() {
        let mut b = Buffer::from_str("foo bar foo baz");
        b.set_cursor(Position::new(0, 14));
        b.set_search_pattern(Some(re("foo")));
        assert!(b.search_backward(true));
        assert_eq!(b.cursor(), Position::new(0, 8));
        b.search_backward(true);
        assert_eq!(b.cursor(), Position::new(0, 0));
    }

    #[test]
    fn backward_wraps_to_bottom() {
        let mut b = Buffer::from_str("foo\nzzz");
        b.set_cursor(Position::new(0, 0));
        b.set_search_pattern(Some(re("zzz")));
        assert!(b.search_backward(true));
        assert_eq!(b.cursor(), Position::new(1, 0));
    }

    #[test]
    fn no_match_returns_false_and_keeps_cursor() {
        let mut b = Buffer::from_str("alpha beta gamma");
        b.set_cursor(Position::new(0, 5));
        b.set_search_pattern(Some(re("zzz")));
        let before = b.cursor();
        assert!(!b.search_forward(false));
        assert_eq!(b.cursor(), before);
    }

    #[test]
    fn cache_invalidates_after_edit() {
        use crate::Edit;
        let mut b = Buffer::from_str("foo bar");
        b.set_search_pattern(Some(re("bar")));
        let initial = b.search_matches(0);
        assert_eq!(initial, vec![(4, 7)]);
        b.apply_edit(Edit::InsertStr {
            at: Position::new(0, 0),
            text: "XX ".into(),
        });
        let after = b.search_matches(0);
        assert_eq!(after, vec![(7, 10)]);
    }

    #[test]
    fn unicode_match_columns_are_charwise() {
        let mut b = Buffer::from_str("tablé foo");
        b.set_search_pattern(Some(re("foo")));
        assert!(b.search_forward(false));
        // 'foo' starts at char index 6 ("tablé " = 6 chars).
        assert_eq!(b.cursor(), Position::new(0, 6));
    }
}
