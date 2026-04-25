//! Manual folds: contiguous row ranges that the host can collapse
//! to a single visible "fold marker" line.
//!
//! Phase 9 of the migration plan unlocks this — vim users get
//! `zo`/`zc`/`za`/`zR`/`zM` over the same buffer the editor is
//! mutating, no separate fold tracker required.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fold {
    /// First row of the folded range (visible when closed).
    pub start_row: usize,
    /// Last row of the folded range, inclusive.
    pub end_row: usize,
    /// `true` = collapsed (rows after `start_row` are hidden).
    pub closed: bool,
}

impl Fold {
    pub fn contains(&self, row: usize) -> bool {
        row >= self.start_row && row <= self.end_row
    }

    /// True when `row` is hidden by a closed fold (i.e. inside the
    /// fold but not on its `start_row` marker line).
    pub fn hides(&self, row: usize) -> bool {
        self.closed && row > self.start_row && row <= self.end_row
    }

    /// Number of rows the fold spans.
    pub fn line_count(&self) -> usize {
        self.end_row.saturating_sub(self.start_row) + 1
    }
}

impl crate::Buffer {
    pub fn folds(&self) -> &[Fold] {
        &self.folds
    }

    /// Register a new fold. If an existing fold has the same
    /// `start_row`, it's replaced; otherwise the new one is inserted
    /// in start-row order. Empty / inverted ranges are rejected.
    pub fn add_fold(&mut self, start_row: usize, end_row: usize, closed: bool) {
        if end_row < start_row {
            return;
        }
        let last = self.row_count().saturating_sub(1);
        if start_row > last {
            return;
        }
        let end_row = end_row.min(last);
        let fold = Fold {
            start_row,
            end_row,
            closed,
        };
        if let Some(idx) = self.folds.iter().position(|f| f.start_row == start_row) {
            self.folds[idx] = fold;
        } else {
            let pos = self
                .folds
                .iter()
                .position(|f| f.start_row > start_row)
                .unwrap_or(self.folds.len());
            self.folds.insert(pos, fold);
        }
        self.dirty_gen_bump();
    }

    /// Drop the fold whose range covers `row`. Returns `true` when a
    /// fold was actually removed.
    pub fn remove_fold_at(&mut self, row: usize) -> bool {
        let Some(idx) = self.folds.iter().position(|f| f.contains(row)) else {
            return false;
        };
        self.folds.remove(idx);
        self.dirty_gen_bump();
        true
    }

    /// Open the fold at `row` (no-op if already open or no fold).
    pub fn open_fold_at(&mut self, row: usize) -> bool {
        let Some(f) = self.folds.iter_mut().find(|f| f.contains(row)) else {
            return false;
        };
        if !f.closed {
            return false;
        }
        f.closed = false;
        self.dirty_gen_bump();
        true
    }

    /// Close the fold at `row` (no-op if already closed or no fold).
    pub fn close_fold_at(&mut self, row: usize) -> bool {
        let Some(f) = self.folds.iter_mut().find(|f| f.contains(row)) else {
            return false;
        };
        if f.closed {
            return false;
        }
        f.closed = true;
        self.dirty_gen_bump();
        true
    }

    /// Flip the closed/open state of the fold containing `row`.
    pub fn toggle_fold_at(&mut self, row: usize) -> bool {
        let Some(f) = self.folds.iter_mut().find(|f| f.contains(row)) else {
            return false;
        };
        f.closed = !f.closed;
        self.dirty_gen_bump();
        true
    }

    /// `zR` — open every fold.
    pub fn open_all_folds(&mut self) {
        let mut changed = false;
        for f in self.folds.iter_mut() {
            if f.closed {
                f.closed = false;
                changed = true;
            }
        }
        if changed {
            self.dirty_gen_bump();
        }
    }

    /// `zM` — close every fold.
    pub fn close_all_folds(&mut self) {
        let mut changed = false;
        for f in self.folds.iter_mut() {
            if !f.closed {
                f.closed = true;
                changed = true;
            }
        }
        if changed {
            self.dirty_gen_bump();
        }
    }

    /// First fold whose range contains `row` (most folds are
    /// non-overlapping in vim's model, so the first match is the
    /// only match). Useful for the host's `za`/`zo`/`zc` handlers.
    pub fn fold_at_row(&self, row: usize) -> Option<&Fold> {
        self.folds.iter().find(|f| f.contains(row))
    }

    /// True iff `row` is hidden by a closed fold (any fold).
    pub fn is_row_hidden(&self, row: usize) -> bool {
        self.folds.iter().any(|f| f.hides(row))
    }

    /// Drop every fold that touches `[start_row, end_row]`. Edit
    /// paths call this to invalidate folds whose contents the user
    /// just mutated — vim's "edits inside a fold open it" behaviour.
    pub fn invalidate_folds_in_range(&mut self, start_row: usize, end_row: usize) {
        let before = self.folds.len();
        self.folds
            .retain(|f| f.end_row < start_row || f.start_row > end_row);
        if self.folds.len() != before {
            self.dirty_gen_bump();
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Buffer;

    fn b() -> Buffer {
        Buffer::from_str("a\nb\nc\nd\ne")
    }

    #[test]
    fn add_keeps_folds_in_start_row_order() {
        let mut buf = b();
        buf.add_fold(2, 3, true);
        buf.add_fold(0, 1, false);
        let starts: Vec<usize> = buf.folds().iter().map(|f| f.start_row).collect();
        assert_eq!(starts, vec![0, 2]);
    }

    #[test]
    fn add_replaces_existing_with_same_start_row() {
        let mut buf = b();
        buf.add_fold(1, 2, true);
        buf.add_fold(1, 4, false);
        assert_eq!(buf.folds().len(), 1);
        assert_eq!(buf.folds()[0].end_row, 4);
        assert!(!buf.folds()[0].closed);
    }

    #[test]
    fn add_clamps_end_row_to_buffer_bounds() {
        let mut buf = b();
        buf.add_fold(2, 99, true);
        assert_eq!(buf.folds()[0].end_row, 4);
    }

    #[test]
    fn add_rejects_inverted_range() {
        let mut buf = b();
        buf.add_fold(3, 1, true);
        assert!(buf.folds().is_empty());
    }

    #[test]
    fn toggle_flips_state() {
        let mut buf = b();
        buf.add_fold(1, 3, false);
        assert!(!buf.folds()[0].closed);
        assert!(buf.toggle_fold_at(2));
        assert!(buf.folds()[0].closed);
        assert!(buf.toggle_fold_at(2));
        assert!(!buf.folds()[0].closed);
    }

    #[test]
    fn is_row_hidden_excludes_start_row() {
        let mut buf = b();
        buf.add_fold(1, 3, true);
        assert!(!buf.is_row_hidden(0));
        assert!(!buf.is_row_hidden(1)); // start row stays visible
        assert!(buf.is_row_hidden(2));
        assert!(buf.is_row_hidden(3));
        assert!(!buf.is_row_hidden(4));
    }

    #[test]
    fn open_close_all_changes_every_fold() {
        let mut buf = b();
        buf.add_fold(0, 1, false);
        buf.add_fold(2, 3, true);
        buf.close_all_folds();
        assert!(buf.folds().iter().all(|f| f.closed));
        buf.open_all_folds();
        assert!(buf.folds().iter().all(|f| !f.closed));
    }

    #[test]
    fn invalidate_drops_overlapping_folds() {
        let mut buf = b();
        buf.add_fold(0, 1, true);
        buf.add_fold(2, 3, true);
        buf.add_fold(4, 4, true);
        buf.invalidate_folds_in_range(2, 3);
        let starts: Vec<usize> = buf.folds().iter().map(|f| f.start_row).collect();
        assert_eq!(starts, vec![0, 4]);
    }
}
