/// A `(row, col)` location inside a [`crate::Buffer`].
///
/// `col` is a **char index** along the row's line, not a byte offset.
/// That's how vim users think about cursor positions ("column 4" =
/// the 4th character) and it sidesteps the off-by-one bugs that come
/// from mixing byte and char indices when the buffer holds
/// non-ASCII text. The accompanying [`Position::byte_offset`] helper
/// converts back to a byte offset when slicing the underlying
/// `String`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Position {
    pub row: usize,
    pub col: usize,
}

impl Position {
    pub const fn new(row: usize, col: usize) -> Self {
        Self { row, col }
    }

    /// Byte offset of `self.col` (a char index) into `line`. Returns
    /// `line.len()` when `col` is at or past the end of the line —
    /// matches `String::insert` / `replace_range` boundary semantics.
    pub fn byte_offset(self, line: &str) -> usize {
        line.char_indices()
            .nth(self.col)
            .map(|(b, _)| b)
            .unwrap_or(line.len())
    }
}

#[cfg(test)]
mod tests {
    use super::Position;

    #[test]
    fn byte_offset_ascii() {
        assert_eq!(Position::new(0, 0).byte_offset("hello"), 0);
        assert_eq!(Position::new(0, 3).byte_offset("hello"), 3);
        assert_eq!(Position::new(0, 5).byte_offset("hello"), 5);
        // Past end clamps at line length so callers can use it as an
        // insertion point without bounds-check ceremony.
        assert_eq!(Position::new(0, 99).byte_offset("hello"), 5);
    }

    #[test]
    fn byte_offset_utf8() {
        // "tablé" — 'é' is 2 bytes in UTF-8.
        let line = "tablé";
        assert_eq!(Position::new(0, 4).byte_offset(line), 4);
        assert_eq!(Position::new(0, 5).byte_offset(line), 6);
    }

    #[test]
    fn ord_is_row_major() {
        assert!(Position::new(0, 5) < Position::new(1, 0));
        assert!(Position::new(2, 0) > Position::new(1, 999));
        assert!(Position::new(1, 3) < Position::new(1, 4));
    }
}
