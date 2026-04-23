//! Post-render selection overlays.
//!
//! tui-textarea only supports a single char-range selection, so char-
//! line- and block-visual modes all render their highlight by OR-ing
//! `Modifier::REVERSED` into the frame buffer *after* the textarea has
//! painted. The modifier composes over whatever syntax highlighting +
//! cursor-line style the textarea applied underneath.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use tui_textarea::TextArea;

/// Paint a char-wise selection overlay spanning from `start` to `end`
/// inclusive. Single-row selections cover `[start.1, end.1]`; multi-row
/// selections cover start.1..EOL on the first row, full width on middle
/// rows, and 0..=end.1 on the last row.
pub fn paint_char_overlay(
    f: &mut Frame<'_>,
    textarea: &TextArea<'_>,
    area: Rect,
    start: (usize, usize),
    end: (usize, usize),
) {
    let top_row = textarea.viewport_top_row();
    let top_col = textarea.viewport_top_col();
    let lnum_width = textarea.lines().len().to_string().len() as u16 + 2;
    let content_x = area.x.saturating_add(lnum_width);
    let content_y = area.y;
    let content_w = area.width.saturating_sub(lnum_width);
    let content_h = area.height;

    let buf = f.buffer_mut();
    let lines = textarea.lines();
    for doc_row in start.0..=end.0 {
        if doc_row < top_row {
            continue;
        }
        let screen_dy = (doc_row - top_row) as u16;
        if screen_dy >= content_h {
            break;
        }
        let screen_y = content_y + screen_dy;
        let line_len = lines.get(doc_row).map(|l| l.chars().count()).unwrap_or(0);
        let row_start = if doc_row == start.0 { start.1 } else { 0 };
        let row_end_inclusive = if doc_row == end.0 {
            end.1
        } else {
            line_len.saturating_sub(1)
        };
        if line_len == 0 {
            continue;
        }
        let effective_start = row_start.max(top_col).min(line_len - 1);
        let effective_end = row_end_inclusive.min(line_len - 1);
        if effective_end < effective_start {
            continue;
        }
        for doc_col in effective_start..=effective_end {
            let screen_dx = (doc_col - top_col) as u16;
            if screen_dx >= content_w {
                break;
            }
            let screen_x = content_x + screen_dx;
            let cell = &mut buf[(screen_x, screen_y)];
            cell.modifier.insert(Modifier::REVERSED);
        }
    }
}

/// Paint a reversed-style overlay across full rows `top..=bot`. Used
/// by VisualLine mode so the cursor can stay at its natural column
/// (matching vim) while the highlight still covers the whole line.
pub fn paint_line_overlay(
    f: &mut Frame<'_>,
    textarea: &TextArea<'_>,
    area: Rect,
    top: usize,
    bot: usize,
) {
    let top_row = textarea.viewport_top_row();
    let lnum_width = textarea.lines().len().to_string().len() as u16 + 2;
    let content_x = area.x.saturating_add(lnum_width);
    let content_y = area.y;
    let content_w = area.width.saturating_sub(lnum_width);
    let content_h = area.height;

    let buf = f.buffer_mut();
    for doc_row in top..=bot {
        if doc_row < top_row {
            continue;
        }
        let screen_dy = (doc_row - top_row) as u16;
        if screen_dy >= content_h {
            break;
        }
        let screen_y = content_y + screen_dy;
        for dx in 0..content_w {
            let screen_x = content_x + dx;
            let cell = &mut buf[(screen_x, screen_y)];
            cell.modifier.insert(Modifier::REVERSED);
        }
    }
}

/// Paint a reversed-style overlay for the `(top, bot, left, right)`
/// document rectangle (all inclusive) directly into the frame buffer.
/// Runs *after* the textarea renders so the modifier lands on whatever
/// colors tree-sitter + the cursor-line style painted underneath.
pub fn paint_block_overlay(
    f: &mut Frame<'_>,
    textarea: &TextArea<'_>,
    area: Rect,
    top: usize,
    bot: usize,
    left: usize,
    right: usize,
) {
    let top_row = textarea.viewport_top_row();
    let top_col = textarea.viewport_top_col();
    let lnum_width = textarea.lines().len().to_string().len() as u16 + 2;
    let content_x = area.x.saturating_add(lnum_width);
    let content_y = area.y;
    let content_w = area.width.saturating_sub(lnum_width);
    let content_h = area.height;

    let buf = f.buffer_mut();
    for doc_row in top..=bot {
        if doc_row < top_row {
            continue;
        }
        let screen_dy = (doc_row - top_row) as u16;
        if screen_dy >= content_h {
            break;
        }
        let screen_y = content_y + screen_dy;
        let row_left = left.max(top_col);
        let row_right = right;
        if row_right < row_left {
            continue;
        }
        let line_len = textarea
            .lines()
            .get(doc_row)
            .map(|l| l.chars().count())
            .unwrap_or(0);
        if line_len == 0 {
            continue;
        }
        let effective_right = row_right.min(line_len - 1);
        if effective_right < row_left {
            continue;
        }
        for doc_col in row_left..=effective_right {
            let screen_dx = (doc_col - top_col) as u16;
            if screen_dx >= content_w {
                break;
            }
            let screen_x = content_x + screen_dx;
            let cell = &mut buf[(screen_x, screen_y)];
            cell.modifier.insert(Modifier::REVERSED);
        }
    }
}
