//! Editor wrapper around `tui_textarea::TextArea`.
//!
//! This file owns the public Editor API — construction, content access,
//! mouse and goto helpers, the (buffer-level) undo stack, and insert-mode
//! session bookkeeping. All vim-specific keyboard handling lives in
//! [`vim`] and communicates with Editor through a small internal API
//! exposed via `pub(super)` fields and helper methods.

use crate::vim::{self, VimState};
use crate::{KeybindingMode, VimMode};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use std::sync::atomic::{AtomicU16, Ordering};
use tui_textarea::{CursorMove, Input, Key, Scrolling, TextArea};

/// Where the cursor should land in the viewport after a `z`-family
/// scroll (`zz` / `zt` / `zb`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CursorScrollTarget {
    Center,
    Top,
    Bottom,
}

pub struct Editor<'a> {
    pub textarea: TextArea<'a>,
    pub keybinding_mode: KeybindingMode,
    /// Set when the user yanks/cuts; caller drains this to write to OS clipboard.
    pub last_yank: Option<String>,
    /// All vim-specific state (mode, pending operator, count, dot-repeat, ...).
    pub(super) vim: VimState,
    /// Undo history: each entry is (lines, cursor) before the edit.
    pub(super) undo_stack: Vec<(Vec<String>, (usize, usize))>,
    /// Redo history: entries pushed when undoing.
    pub(super) redo_stack: Vec<(Vec<String>, (usize, usize))>,
    /// Set whenever the buffer content changes; cleared by `take_dirty`.
    pub(super) content_dirty: bool,
    /// Cached snapshot of `lines().join("\n") + "\n"` wrapped in an Arc
    /// so repeated `content_arc()` calls within the same un-mutated
    /// window are free (ref-count bump instead of a full-buffer join).
    /// Invalidated by every [`mark_content_dirty`] call.
    pub(super) cached_content: Option<std::sync::Arc<String>>,
    /// Last rendered viewport height (text rows only, no chrome). Written
    /// by the draw path via [`set_viewport_height`] so the scroll helpers
    /// can clamp the cursor to stay visible without plumbing the height
    /// through every call.
    pub(super) viewport_height: AtomicU16,
    /// Pending LSP intent set by a normal-mode chord (e.g. `gd` for
    /// goto-definition). The host app drains this each step and fires
    /// the matching request against its own LSP client.
    pub(super) pending_lsp: Option<LspIntent>,
    /// Mirror buffer for the in-flight migration off tui-textarea.
    /// Phase 7a: content syncs on every `set_content` so the rest of
    /// the engine can start reading from / writing to it in
    /// follow-up commits without behaviour changing today.
    pub(super) buffer: sqeel_buffer::Buffer,
}

/// Host-observable LSP requests triggered by editor bindings. The
/// sqeel-vim crate doesn't talk to an LSP itself — it just raises an
/// intent that the TUI layer picks up and routes to `sqls`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspIntent {
    /// `gd` — textDocument/definition at the cursor.
    GotoDefinition,
}

impl<'a> Editor<'a> {
    pub fn new(keybinding_mode: KeybindingMode) -> Self {
        let mut textarea = TextArea::default();
        textarea.set_max_histories(0);
        Self {
            textarea,
            keybinding_mode,
            last_yank: None,
            vim: VimState::default(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            content_dirty: false,
            cached_content: None,
            viewport_height: AtomicU16::new(0),
            pending_lsp: None,
            buffer: sqeel_buffer::Buffer::new(),
        }
    }

    /// Drain any pending LSP intent raised by the last key. Returns
    /// `None` when no intent is armed.
    pub fn take_lsp_intent(&mut self) -> Option<LspIntent> {
        self.pending_lsp.take()
    }

    /// Mirror the textarea's current cursor + sticky col into the
    /// migration buffer. Called after every motion so the buffer
    /// stays in sync with the still-authoritative textarea during
    /// phases 7b-7e of the migration.
    ///
    /// Once the per-edit + per-motion call sites are ported in
    /// later phases, this drops out — the buffer becomes the source
    /// of truth and the textarea is mirrored from it instead.
    pub(crate) fn sync_buffer_from_textarea(&mut self) {
        let (row, col) = self.textarea.cursor();
        self.buffer
            .set_cursor(sqeel_buffer::Position::new(row, col));
        self.buffer.set_sticky_col(self.vim.sticky_col);
        let height = self.viewport_height_value();
        let viewport = self.buffer.viewport_mut();
        viewport.top_row = self.textarea.viewport_top_row();
        viewport.top_col = self.textarea.viewport_top_col();
        viewport.height = height;
    }

    /// Full content sync — mirrors lines + cursor + sticky col +
    /// viewport from the textarea into the buffer. Called after
    /// every key handler in `step()` so per-edit mutations
    /// (insert_char, delete_char, …) propagate to the buffer
    /// without each call site having to call into it explicitly.
    pub(crate) fn sync_buffer_content_from_textarea(&mut self) {
        let text = self.textarea.lines().join("\n");
        self.buffer.replace_all(&text);
        self.sync_buffer_from_textarea();
    }

    /// Push a `(row, col)` onto the back-jumplist so `Ctrl-o` returns
    /// to it later. Used by host-driven jumps (e.g. `gd`) that move
    /// the cursor without going through the vim engine's motion
    /// machinery, where push_jump fires automatically.
    pub fn record_jump(&mut self, pos: (usize, usize)) {
        const JUMPLIST_MAX: usize = 100;
        self.vim.jump_back.push(pos);
        if self.vim.jump_back.len() > JUMPLIST_MAX {
            self.vim.jump_back.remove(0);
        }
        self.vim.jump_fwd.clear();
    }

    /// Host apps call this each draw with the current text area height so
    /// scroll helpers can clamp the cursor without recomputing layout.
    pub fn set_viewport_height(&self, height: u16) {
        self.viewport_height.store(height, Ordering::Relaxed);
    }

    /// Last height published by `set_viewport_height` (in rows).
    pub fn viewport_height_value(&self) -> u16 {
        self.viewport_height.load(Ordering::Relaxed)
    }

    /// Calls `f` on the textarea and marks the content dirty.
    pub(super) fn mutate<R>(&mut self, f: impl FnOnce(&mut TextArea<'a>) -> R) -> R {
        self.mark_content_dirty();
        f(&mut self.textarea)
    }

    /// Single choke-point for "the buffer just changed". Sets the
    /// dirty flag and drops the cached `content_arc` snapshot so
    /// subsequent reads rebuild from the live textarea. Callers
    /// mutating `textarea` directly (e.g. the TUI's bracketed-paste
    /// path) must invoke this to keep the cache honest.
    pub fn mark_content_dirty(&mut self) {
        self.content_dirty = true;
        self.cached_content = None;
    }

    /// Returns true if content changed since the last call, then clears the flag.
    pub fn take_dirty(&mut self) -> bool {
        let dirty = self.content_dirty;
        self.content_dirty = false;
        dirty
    }

    /// Returns the cursor's row within the visible textarea (0-based), updating
    /// the stored viewport top so subsequent calls remain accurate.
    pub fn cursor_screen_row(&mut self, height: u16) -> u16 {
        let cursor = self.textarea.cursor().0;
        let top = self.textarea.viewport_top_row();
        cursor.saturating_sub(top).min(height as usize - 1) as u16
    }

    /// Returns the cursor's screen position `(x, y)` for `area` (the textarea
    /// rect). Accounts for line-number gutter and viewport scroll. Returns
    /// `None` if the cursor is outside the visible viewport.
    pub fn cursor_screen_pos(&self, area: Rect) -> Option<(u16, u16)> {
        let (row, col) = self.textarea.cursor();
        let top_row = self.textarea.viewport_top_row();
        let top_col = self.textarea.viewport_top_col();
        if row < top_row || col < top_col {
            return None;
        }
        let lnum_width = self.textarea.lines().len().to_string().len() as u16 + 2;
        let dy = (row - top_row) as u16;
        let dx = (col - top_col) as u16;
        if dy >= area.height || dx + lnum_width >= area.width {
            return None;
        }
        Some((area.x + lnum_width + dx, area.y + dy))
    }

    pub fn vim_mode(&self) -> VimMode {
        self.vim.public_mode()
    }

    /// Bounds of the active visual-block rectangle as
    /// `(top_row, bot_row, left_col, right_col)` — all inclusive.
    /// `None` when we're not in VisualBlock mode.
    /// Read-only view of the live `/` or `?` prompt. `None` outside
    /// search-prompt mode.
    pub fn search_prompt(&self) -> Option<&crate::vim::SearchPrompt> {
        self.vim.search_prompt.as_ref()
    }

    /// Most recent committed search pattern (persists across `n` / `N`
    /// and across prompt exits). `None` before the first search.
    pub fn last_search(&self) -> Option<&str> {
        self.vim.last_search.as_deref()
    }

    /// Start/end `(row, col)` of the active char-wise Visual selection
    /// (inclusive on both ends, positionally ordered). `None` when not
    /// in Visual mode.
    pub fn char_highlight(&self) -> Option<((usize, usize), (usize, usize))> {
        if self.vim_mode() != VimMode::Visual {
            return None;
        }
        let anchor = self.vim.visual_anchor;
        let cursor = self.textarea.cursor();
        let (start, end) = if anchor <= cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        Some((start, end))
    }

    /// Top/bottom rows of the active VisualLine selection (inclusive).
    /// `None` when we're not in VisualLine mode.
    pub fn line_highlight(&self) -> Option<(usize, usize)> {
        if self.vim_mode() != VimMode::VisualLine {
            return None;
        }
        let anchor = self.vim.visual_line_anchor;
        let cursor = self.textarea.cursor().0;
        Some((anchor.min(cursor), anchor.max(cursor)))
    }

    pub fn block_highlight(&self) -> Option<(usize, usize, usize, usize)> {
        if self.vim_mode() != VimMode::VisualBlock {
            return None;
        }
        let (ar, ac) = self.vim.block_anchor;
        let cr = self.textarea.cursor().0;
        let cc = self.vim.block_vcol;
        let top = ar.min(cr);
        let bot = ar.max(cr);
        let left = ac.min(cc);
        let right = ac.max(cc);
        Some((top, bot, left, right))
    }

    /// Active selection in `sqeel_buffer::Selection` shape. `None` when
    /// not in a Visual mode. Phase 7d-i wiring — the host hands this
    /// straight to `BufferView` once render flips off textarea
    /// (Phase 7d-ii drops the `paint_*_overlay` calls on the same
    /// switch).
    pub fn buffer_selection(&self) -> Option<sqeel_buffer::Selection> {
        use sqeel_buffer::{Position, Selection};
        match self.vim_mode() {
            VimMode::Visual => {
                let (ar, ac) = self.vim.visual_anchor;
                let (cr, cc) = self.textarea.cursor();
                Some(Selection::Char {
                    anchor: Position::new(ar, ac),
                    head: Position::new(cr, cc),
                })
            }
            VimMode::VisualLine => {
                let anchor_row = self.vim.visual_line_anchor;
                let head_row = self.textarea.cursor().0;
                Some(Selection::Line {
                    anchor_row,
                    head_row,
                })
            }
            VimMode::VisualBlock => {
                let (ar, ac) = self.vim.block_anchor;
                let cr = self.textarea.cursor().0;
                let cc = self.vim.block_vcol;
                Some(Selection::Block {
                    anchor: Position::new(ar, ac),
                    head: Position::new(cr, cc),
                })
            }
            _ => None,
        }
    }

    /// Force back to normal mode (used when dismissing completions etc.)
    pub fn force_normal(&mut self) {
        self.textarea.cancel_selection();
        self.vim.force_normal();
    }

    pub fn content(&self) -> String {
        let mut s = self.textarea.lines().join("\n");
        s.push('\n');
        s
    }

    /// Same logical output as [`content`], but returns a cached
    /// `Arc<String>` so back-to-back reads within an un-mutated window
    /// are ref-count bumps instead of multi-MB joins. The cache is
    /// invalidated by every [`mark_content_dirty`] call.
    pub fn content_arc(&mut self) -> std::sync::Arc<String> {
        if let Some(arc) = &self.cached_content {
            return std::sync::Arc::clone(arc);
        }
        let arc = std::sync::Arc::new(self.content());
        self.cached_content = Some(std::sync::Arc::clone(&arc));
        arc
    }

    pub fn set_content(&mut self, text: &str) {
        let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
        while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
            lines.pop();
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        let carried_yank = self.textarea.yank_text();
        self.textarea = TextArea::new(lines);
        self.textarea.set_max_histories(0);
        if !carried_yank.is_empty() {
            self.textarea.set_yank_text(carried_yank);
        }
        // Mirror the load into the migration buffer. Phase 7a only
        // syncs on `set_content`; phases 7b-7c plumb the per-edit
        // mutations through.
        self.buffer = sqeel_buffer::Buffer::from_str(text);
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.mark_content_dirty();
    }

    /// Install `text` as the pending yank buffer so the next `p`/`P` pastes
    /// it. Linewise is inferred from a trailing newline, matching how `yy`/`dd`
    /// shape their payload.
    pub fn seed_yank(&mut self, text: String) {
        self.vim.yank_linewise = text.ends_with('\n');
        self.textarea.set_yank_text(text);
    }

    /// Scroll the viewport down by `rows`. The cursor stays on its
    /// absolute line (vim convention) unless the scroll would take it
    /// off-screen — in that case it's clamped to the first row still
    /// visible.
    pub fn scroll_down(&mut self, rows: i16) {
        self.scroll_viewport(rows);
    }

    /// Scroll the viewport up by `rows`. Cursor stays unless it would
    /// fall off the bottom of the new viewport, then clamp to the
    /// bottom-most visible row.
    pub fn scroll_up(&mut self, rows: i16) {
        self.scroll_viewport(-rows);
    }

    /// Vim's `scrolloff` default — keep the cursor at least this many
    /// rows away from the top / bottom edge of the viewport while
    /// scrolling. Collapses to `height / 2` for tiny viewports.
    const SCROLLOFF: usize = 5;

    fn scroll_viewport(&mut self, delta: i16) {
        if delta == 0 {
            return;
        }
        self.textarea.scroll(Scrolling::Delta {
            rows: delta,
            cols: 0,
        });
        let (cur_row, cur_col) = self.textarea.cursor();
        let top = self.textarea.viewport_top_row();
        let height = self.viewport_height.load(Ordering::Relaxed) as usize;
        if height == 0 {
            return;
        }
        let margin = Self::SCROLLOFF.min(height / 2);
        let min_row = top + margin;
        let max_row = top + height.saturating_sub(1).saturating_sub(margin);
        let new_row = cur_row.clamp(min_row, max_row.max(min_row));
        if new_row != cur_row {
            let line_len = self
                .textarea
                .lines()
                .get(new_row)
                .map(|l| l.chars().count())
                .unwrap_or(0);
            let line_len = line_len.saturating_sub(1);
            let new_col = cur_col.min(line_len);
            self.textarea
                .move_cursor(CursorMove::Jump(new_row, new_col));
        }
    }

    pub fn goto_line(&mut self, line: usize) {
        self.textarea
            .move_cursor(CursorMove::Jump(line.saturating_sub(1), 0));
    }

    /// Scroll so the cursor row lands at the given viewport position:
    /// `Center` → middle row, `Top` → first row, `Bottom` → last row.
    /// Cursor stays on its absolute line; only the viewport moves.
    pub(super) fn scroll_cursor_to(&mut self, pos: CursorScrollTarget) {
        let height = self.viewport_height.load(Ordering::Relaxed) as usize;
        if height == 0 {
            return;
        }
        let cur_row = self.textarea.cursor().0;
        let cur_top = self.textarea.viewport_top_row();
        let new_top = match pos {
            CursorScrollTarget::Center => cur_row.saturating_sub(height / 2),
            CursorScrollTarget::Top => cur_row,
            CursorScrollTarget::Bottom => cur_row.saturating_sub(height.saturating_sub(1)),
        };
        let delta = new_top as isize - cur_top as isize;
        if delta == 0 {
            return;
        }
        self.textarea.scroll(Scrolling::Delta {
            rows: delta.clamp(i16::MIN as isize, i16::MAX as isize) as i16,
            cols: 0,
        });
    }

    /// Translate a terminal mouse position into a (row, col) inside the document.
    /// `area` is the outer editor rect: 1-row tab bar at top (flush), then the
    /// textarea with 1 cell of horizontal pane padding on each side.
    fn mouse_to_doc_pos(&self, area: Rect, col: u16, row: u16) -> (usize, usize) {
        let lines = self.textarea.lines();
        let inner_top = area.y.saturating_add(1); // tab bar row
        let lnum_width = lines.len().to_string().len() as u16 + 2;
        let content_x = area.x.saturating_add(1).saturating_add(lnum_width);
        let rel_row = row.saturating_sub(inner_top) as usize;
        let top = self.textarea.viewport_top_row();
        let doc_row = (top + rel_row).min(lines.len().saturating_sub(1));
        let rel_col = col.saturating_sub(content_x) as usize;
        let line_len = lines.get(doc_row).map(|l| l.len()).unwrap_or(0);
        (doc_row, rel_col.min(line_len))
    }

    /// Jump the cursor to the given 1-based line/column, clamped to the document.
    pub fn jump_to(&mut self, line: usize, col: usize) {
        let r = line.saturating_sub(1);
        let max_row = self.textarea.lines().len().saturating_sub(1);
        let r = r.min(max_row);
        let line_len = self.textarea.lines()[r].chars().count();
        let c = col.saturating_sub(1).min(line_len);
        self.textarea.move_cursor(CursorMove::Jump(r, c));
    }

    /// Jump cursor to the terminal-space mouse position; exits Visual modes if active.
    pub fn mouse_click(&mut self, area: Rect, col: u16, row: u16) {
        if self.vim.is_visual() {
            self.textarea.cancel_selection();
            self.vim.force_normal();
        }
        let (r, c) = self.mouse_to_doc_pos(area, col, row);
        self.textarea.move_cursor(CursorMove::Jump(r, c));
    }

    /// Begin a mouse-drag selection: anchor at current cursor and enter Visual mode.
    pub fn mouse_begin_drag(&mut self) {
        if !self.vim.is_visual_char() {
            self.textarea.cancel_selection();
            self.vim.enter_visual(self.textarea.cursor());
        }
    }

    /// Extend an in-progress mouse drag to the given terminal-space position.
    pub fn mouse_extend_drag(&mut self, area: Rect, col: u16, row: u16) {
        let (r, c) = self.mouse_to_doc_pos(area, col, row);
        self.textarea.move_cursor(CursorMove::Jump(r, c));
    }

    pub fn insert_str(&mut self, text: &str) {
        self.mutate(|t| t.insert_str(text));
    }

    pub fn accept_completion(&mut self, completion: &str) {
        let (row, col) = self.textarea.cursor();
        let line = self.textarea.lines()[row].clone();
        let before = &line[..col.min(line.len())];
        let prefix_len = before
            .chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .count();
        for _ in 0..prefix_len {
            self.mutate(|t| t.delete_char());
        }
        self.mutate(|t| t.insert_str(completion));
    }

    pub(super) fn snapshot(&self) -> (Vec<String>, (usize, usize)) {
        (self.textarea.lines().to_vec(), self.textarea.cursor())
    }

    pub(super) fn push_undo(&mut self) {
        let snap = self.snapshot();
        if self.undo_stack.len() >= 200 {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(snap);
        self.redo_stack.clear();
    }

    pub(super) fn restore(&mut self, lines: Vec<String>, cursor: (usize, usize)) {
        self.textarea = TextArea::new(lines);
        self.textarea.set_max_histories(0);
        self.textarea
            .move_cursor(CursorMove::Jump(cursor.0, cursor.1));
        self.mark_content_dirty();
    }

    /// Returns true if the key was consumed by the editor.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let input = crossterm_to_input(key);
        if input.key == Key::Null {
            return false;
        }
        vim::step(self, input)
    }
}

pub(super) fn crossterm_to_input(key: KeyEvent) -> Input {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let k = match key.code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Enter => Key::Enter,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::Tab => Key::Tab,
        KeyCode::Esc => Key::Esc,
        _ => Key::Null,
    };
    Input {
        key: k,
        ctrl,
        alt,
        shift,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn shift_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }
    fn ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn vim_normal_to_insert() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.handle_key(key(KeyCode::Char('i')));
        assert_eq!(e.vim_mode(), VimMode::Insert);
    }

    #[test]
    fn vim_insert_to_normal() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.handle_key(key(KeyCode::Char('i')));
        e.handle_key(key(KeyCode::Esc));
        assert_eq!(e.vim_mode(), VimMode::Normal);
    }

    #[test]
    fn vim_normal_to_visual() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.handle_key(key(KeyCode::Char('v')));
        assert_eq!(e.vim_mode(), VimMode::Visual);
    }

    #[test]
    fn vim_visual_to_normal() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.handle_key(key(KeyCode::Char('v')));
        e.handle_key(key(KeyCode::Esc));
        assert_eq!(e.vim_mode(), VimMode::Normal);
    }

    #[test]
    fn vim_shift_i_moves_to_first_non_whitespace() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("   hello");
        e.textarea.move_cursor(CursorMove::End);
        e.handle_key(shift_key(KeyCode::Char('I')));
        assert_eq!(e.vim_mode(), VimMode::Insert);
        assert_eq!(e.textarea.cursor(), (0, 3));
    }

    #[test]
    fn vim_shift_a_moves_to_end_and_insert() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("hello");
        e.handle_key(shift_key(KeyCode::Char('A')));
        assert_eq!(e.vim_mode(), VimMode::Insert);
        assert_eq!(e.textarea.cursor().1, 5);
    }

    #[test]
    fn count_10j_moves_down_10() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content(
            (0..20)
                .map(|i| format!("line{i}"))
                .collect::<Vec<_>>()
                .join("\n")
                .as_str(),
        );
        for d in "10".chars() {
            e.handle_key(key(KeyCode::Char(d)));
        }
        e.handle_key(key(KeyCode::Char('j')));
        assert_eq!(e.textarea.cursor().0, 10);
    }

    #[test]
    fn count_o_repeats_insert_on_esc() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("hello");
        for d in "3".chars() {
            e.handle_key(key(KeyCode::Char(d)));
        }
        e.handle_key(key(KeyCode::Char('o')));
        assert_eq!(e.vim_mode(), VimMode::Insert);
        for c in "world".chars() {
            e.handle_key(key(KeyCode::Char(c)));
        }
        e.handle_key(key(KeyCode::Esc));
        assert_eq!(e.vim_mode(), VimMode::Normal);
        assert_eq!(e.textarea.lines().len(), 4);
        assert!(e.textarea.lines().iter().skip(1).all(|l| l == "world"));
    }

    #[test]
    fn count_i_repeats_text_on_esc() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("");
        for d in "3".chars() {
            e.handle_key(key(KeyCode::Char(d)));
        }
        e.handle_key(key(KeyCode::Char('i')));
        for c in "ab".chars() {
            e.handle_key(key(KeyCode::Char(c)));
        }
        e.handle_key(key(KeyCode::Esc));
        assert_eq!(e.vim_mode(), VimMode::Normal);
        assert_eq!(e.textarea.lines()[0], "ababab");
    }

    #[test]
    fn vim_shift_o_opens_line_above() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("hello");
        e.handle_key(shift_key(KeyCode::Char('O')));
        assert_eq!(e.vim_mode(), VimMode::Insert);
        assert_eq!(e.textarea.cursor(), (0, 0));
        assert_eq!(e.textarea.lines().len(), 2);
    }

    #[test]
    fn vim_gg_goes_to_top() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("a\nb\nc");
        e.textarea.move_cursor(CursorMove::Bottom);
        e.handle_key(key(KeyCode::Char('g')));
        e.handle_key(key(KeyCode::Char('g')));
        assert_eq!(e.textarea.cursor().0, 0);
    }

    #[test]
    fn vim_shift_g_goes_to_bottom() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("a\nb\nc");
        e.handle_key(shift_key(KeyCode::Char('G')));
        assert_eq!(e.textarea.cursor().0, 2);
    }

    #[test]
    fn vim_dd_deletes_line() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("first\nsecond");
        e.handle_key(key(KeyCode::Char('d')));
        e.handle_key(key(KeyCode::Char('d')));
        assert_eq!(e.textarea.lines().len(), 1);
        assert_eq!(e.textarea.lines()[0], "second");
    }

    #[test]
    fn vim_dw_deletes_word() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("hello world");
        e.handle_key(key(KeyCode::Char('d')));
        e.handle_key(key(KeyCode::Char('w')));
        assert_eq!(e.vim_mode(), VimMode::Normal);
        assert!(!e.textarea.lines()[0].starts_with("hello"));
    }

    #[test]
    fn vim_yy_yanks_line() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("hello\nworld");
        e.handle_key(key(KeyCode::Char('y')));
        e.handle_key(key(KeyCode::Char('y')));
        assert!(e.last_yank.as_deref().unwrap_or("").starts_with("hello"));
    }

    #[test]
    fn vim_yy_does_not_move_cursor() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("first\nsecond\nthird");
        e.textarea.move_cursor(CursorMove::Down);
        let before = e.textarea.cursor();
        e.handle_key(key(KeyCode::Char('y')));
        e.handle_key(key(KeyCode::Char('y')));
        assert_eq!(e.textarea.cursor(), before);
        assert_eq!(e.vim_mode(), VimMode::Normal);
    }

    #[test]
    fn vim_yw_yanks_word() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("hello world");
        e.handle_key(key(KeyCode::Char('y')));
        e.handle_key(key(KeyCode::Char('w')));
        assert_eq!(e.vim_mode(), VimMode::Normal);
        assert!(e.last_yank.is_some());
    }

    #[test]
    fn vim_cc_changes_line() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("hello\nworld");
        e.handle_key(key(KeyCode::Char('c')));
        e.handle_key(key(KeyCode::Char('c')));
        assert_eq!(e.vim_mode(), VimMode::Insert);
    }

    #[test]
    fn vim_u_undoes_insert_session_as_chunk() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("hello");
        e.handle_key(key(KeyCode::Char('i')));
        e.handle_key(key(KeyCode::Enter));
        e.handle_key(key(KeyCode::Enter));
        e.handle_key(key(KeyCode::Esc));
        assert_eq!(e.textarea.lines().len(), 3);
        e.handle_key(key(KeyCode::Char('u')));
        assert_eq!(e.textarea.lines().len(), 1);
        assert_eq!(e.textarea.lines()[0], "hello");
    }

    #[test]
    fn vim_undo_redo_roundtrip() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("hello");
        e.handle_key(key(KeyCode::Char('i')));
        for c in "world".chars() {
            e.handle_key(key(KeyCode::Char(c)));
        }
        e.handle_key(key(KeyCode::Esc));
        let after = e.textarea.lines()[0].clone();
        e.handle_key(key(KeyCode::Char('u')));
        assert_eq!(e.textarea.lines()[0], "hello");
        e.handle_key(ctrl_key(KeyCode::Char('r')));
        assert_eq!(e.textarea.lines()[0], after);
    }

    #[test]
    fn vim_u_undoes_dd() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("first\nsecond");
        e.handle_key(key(KeyCode::Char('d')));
        e.handle_key(key(KeyCode::Char('d')));
        assert_eq!(e.textarea.lines().len(), 1);
        e.handle_key(key(KeyCode::Char('u')));
        assert_eq!(e.textarea.lines().len(), 2);
        assert_eq!(e.textarea.lines()[0], "first");
    }

    #[test]
    fn vim_ctrl_r_redoes() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("hello");
        e.handle_key(ctrl_key(KeyCode::Char('r')));
    }

    #[test]
    fn vim_r_replaces_char() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("hello");
        e.handle_key(key(KeyCode::Char('r')));
        e.handle_key(key(KeyCode::Char('x')));
        assert_eq!(e.textarea.lines()[0].chars().next(), Some('x'));
    }

    #[test]
    fn vim_tilde_toggles_case() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("hello");
        e.handle_key(key(KeyCode::Char('~')));
        assert_eq!(e.textarea.lines()[0].chars().next(), Some('H'));
    }

    #[test]
    fn vim_visual_d_cuts() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("hello");
        e.handle_key(key(KeyCode::Char('v')));
        e.handle_key(key(KeyCode::Char('l')));
        e.handle_key(key(KeyCode::Char('l')));
        e.handle_key(key(KeyCode::Char('d')));
        assert_eq!(e.vim_mode(), VimMode::Normal);
        assert!(e.last_yank.is_some());
    }

    #[test]
    fn vim_visual_c_enters_insert() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("hello");
        e.handle_key(key(KeyCode::Char('v')));
        e.handle_key(key(KeyCode::Char('l')));
        e.handle_key(key(KeyCode::Char('c')));
        assert_eq!(e.vim_mode(), VimMode::Insert);
    }

    #[test]
    fn vim_normal_unknown_key_consumed() {
        let mut e = Editor::new(KeybindingMode::Vim);
        // Unknown keys are consumed (swallowed) rather than returning false.
        let consumed = e.handle_key(key(KeyCode::Char('z')));
        assert!(consumed);
    }

    #[test]
    fn force_normal_clears_operator() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.handle_key(key(KeyCode::Char('d')));
        e.force_normal();
        assert_eq!(e.vim_mode(), VimMode::Normal);
    }

    fn many_lines(n: usize) -> String {
        (0..n)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn prime_viewport(e: &mut Editor<'_>, height: u16) {
        use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};
        e.set_viewport_height(height);
        let r = Rect {
            x: 0,
            y: 0,
            width: 40,
            height,
        };
        let mut b = Buffer::empty(r);
        (&e.textarea).render(r, &mut b);
    }

    #[test]
    fn zz_centers_cursor_in_viewport() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content(&many_lines(100));
        prime_viewport(&mut e, 20);
        e.textarea.move_cursor(CursorMove::Jump(50, 0));
        e.handle_key(key(KeyCode::Char('z')));
        e.handle_key(key(KeyCode::Char('z')));
        assert_eq!(e.textarea.viewport_top_row(), 40);
        assert_eq!(e.textarea.cursor().0, 50);
    }

    #[test]
    fn zt_puts_cursor_at_viewport_top() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content(&many_lines(100));
        prime_viewport(&mut e, 20);
        e.textarea.move_cursor(CursorMove::Jump(50, 0));
        e.handle_key(key(KeyCode::Char('z')));
        e.handle_key(key(KeyCode::Char('t')));
        assert_eq!(e.textarea.viewport_top_row(), 50);
        assert_eq!(e.textarea.cursor().0, 50);
    }

    #[test]
    fn ctrl_a_increments_number_at_cursor() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("x = 41");
        e.handle_key(ctrl_key(KeyCode::Char('a')));
        assert_eq!(e.textarea.lines()[0], "x = 42");
        assert_eq!(e.textarea.cursor(), (0, 5));
    }

    #[test]
    fn ctrl_a_finds_number_to_right_of_cursor() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("foo 99 bar");
        e.handle_key(ctrl_key(KeyCode::Char('a')));
        assert_eq!(e.textarea.lines()[0], "foo 100 bar");
        assert_eq!(e.textarea.cursor(), (0, 6));
    }

    #[test]
    fn ctrl_a_with_count_adds_count() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("x = 10");
        for d in "5".chars() {
            e.handle_key(key(KeyCode::Char(d)));
        }
        e.handle_key(ctrl_key(KeyCode::Char('a')));
        assert_eq!(e.textarea.lines()[0], "x = 15");
    }

    #[test]
    fn ctrl_x_decrements_number() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("n=5");
        e.handle_key(ctrl_key(KeyCode::Char('x')));
        assert_eq!(e.textarea.lines()[0], "n=4");
    }

    #[test]
    fn ctrl_x_crosses_zero_into_negative() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("v=0");
        e.handle_key(ctrl_key(KeyCode::Char('x')));
        assert_eq!(e.textarea.lines()[0], "v=-1");
    }

    #[test]
    fn ctrl_a_on_negative_number_increments_toward_zero() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("a = -5");
        e.handle_key(ctrl_key(KeyCode::Char('a')));
        assert_eq!(e.textarea.lines()[0], "a = -4");
    }

    #[test]
    fn ctrl_a_noop_when_no_digit_on_line() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("no digits here");
        e.handle_key(ctrl_key(KeyCode::Char('a')));
        assert_eq!(e.textarea.lines()[0], "no digits here");
    }

    #[test]
    fn zb_puts_cursor_at_viewport_bottom() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content(&many_lines(100));
        prime_viewport(&mut e, 20);
        e.textarea.move_cursor(CursorMove::Jump(50, 0));
        e.handle_key(key(KeyCode::Char('z')));
        e.handle_key(key(KeyCode::Char('b')));
        assert_eq!(e.textarea.viewport_top_row(), 31);
        assert_eq!(e.textarea.cursor().0, 50);
    }

    /// Contract that the TUI drain relies on: `set_content` flags the
    /// editor dirty (so the next `take_dirty` call reports the change),
    /// and a second `take_dirty` returns `false` after consumption. The
    /// TUI drains this flag after every programmatic content load so
    /// opening a tab doesn't get mistaken for a user edit and mark the
    /// tab dirty (which would then trigger the quit-prompt on `:q`).
    #[test]
    fn set_content_dirties_then_take_dirty_clears() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("hello");
        assert!(
            e.take_dirty(),
            "set_content should leave content_dirty=true"
        );
        assert!(!e.take_dirty(), "take_dirty should clear the flag");
    }

    #[test]
    fn content_arc_returns_same_arc_until_mutation() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("hello");
        let a = e.content_arc();
        let b = e.content_arc();
        assert!(
            std::sync::Arc::ptr_eq(&a, &b),
            "repeated content_arc() should hit the cache"
        );

        // Any mutation must invalidate the cache.
        e.handle_key(key(KeyCode::Char('i')));
        e.handle_key(key(KeyCode::Char('!')));
        let c = e.content_arc();
        assert!(
            !std::sync::Arc::ptr_eq(&a, &c),
            "mutation should invalidate content_arc() cache"
        );
        assert!(c.contains('!'));
    }

    #[test]
    fn content_arc_cache_invalidated_by_set_content() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("one");
        let a = e.content_arc();
        e.set_content("two");
        let b = e.content_arc();
        assert!(!std::sync::Arc::ptr_eq(&a, &b));
        assert!(b.starts_with("two"));
    }
}
