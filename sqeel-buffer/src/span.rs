/// One styled byte range on a buffer row.
///
/// The buffer holds these per row (`Vec<Vec<Span>>`) so the render
/// path doesn't have to re-tokenise each frame. `style` is opaque to
/// the buffer — sqeel-vim layers tree-sitter and LSP diagnostic
/// styling on top, then hands the merged spans back via
/// [`Buffer::set_spans`]. The render layer turns it into a real
/// `ratatui::style::Style` at draw time.
///
/// Byte ranges are half-open: `[start_byte, end_byte)`. They line up
/// with the row's `String` so callers can slice without re-deriving
/// indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start_byte: usize,
    pub end_byte: usize,
    /// Opaque style id resolved by the host's render layer.
    pub style: u32,
}

impl Span {
    pub const fn new(start_byte: usize, end_byte: usize, style: u32) -> Self {
        Self {
            start_byte,
            end_byte,
            style,
        }
    }

    /// Width of the span in bytes; useful for render-cache fingerprints.
    pub const fn len(self) -> usize {
        self.end_byte.saturating_sub(self.start_byte)
    }

    pub const fn is_empty(self) -> bool {
        self.end_byte <= self.start_byte
    }
}

#[cfg(test)]
mod tests {
    use super::Span;

    #[test]
    fn len_and_is_empty() {
        assert_eq!(Span::new(0, 5, 0).len(), 5);
        assert!(Span::new(3, 3, 0).is_empty());
        assert!(Span::new(7, 5, 0).is_empty());
    }
}
