use crate::Position;

/// Where the buffer is scrolled to and how big the visible area is.
///
/// Mirrors what tui-textarea exposed today: the host publishes
/// `(width, height)` from the render path each frame, and the buffer
/// uses the cached values to clamp the cursor / scroll offsets when
/// motions ask for it. `top_row` and `top_col` are the first visible
/// row / column; `top_col` is a char index, matching [`Position`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Viewport {
    pub top_row: usize,
    pub top_col: usize,
    pub width: u16,
    pub height: u16,
}

impl Viewport {
    pub const fn new() -> Self {
        Self {
            top_row: 0,
            top_col: 0,
            width: 0,
            height: 0,
        }
    }

    /// Last document row that's currently on screen (inclusive).
    /// Returns `top_row` when `height == 0` so callers don't have
    /// to special-case the pre-first-draw state.
    pub fn bottom_row(self) -> usize {
        self.top_row
            .saturating_add((self.height as usize).max(1).saturating_sub(1))
    }

    /// True when `pos` lies inside the current viewport rect.
    pub fn contains(self, pos: Position) -> bool {
        let in_rows = pos.row >= self.top_row && pos.row <= self.bottom_row();
        let in_cols = pos.col >= self.top_col
            && pos.col < self.top_col.saturating_add((self.width as usize).max(1));
        in_rows && in_cols
    }

    /// Adjust `top_row` / `top_col` so `pos` is visible, scrolling by
    /// the minimum amount needed. Used after motions and after
    /// content edits that move the cursor.
    pub fn ensure_visible(&mut self, pos: Position) {
        if self.height == 0 || self.width == 0 {
            return;
        }
        let rows = self.height as usize;
        if pos.row < self.top_row {
            self.top_row = pos.row;
        } else if pos.row >= self.top_row + rows {
            self.top_row = pos.row + 1 - rows;
        }
        let cols = self.width as usize;
        if pos.col < self.top_col {
            self.top_col = pos.col;
        } else if pos.col >= self.top_col + cols {
            self.top_col = pos.col + 1 - cols;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vp(top_row: usize, height: u16) -> Viewport {
        Viewport {
            top_row,
            top_col: 0,
            width: 80,
            height,
        }
    }

    #[test]
    fn contains_inside_window() {
        let v = vp(10, 5);
        assert!(v.contains(Position::new(10, 0)));
        assert!(v.contains(Position::new(14, 79)));
    }

    #[test]
    fn contains_outside_window() {
        let v = vp(10, 5);
        assert!(!v.contains(Position::new(9, 0)));
        assert!(!v.contains(Position::new(15, 0)));
        assert!(!v.contains(Position::new(12, 80)));
    }

    #[test]
    fn ensure_visible_scrolls_down() {
        let mut v = vp(0, 5);
        v.ensure_visible(Position::new(10, 0));
        assert_eq!(v.top_row, 6);
    }

    #[test]
    fn ensure_visible_scrolls_up() {
        let mut v = vp(20, 5);
        v.ensure_visible(Position::new(15, 0));
        assert_eq!(v.top_row, 15);
    }

    #[test]
    fn ensure_visible_no_scroll_when_inside() {
        let mut v = vp(10, 5);
        v.ensure_visible(Position::new(12, 4));
        assert_eq!(v.top_row, 10);
    }

    #[test]
    fn ensure_visible_zero_dim_is_noop() {
        let mut v = Viewport::default();
        v.ensure_visible(Position::new(100, 100));
        assert_eq!(v.top_row, 0);
        assert_eq!(v.top_col, 0);
    }
}
