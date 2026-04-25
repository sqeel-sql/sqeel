//! Editor — the public sqeel-vim type, layered over `sqeel_buffer::Buffer`.
//!
//! This file owns the public Editor API — construction, content access,
//! mouse and goto helpers, the (buffer-level) undo stack, and insert-mode
//! session bookkeeping. All vim-specific keyboard handling lives in
//! [`vim`] and communicates with Editor through a small internal API
//! exposed via `pub(super)` fields and helper methods.

use crate::input::{Input, Key};
use crate::vim::{self, VimState};
use crate::{KeybindingMode, VimMode};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use std::sync::atomic::{AtomicU16, Ordering};

/// Where the cursor should land in the viewport after a `z`-family
/// scroll (`zz` / `zt` / `zb`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CursorScrollTarget {
    Center,
    Top,
    Bottom,
}

pub struct Editor<'a> {
    pub keybinding_mode: KeybindingMode,
    /// Reserved for the lifetime parameter — Editor used to wrap a
    /// `TextArea<'a>` whose lifetime came from this slot. Phase 7f
    /// ripped the field but the lifetime stays so downstream
    /// `Editor<'a>` consumers don't have to churn.
    _marker: std::marker::PhantomData<&'a ()>,
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
    /// Style intern table for the migration buffer's opaque
    /// `Span::style` ids. Phase 7d-ii-a wiring — `apply_window_spans`
    /// produces `(start, end, Style)` tuples for the textarea; we
    /// translate those to `sqeel_buffer::Span` by interning the
    /// `Style` here and storing the table index. The render path's
    /// `StyleResolver` looks the style back up by id.
    pub(super) style_table: Vec<ratatui::style::Style>,
    /// Vim-style register bank — `"`, `"0`–`"9`, `"a`–`"z`. Sources
    /// every `p` / `P` via the active selector (default unnamed).
    pub(super) registers: crate::registers::Registers,
    /// Per-row syntax styling, kept here so the host can do
    /// incremental window updates (see `apply_window_spans` in
    /// sqeel-tui). Same `(start_byte, end_byte, Style)` tuple shape
    /// the textarea used to host. The Buffer-side opaque-id spans are
    /// derived from this on every install.
    pub styled_spans: Vec<Vec<(usize, usize, ratatui::style::Style)>>,
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
        Self {
            _marker: std::marker::PhantomData,
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
            style_table: Vec::new(),
            registers: crate::registers::Registers::default(),
            styled_spans: Vec::new(),
        }
    }

    /// Install styled syntax spans into both the host-visible cache
    /// (`styled_spans`) and the buffer's opaque-id span table. Drops
    /// zero-width runs and clamps `end` to the line's char length so
    /// the buffer cache doesn't see runaway ranges. Replaces the
    /// previous `set_syntax_spans` + `sync_buffer_spans_from_textarea`
    /// round-trip.
    pub fn install_syntax_spans(&mut self, spans: Vec<Vec<(usize, usize, ratatui::style::Style)>>) {
        let line_byte_lens: Vec<usize> = self.buffer.lines().iter().map(|l| l.len()).collect();
        let mut by_row: Vec<Vec<sqeel_buffer::Span>> = Vec::with_capacity(spans.len());
        for (row, row_spans) in spans.iter().enumerate() {
            let line_len = line_byte_lens.get(row).copied().unwrap_or(0);
            let mut translated = Vec::with_capacity(row_spans.len());
            for (start, end, style) in row_spans {
                let end_clamped = (*end).min(line_len);
                if end_clamped <= *start {
                    continue;
                }
                let id = self.intern_style(*style);
                translated.push(sqeel_buffer::Span::new(*start, end_clamped, id));
            }
            by_row.push(translated);
        }
        self.buffer.set_spans(by_row);
        self.styled_spans = spans;
    }

    /// Snapshot of the unnamed register (the default `p` / `P` source).
    pub fn yank(&self) -> &str {
        &self.registers.unnamed.text
    }

    /// Borrow the full register bank — `"`, `"0`–`"9`, `"a`–`"z`.
    pub fn registers(&self) -> &crate::registers::Registers {
        &self.registers
    }

    /// Replace the unnamed register without touching any other slot.
    /// For host-driven imports (e.g. system clipboard); operator
    /// code uses [`record_yank`] / [`record_delete`].
    pub fn set_yank(&mut self, text: impl Into<String>) {
        let text = text.into();
        let linewise = self.vim.yank_linewise;
        self.registers.unnamed = crate::registers::Slot { text, linewise };
    }

    /// Record a yank into `"` and `"0`, plus the named target if the
    /// user prefixed `"reg`. Updates `vim.yank_linewise` for the
    /// paste path.
    pub(crate) fn record_yank(&mut self, text: String, linewise: bool) {
        self.vim.yank_linewise = linewise;
        let target = self.vim.pending_register.take();
        self.registers.record_yank(text, linewise, target);
    }

    /// Record a delete / change into `"` and the `"1`–`"9` ring.
    /// Honours the active named-register prefix.
    pub(crate) fn record_delete(&mut self, text: String, linewise: bool) {
        self.vim.yank_linewise = linewise;
        let target = self.vim.pending_register.take();
        self.registers.record_delete(text, linewise, target);
    }

    /// Intern a `ratatui::style::Style` and return the opaque id used
    /// in `sqeel_buffer::Span::style`. The render-side `StyleResolver`
    /// closure (built by [`Editor::style_resolver`]) uses the id to
    /// look up the style back. Linear-scan dedup — the table grows
    /// only as new tree-sitter token kinds appear, so it stays tiny.
    pub fn intern_style(&mut self, style: ratatui::style::Style) -> u32 {
        if let Some(idx) = self.style_table.iter().position(|s| *s == style) {
            return idx as u32;
        }
        self.style_table.push(style);
        (self.style_table.len() - 1) as u32
    }

    /// Read-only view of the style table — id `i` → `style_table[i]`.
    /// The render path passes a closure backed by this slice as the
    /// `StyleResolver` for `BufferView`.
    pub fn style_table(&self) -> &[ratatui::style::Style] {
        &self.style_table
    }

    /// Borrow the migration buffer. Host renders through this via
    /// `sqeel_buffer::BufferView`.
    pub fn buffer(&self) -> &sqeel_buffer::Buffer {
        &self.buffer
    }

    pub fn buffer_mut(&mut self) -> &mut sqeel_buffer::Buffer {
        &mut self.buffer
    }

    /// Historical reverse-sync hook from when the textarea mirrored
    /// the buffer. Now that Buffer is the cursor authority this is a
    /// no-op; call sites can remain in place during the migration.
    pub(crate) fn push_buffer_cursor_to_textarea(&mut self) {}

    /// Force the buffer viewport's top row without touching the
    /// cursor. Used by tests that simulate a scroll without the
    /// SCROLLOFF cursor adjustment that `scroll_down` / `scroll_up`
    /// apply. Note: does not touch the textarea — the migration
    /// buffer's viewport is what `BufferView` renders from, and the
    /// textarea's own scroll path would clamp the cursor into its
    /// (often-zero) visible window.
    pub fn set_viewport_top(&mut self, row: usize) {
        let last = self.buffer.row_count().saturating_sub(1);
        let target = row.min(last);
        self.buffer.viewport_mut().top_row = target;
    }

    /// Set the cursor to `(row, col)`, clamped to the buffer's
    /// content. Replaces the scattered
    /// `ed.textarea.move_cursor(CursorMove::Jump(r, c))` pattern that
    /// existed before Phase 7f.
    pub(crate) fn jump_cursor(&mut self, row: usize, col: usize) {
        self.buffer
            .set_cursor(sqeel_buffer::Position::new(row, col));
    }

    /// `(row, col)` cursor read sourced from the migration buffer.
    /// Equivalent to `self.textarea.cursor()` when the two are in
    /// sync — which is the steady state during Phase 7f because
    /// every step opens with `sync_buffer_content_from_textarea` and
    /// every ported motion pushes the result back. Prefer this over
    /// `self.textarea.cursor()` so call sites keep working unchanged
    /// once the textarea field is ripped.
    pub fn cursor(&self) -> (usize, usize) {
        let pos = self.buffer.cursor();
        (pos.row, pos.col)
    }

    /// Drain any pending LSP intent raised by the last key. Returns
    /// `None` when no intent is armed.
    pub fn take_lsp_intent(&mut self) -> Option<LspIntent> {
        self.pending_lsp.take()
    }

    /// Refresh the buffer's host-side state — sticky col + viewport
    /// height. Called from the per-step boilerplate; was the textarea
    /// → buffer mirror before Phase 7f put Buffer in charge.
    pub(crate) fn sync_buffer_from_textarea(&mut self) {
        self.buffer.set_sticky_col(self.vim.sticky_col);
        let height = self.viewport_height_value();
        self.buffer.viewport_mut().height = height;
    }

    /// Was the full textarea → buffer content sync. Buffer is the
    /// content authority now; this remains as a no-op so the per-step
    /// call sites don't have to be ripped in the same patch.
    pub(crate) fn sync_buffer_content_from_textarea(&mut self) {
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

    /// Phase 7f edit funnel: apply `edit` to the migration buffer
    /// (the eventual edit authority), mirror the result back into
    /// the textarea so the still-textarea-driven paths (insert mode,
    /// yank pipe) keep observing the same content. Returns the
    /// inverse for the host's undo stack.
    pub(super) fn mutate_edit(&mut self, edit: sqeel_buffer::Edit) -> sqeel_buffer::Edit {
        let pre_row = self.buffer.cursor().row;
        let inverse = self.buffer.apply_edit(edit);
        let pos = self.buffer.cursor();
        // Drop any folds the edit's range overlapped — vim opens the
        // surrounding fold automatically when you edit inside it. The
        // approximation here invalidates folds covering either the
        // pre-edit cursor row or the post-edit cursor row, which
        // catches the common single-line / multi-line edit shapes.
        let lo = pre_row.min(pos.row);
        let hi = pre_row.max(pos.row);
        self.buffer.invalidate_folds_in_range(lo, hi);
        self.vim.last_edit_pos = Some((pos.row, pos.col));
        self.push_buffer_content_to_textarea();
        self.mark_content_dirty();
        inverse
    }

    /// Reverse-sync helper paired with [`Editor::mutate_edit`]: rebuild
    /// the textarea from the buffer's lines + cursor, preserving yank
    /// text. Heavy (allocates a fresh `TextArea`) but correct; the
    /// textarea field disappears at the end of Phase 7f anyway.
    /// No-op since Buffer is the content authority. Retained as a
    /// shim so call sites in `mutate_edit` and friends don't have to
    /// be ripped in lockstep with the field removal.
    pub(crate) fn push_buffer_content_to_textarea(&mut self) {}

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
        let cursor = self.buffer.cursor().row;
        let top = self.buffer.viewport().top_row;
        cursor.saturating_sub(top).min(height as usize - 1) as u16
    }

    /// Returns the cursor's screen position `(x, y)` for `area` (the textarea
    /// rect). Accounts for line-number gutter and viewport scroll. Returns
    /// `None` if the cursor is outside the visible viewport.
    pub fn cursor_screen_pos(&self, area: Rect) -> Option<(u16, u16)> {
        let pos = self.buffer.cursor();
        let v = self.buffer.viewport();
        if pos.row < v.top_row || pos.col < v.top_col {
            return None;
        }
        let lnum_width = self.buffer.row_count().to_string().len() as u16 + 2;
        let dy = (pos.row - v.top_row) as u16;
        let dx = (pos.col - v.top_col) as u16;
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
        let cursor = self.cursor();
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
        let cursor = self.buffer.cursor().row;
        Some((anchor.min(cursor), anchor.max(cursor)))
    }

    pub fn block_highlight(&self) -> Option<(usize, usize, usize, usize)> {
        if self.vim_mode() != VimMode::VisualBlock {
            return None;
        }
        let (ar, ac) = self.vim.block_anchor;
        let cr = self.buffer.cursor().row;
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
                let head = self.buffer.cursor();
                Some(Selection::Char {
                    anchor: Position::new(ar, ac),
                    head,
                })
            }
            VimMode::VisualLine => {
                let anchor_row = self.vim.visual_line_anchor;
                let head_row = self.buffer.cursor().row;
                Some(Selection::Line {
                    anchor_row,
                    head_row,
                })
            }
            VimMode::VisualBlock => {
                let (ar, ac) = self.vim.block_anchor;
                let cr = self.buffer.cursor().row;
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
        self.vim.force_normal();
    }

    pub fn content(&self) -> String {
        let mut s = self.buffer.lines().join("\n");
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
        let _ = lines;
        self.buffer = sqeel_buffer::Buffer::from_str(text);
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.mark_content_dirty();
    }

    /// Install `text` as the pending yank buffer so the next `p`/`P` pastes
    /// it. Linewise is inferred from a trailing newline, matching how `yy`/`dd`
    /// shape their payload.
    pub fn seed_yank(&mut self, text: String) {
        let linewise = text.ends_with('\n');
        self.vim.yank_linewise = linewise;
        self.registers.unnamed = crate::registers::Slot { text, linewise };
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
        // Bump the buffer's viewport top within bounds.
        let total_rows = self.buffer.row_count() as isize;
        let height = self.viewport_height.load(Ordering::Relaxed) as usize;
        let cur_top = self.buffer.viewport().top_row as isize;
        let new_top = (cur_top + delta as isize)
            .max(0)
            .min((total_rows - 1).max(0)) as usize;
        self.buffer.viewport_mut().top_row = new_top;
        // Mirror to textarea so its viewport reads (still consumed by
        // a couple of helpers) stay accurate.
        let _ = cur_top;
        if height == 0 {
            return;
        }
        // Apply scrolloff: keep the cursor at least SCROLLOFF rows
        // from the visible viewport edges.
        let cursor = self.buffer.cursor();
        let margin = Self::SCROLLOFF.min(height / 2);
        let min_row = new_top + margin;
        let max_row = new_top + height.saturating_sub(1).saturating_sub(margin);
        let target_row = cursor.row.clamp(min_row, max_row.max(min_row));
        if target_row != cursor.row {
            let line_len = self
                .buffer
                .line(target_row)
                .map(|l| l.chars().count())
                .unwrap_or(0);
            let target_col = cursor.col.min(line_len.saturating_sub(1));
            self.buffer
                .set_cursor(sqeel_buffer::Position::new(target_row, target_col));
        }
    }

    pub fn goto_line(&mut self, line: usize) {
        let row = line.saturating_sub(1);
        let max = self.buffer.row_count().saturating_sub(1);
        let target = row.min(max);
        self.buffer
            .set_cursor(sqeel_buffer::Position::new(target, 0));
    }

    /// Scroll so the cursor row lands at the given viewport position:
    /// `Center` → middle row, `Top` → first row, `Bottom` → last row.
    /// Cursor stays on its absolute line; only the viewport moves.
    pub(super) fn scroll_cursor_to(&mut self, pos: CursorScrollTarget) {
        let height = self.viewport_height.load(Ordering::Relaxed) as usize;
        if height == 0 {
            return;
        }
        let cur_row = self.buffer.cursor().row;
        let cur_top = self.buffer.viewport().top_row;
        let new_top = match pos {
            CursorScrollTarget::Center => cur_row.saturating_sub(height / 2),
            CursorScrollTarget::Top => cur_row,
            CursorScrollTarget::Bottom => cur_row.saturating_sub(height.saturating_sub(1)),
        };
        if new_top == cur_top {
            return;
        }
        self.buffer.viewport_mut().top_row = new_top;
    }

    /// Translate a terminal mouse position into a (row, col) inside the document.
    /// `area` is the outer editor rect: 1-row tab bar at top (flush), then the
    /// textarea with 1 cell of horizontal pane padding on each side.
    fn mouse_to_doc_pos(&self, area: Rect, col: u16, row: u16) -> (usize, usize) {
        let lines = self.buffer.lines();
        let inner_top = area.y.saturating_add(1); // tab bar row
        let lnum_width = lines.len().to_string().len() as u16 + 2;
        let content_x = area.x.saturating_add(1).saturating_add(lnum_width);
        let rel_row = row.saturating_sub(inner_top) as usize;
        let top = self.buffer.viewport().top_row;
        let doc_row = (top + rel_row).min(lines.len().saturating_sub(1));
        let rel_col = col.saturating_sub(content_x) as usize;
        let line_len = lines.get(doc_row).map(|l| l.len()).unwrap_or(0);
        (doc_row, rel_col.min(line_len))
    }

    /// Jump the cursor to the given 1-based line/column, clamped to the document.
    pub fn jump_to(&mut self, line: usize, col: usize) {
        let r = line.saturating_sub(1);
        let max_row = self.buffer.row_count().saturating_sub(1);
        let r = r.min(max_row);
        let line_len = self.buffer.line(r).map(|l| l.chars().count()).unwrap_or(0);
        let c = col.saturating_sub(1).min(line_len);
        self.buffer.set_cursor(sqeel_buffer::Position::new(r, c));
    }

    /// Jump cursor to the terminal-space mouse position; exits Visual modes if active.
    pub fn mouse_click(&mut self, area: Rect, col: u16, row: u16) {
        if self.vim.is_visual() {
            self.vim.force_normal();
        }
        let (r, c) = self.mouse_to_doc_pos(area, col, row);
        self.buffer.set_cursor(sqeel_buffer::Position::new(r, c));
    }

    /// Begin a mouse-drag selection: anchor at current cursor and enter Visual mode.
    pub fn mouse_begin_drag(&mut self) {
        if !self.vim.is_visual_char() {
            let cursor = self.cursor();
            self.vim.enter_visual(cursor);
        }
    }

    /// Extend an in-progress mouse drag to the given terminal-space position.
    pub fn mouse_extend_drag(&mut self, area: Rect, col: u16, row: u16) {
        let (r, c) = self.mouse_to_doc_pos(area, col, row);
        self.buffer.set_cursor(sqeel_buffer::Position::new(r, c));
    }

    pub fn insert_str(&mut self, text: &str) {
        let pos = self.buffer.cursor();
        self.buffer.apply_edit(sqeel_buffer::Edit::InsertStr {
            at: pos,
            text: text.to_string(),
        });
        self.push_buffer_content_to_textarea();
        self.mark_content_dirty();
    }

    pub fn accept_completion(&mut self, completion: &str) {
        use sqeel_buffer::{Edit, MotionKind, Position};
        let cursor = self.buffer.cursor();
        let line = self.buffer.line(cursor.row).unwrap_or("").to_string();
        let chars: Vec<char> = line.chars().collect();
        let prefix_len = chars[..cursor.col.min(chars.len())]
            .iter()
            .rev()
            .take_while(|c| c.is_alphanumeric() || **c == '_')
            .count();
        if prefix_len > 0 {
            let start = Position::new(cursor.row, cursor.col - prefix_len);
            self.buffer.apply_edit(Edit::DeleteRange {
                start,
                end: cursor,
                kind: MotionKind::Char,
            });
        }
        let cursor = self.buffer.cursor();
        self.buffer.apply_edit(Edit::InsertStr {
            at: cursor,
            text: completion.to_string(),
        });
        self.push_buffer_content_to_textarea();
        self.mark_content_dirty();
    }

    pub(super) fn snapshot(&self) -> (Vec<String>, (usize, usize)) {
        let pos = self.buffer.cursor();
        (self.buffer.lines().to_vec(), (pos.row, pos.col))
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
        let text = lines.join("\n");
        self.buffer.replace_all(&text);
        self.buffer
            .set_cursor(sqeel_buffer::Position::new(cursor.0, cursor.1));
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
        e.jump_cursor(0, 8);
        e.handle_key(shift_key(KeyCode::Char('I')));
        assert_eq!(e.vim_mode(), VimMode::Insert);
        assert_eq!(e.cursor(), (0, 3));
    }

    #[test]
    fn vim_shift_a_moves_to_end_and_insert() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("hello");
        e.handle_key(shift_key(KeyCode::Char('A')));
        assert_eq!(e.vim_mode(), VimMode::Insert);
        assert_eq!(e.cursor().1, 5);
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
        assert_eq!(e.cursor().0, 10);
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
        assert_eq!(e.buffer().lines().len(), 4);
        assert!(e.buffer().lines().iter().skip(1).all(|l| l == "world"));
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
        assert_eq!(e.buffer().lines()[0], "ababab");
    }

    #[test]
    fn vim_shift_o_opens_line_above() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("hello");
        e.handle_key(shift_key(KeyCode::Char('O')));
        assert_eq!(e.vim_mode(), VimMode::Insert);
        assert_eq!(e.cursor(), (0, 0));
        assert_eq!(e.buffer().lines().len(), 2);
    }

    #[test]
    fn vim_gg_goes_to_top() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("a\nb\nc");
        e.jump_cursor(2, 0);
        e.handle_key(key(KeyCode::Char('g')));
        e.handle_key(key(KeyCode::Char('g')));
        assert_eq!(e.cursor().0, 0);
    }

    #[test]
    fn vim_shift_g_goes_to_bottom() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("a\nb\nc");
        e.handle_key(shift_key(KeyCode::Char('G')));
        assert_eq!(e.cursor().0, 2);
    }

    #[test]
    fn vim_dd_deletes_line() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("first\nsecond");
        e.handle_key(key(KeyCode::Char('d')));
        e.handle_key(key(KeyCode::Char('d')));
        assert_eq!(e.buffer().lines().len(), 1);
        assert_eq!(e.buffer().lines()[0], "second");
    }

    #[test]
    fn vim_dw_deletes_word() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("hello world");
        e.handle_key(key(KeyCode::Char('d')));
        e.handle_key(key(KeyCode::Char('w')));
        assert_eq!(e.vim_mode(), VimMode::Normal);
        assert!(!e.buffer().lines()[0].starts_with("hello"));
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
        e.jump_cursor(1, 0);
        let before = e.cursor();
        e.handle_key(key(KeyCode::Char('y')));
        e.handle_key(key(KeyCode::Char('y')));
        assert_eq!(e.cursor(), before);
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
        assert_eq!(e.buffer().lines().len(), 3);
        e.handle_key(key(KeyCode::Char('u')));
        assert_eq!(e.buffer().lines().len(), 1);
        assert_eq!(e.buffer().lines()[0], "hello");
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
        let after = e.buffer().lines()[0].clone();
        e.handle_key(key(KeyCode::Char('u')));
        assert_eq!(e.buffer().lines()[0], "hello");
        e.handle_key(ctrl_key(KeyCode::Char('r')));
        assert_eq!(e.buffer().lines()[0], after);
    }

    #[test]
    fn vim_u_undoes_dd() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("first\nsecond");
        e.handle_key(key(KeyCode::Char('d')));
        e.handle_key(key(KeyCode::Char('d')));
        assert_eq!(e.buffer().lines().len(), 1);
        e.handle_key(key(KeyCode::Char('u')));
        assert_eq!(e.buffer().lines().len(), 2);
        assert_eq!(e.buffer().lines()[0], "first");
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
        assert_eq!(e.buffer().lines()[0].chars().next(), Some('x'));
    }

    #[test]
    fn vim_tilde_toggles_case() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("hello");
        e.handle_key(key(KeyCode::Char('~')));
        assert_eq!(e.buffer().lines()[0].chars().next(), Some('H'));
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
        e.set_viewport_height(height);
    }

    #[test]
    fn zz_centers_cursor_in_viewport() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content(&many_lines(100));
        prime_viewport(&mut e, 20);
        e.jump_cursor(50, 0);
        e.handle_key(key(KeyCode::Char('z')));
        e.handle_key(key(KeyCode::Char('z')));
        assert_eq!(e.buffer().viewport().top_row, 40);
        assert_eq!(e.cursor().0, 50);
    }

    #[test]
    fn zt_puts_cursor_at_viewport_top() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content(&many_lines(100));
        prime_viewport(&mut e, 20);
        e.jump_cursor(50, 0);
        e.handle_key(key(KeyCode::Char('z')));
        e.handle_key(key(KeyCode::Char('t')));
        assert_eq!(e.buffer().viewport().top_row, 50);
        assert_eq!(e.cursor().0, 50);
    }

    #[test]
    fn ctrl_a_increments_number_at_cursor() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("x = 41");
        e.handle_key(ctrl_key(KeyCode::Char('a')));
        assert_eq!(e.buffer().lines()[0], "x = 42");
        assert_eq!(e.cursor(), (0, 5));
    }

    #[test]
    fn ctrl_a_finds_number_to_right_of_cursor() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("foo 99 bar");
        e.handle_key(ctrl_key(KeyCode::Char('a')));
        assert_eq!(e.buffer().lines()[0], "foo 100 bar");
        assert_eq!(e.cursor(), (0, 6));
    }

    #[test]
    fn ctrl_a_with_count_adds_count() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("x = 10");
        for d in "5".chars() {
            e.handle_key(key(KeyCode::Char(d)));
        }
        e.handle_key(ctrl_key(KeyCode::Char('a')));
        assert_eq!(e.buffer().lines()[0], "x = 15");
    }

    #[test]
    fn ctrl_x_decrements_number() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("n=5");
        e.handle_key(ctrl_key(KeyCode::Char('x')));
        assert_eq!(e.buffer().lines()[0], "n=4");
    }

    #[test]
    fn ctrl_x_crosses_zero_into_negative() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("v=0");
        e.handle_key(ctrl_key(KeyCode::Char('x')));
        assert_eq!(e.buffer().lines()[0], "v=-1");
    }

    #[test]
    fn ctrl_a_on_negative_number_increments_toward_zero() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("a = -5");
        e.handle_key(ctrl_key(KeyCode::Char('a')));
        assert_eq!(e.buffer().lines()[0], "a = -4");
    }

    #[test]
    fn ctrl_a_noop_when_no_digit_on_line() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("no digits here");
        e.handle_key(ctrl_key(KeyCode::Char('a')));
        assert_eq!(e.buffer().lines()[0], "no digits here");
    }

    #[test]
    fn zb_puts_cursor_at_viewport_bottom() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content(&many_lines(100));
        prime_viewport(&mut e, 20);
        e.jump_cursor(50, 0);
        e.handle_key(key(KeyCode::Char('z')));
        e.handle_key(key(KeyCode::Char('b')));
        assert_eq!(e.buffer().viewport().top_row, 31);
        assert_eq!(e.cursor().0, 50);
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
