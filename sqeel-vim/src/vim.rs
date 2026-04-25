//! Vim-mode engine.
//!
//! Implements a command grammar of the form
//!
//! ```text
//! Command := count? (operator count? (motion | text-object)
//!                   | motion
//!                   | insert-entry
//!                   | misc)
//! ```
//!
//! The parser is a small state machine driven by one `Input` at a time.
//! Motions and text objects produce a [`Range`] (with inclusive/exclusive
//! / linewise classification). A single [`Operator`] implementation
//! applies a range — so `dw`, `d$`, `daw`, and visual `d` all go through
//! the same code path.
//!
//! The most recent mutating command is stored in
//! [`VimState::last_change`] so `.` can replay it.
//!
//! # Roadmap
//!
//! Tracked in the original plan at
//! `~/.claude/plans/look-at-the-vim-curried-fern.md`. Phases still
//! outstanding — each one can land as an isolated PR.
//!
//! ## P3 — Registers & marks
//!
//! - TODO: `RegisterBank` indexed by char:
//!     - unnamed `""`, last-yank `"0`, small-delete `"-`
//!     - named `"a-"z` (uppercase `"A-"Z` appends instead of overwriting)
//!     - blackhole `"_`
//!     - system clipboard `"+` / `"*` (wire to `crate::clipboard::Clipboard`)
//!     - read-only `":`, `".`, `"%` — surface in `:reg` output
//! - TODO: route every yank / cut / paste through the bank. Parser needs
//!   a `"{reg}` prefix state that captures the target register before a
//!   count / operator.
//! - TODO: `m{a-z}` sets a mark in a `HashMap<char, (buffer_id, row, col)>`;
//!   `'x` jumps to the line (FirstNonBlank), `` `x `` to the exact cell.
//!   Uppercase marks are global across tabs; lowercase are per-buffer.
//! - TODO: `''` and `` `` `` jump to the last-jump position; `'[` `']`
//!   `'<` `'>` bound the last change / visual region.
//! - TODO: `:reg` and `:marks` ex commands.
//!
//! ## P4 — Macros
//!
//! - TODO: `q{a-z}` starts recording raw `Input`s into the register;
//!   next `q` stops.
//! - TODO: `@{a-z}` replays the register by re-feeding inputs through
//!   `step`. `@@` repeats the last macro. Nested macros need a sane
//!   depth cap (e.g. 100) to avoid runaway loops.
//! - TODO: ensure recording doesn't capture the initial `q{a-z}` itself.
//!
//! ## P6 — Polish (still outstanding)
//!
//! - TODO: indent operators `>` / `<` (with line + text-object targets).
//! - TODO: format operator `=` — map to whatever SQL formatter we wire
//!   up; for now stub that returns the range unchanged with a toast.
//! - TODO: case operators `gU` / `gu` / `g~` on a range (already have
//!   single-char `~`).
//! - TODO: screen motions `H` / `M` / `L` once we track the render
//!   viewport height inside Editor.
//! - TODO: scroll-to-cursor motions `zz` / `zt` / `zb`.
//!
//! ## Known substrate / divergence notes
//!
//! - TODO: insert-mode indent helpers — `Ctrl-t` / `Ctrl-d` (increase /
//!   decrease indent on current line) and `Ctrl-r <reg>` (paste from a
//!   register). `Ctrl-r` needs the `RegisterBank` from P3 to be useful.
//! - TODO: `/` and `?` search prompts still live in `sqeel-tui/src/lib.rs`.
//!   The plan calls for moving them into the editor (so the editor owns
//!   `last_search_pattern` rather than the TUI loop). Safe to defer.

use crate::VimMode;
use tui_textarea::{CursorMove, Input, Key};

use crate::editor::Editor;

// ─── Modes & parser state ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
    Visual,
    VisualLine,
    /// Column-oriented selection (`Ctrl-V`). Unlike the other visual
    /// modes this one doesn't use tui-textarea's single-range selection
    /// — the block corners live in [`VimState::block_anchor`] and the
    /// live cursor. Operators read the rectangle off those two points.
    VisualBlock,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum Pending {
    #[default]
    None,
    /// Operator seen; still waiting for a motion / text-object / double-op.
    /// `count1` is any count pressed before the operator.
    Op { op: Operator, count1: usize },
    /// Operator + 'i' or 'a' seen; waiting for the text-object character.
    OpTextObj {
        op: Operator,
        count1: usize,
        inner: bool,
    },
    /// Operator + 'g' seen (for `dgg`).
    OpG { op: Operator, count1: usize },
    /// Bare `g` seen in normal/visual — looking for `g`, `e`, `E`, …
    G,
    /// Bare `f`/`F`/`t`/`T` — looking for the target char.
    Find { forward: bool, till: bool },
    /// Operator + `f`/`F`/`t`/`T` — looking for target char.
    OpFind {
        op: Operator,
        count1: usize,
        forward: bool,
        till: bool,
    },
    /// `r` pressed — waiting for the replacement char.
    Replace,
    /// Visual mode + `i` or `a` pressed — waiting for the text-object
    /// character to extend the selection over.
    VisualTextObj { inner: bool },
    /// Bare `z` seen — looking for `z` (center), `t` (top), `b` (bottom).
    Z,
    /// `m` pressed — waiting for the mark letter to set.
    SetMark,
    /// `'` pressed — waiting for the mark letter to jump to its line
    /// (lands on first non-blank, linewise for operators).
    GotoMarkLine,
    /// `` ` `` pressed — waiting for the mark letter to jump to the
    /// exact `(row, col)` stored at set time (charwise for operators).
    GotoMarkChar,
}

// ─── Operator / Motion / TextObject ────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    Delete,
    Change,
    Yank,
    /// `gU{motion}` — uppercase the range. Entered via the `g` prefix
    /// in normal mode or `U` in visual mode.
    Uppercase,
    /// `gu{motion}` — lowercase the range. `u` in visual mode.
    Lowercase,
    /// `g~{motion}` — toggle case of the range. `~` in visual mode
    /// (character at the cursor for the single-char `~` command stays
    /// its own code path in normal mode).
    ToggleCase,
    /// `>{motion}` — indent the line range by `shiftwidth` spaces.
    /// Always linewise, even when the motion is char-wise — mirrors
    /// vim's behaviour where `>w` indents the current line, not the
    /// word on it.
    Indent,
    /// `<{motion}` — outdent the line range (remove up to
    /// `shiftwidth` leading spaces per line).
    Outdent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    WordFwd,
    BigWordFwd,
    WordBack,
    BigWordBack,
    WordEnd,
    BigWordEnd,
    /// `ge` — backward word end.
    WordEndBack,
    /// `gE` — backward WORD end.
    BigWordEndBack,
    LineStart,
    FirstNonBlank,
    LineEnd,
    FileTop,
    FileBottom,
    Find {
        ch: char,
        forward: bool,
        till: bool,
    },
    FindRepeat {
        reverse: bool,
    },
    MatchBracket,
    WordAtCursor {
        forward: bool,
    },
    /// `n` / `N` — repeat the last `/` or `?` search.
    SearchNext {
        reverse: bool,
    },
    /// `H` — cursor to viewport top (plus `count - 1` rows down).
    ViewportTop,
    /// `M` — cursor to viewport middle.
    ViewportMiddle,
    /// `L` — cursor to viewport bottom (minus `count - 1` rows up).
    ViewportBottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextObject {
    Word { big: bool },
    Quote(char),
    Bracket(char),
    Paragraph,
}

/// Classification determines how operators treat the range end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionKind {
    /// Range end is exclusive (end column not included). Typical: h, l, w, 0, $.
    Exclusive,
    /// Range end is inclusive. Typical: e, f, t, %.
    Inclusive,
    /// Whole lines from top row to bottom row. Typical: j, k, gg, G.
    Linewise,
}

// ─── Dot-repeat storage ────────────────────────────────────────────────────

/// Information needed to replay a mutating change via `.`.
#[derive(Debug, Clone)]
enum LastChange {
    /// Operator over a motion.
    OpMotion {
        op: Operator,
        motion: Motion,
        count: usize,
        inserted: Option<String>,
    },
    /// Operator over a text-object.
    OpTextObj {
        op: Operator,
        obj: TextObject,
        inner: bool,
        inserted: Option<String>,
    },
    /// `dd`, `cc`, `yy` with a count.
    LineOp {
        op: Operator,
        count: usize,
        inserted: Option<String>,
    },
    /// `x`, `X` with a count.
    CharDel { forward: bool, count: usize },
    /// `r<ch>` with a count.
    ReplaceChar { ch: char, count: usize },
    /// `~` with a count.
    ToggleCase { count: usize },
    /// `J` with a count.
    JoinLine { count: usize },
    /// `p` / `P` with a count.
    Paste { before: bool, count: usize },
    /// `D` (delete to EOL).
    DeleteToEol { inserted: Option<String> },
    /// `o` / `O` + the inserted text.
    OpenLine { above: bool, inserted: String },
    /// `i`/`I`/`a`/`A` + inserted text.
    InsertAt {
        entry: InsertEntry,
        inserted: String,
        count: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InsertEntry {
    I,
    A,
    ShiftI,
    ShiftA,
}

// ─── VimState ──────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct VimState {
    mode: Mode,
    pending: Pending,
    count: usize,
    /// Last `f`/`F`/`t`/`T` target, for `;` / `,` repeat.
    last_find: Option<(char, bool, bool)>,
    last_change: Option<LastChange>,
    /// Captured on insert-mode entry: count, buffer snapshot, entry kind.
    insert_session: Option<InsertSession>,
    /// (row, col) anchor for char-wise Visual mode. Set on entry, used
    /// to compute the highlight range and the operator range without
    /// relying on tui-textarea's live selection.
    pub(super) visual_anchor: (usize, usize),
    /// Row anchor for VisualLine mode.
    pub(super) visual_line_anchor: usize,
    /// (row, col) anchor for VisualBlock mode. The live cursor is the
    /// opposite corner.
    pub(super) block_anchor: (usize, usize),
    /// Intended "virtual" column for the block's active corner. j/k
    /// clamp cursor.col to shorter rows, which would collapse the
    /// block across ragged content — so we remember the desired column
    /// separately and use it for block bounds / insert-column
    /// computations. Updated by h/l only.
    pub(super) block_vcol: usize,
    /// Vim's "sticky column" (curswant). `None` before the first
    /// motion — the next vertical motion bootstraps from the current
    /// cursor column. Horizontal motions refresh this to the new
    /// cursor column; vertical motions *read* it to restore the
    /// cursor on the destination row when that row is long enough,
    /// so bouncing through a shorter or empty line doesn't drag the
    /// cursor back to column 0.
    pub(super) sticky_col: Option<usize>,
    /// Track whether the last yank/cut was linewise (drives `p`/`P` layout).
    pub(super) yank_linewise: bool,
    /// Set while replaying `.` / last-change so we don't re-record it.
    replaying: bool,
    /// Entered Normal from Insert via `Ctrl-o`; after the next complete
    /// normal-mode command we return to Insert.
    one_shot_normal: bool,
    /// Live `/` or `?` prompt. `None` outside search-prompt mode.
    pub(super) search_prompt: Option<SearchPrompt>,
    /// Most recent committed search pattern. Surfaced to host apps via
    /// [`Editor::last_search`] so their status line can render a hint
    /// and so `n` / `N` have something to repeat.
    pub(super) last_search: Option<String>,
    /// Back half of the jumplist — `Ctrl-o` pops from here. Populated
    /// with the pre-motion cursor when a "big jump" motion fires
    /// (`gg`/`G`, `%`, `*`/`#`, `n`/`N`, `H`/`M`/`L`, committed `/` or
    /// `?`). Capped at 100 entries.
    pub(super) jump_back: Vec<(usize, usize)>,
    /// Forward half — `Ctrl-i` pops from here. Cleared by any new big
    /// jump, matching vim's "branch off trims forward history" rule.
    pub(super) jump_fwd: Vec<(usize, usize)>,
    /// Buffer-local lowercase marks. `m{a-z}` stores the current
    /// cursor `(row, col)` under the letter; `'{a-z}` and `` `{a-z} ``
    /// read it back. Uppercase / global marks aren't supported
    /// (single-buffer model).
    pub(super) marks: std::collections::HashMap<char, (usize, usize)>,
}

/// Active `/` or `?` search prompt. Text mutations drive the textarea's
/// live search pattern so matches highlight as the user types.
#[derive(Debug, Clone)]
pub struct SearchPrompt {
    pub text: String,
    pub cursor: usize,
    pub forward: bool,
}

#[derive(Debug, Clone)]
struct InsertSession {
    count: usize,
    /// Min/max row visited during this session. Widens on every key.
    row_min: usize,
    row_max: usize,
    /// Snapshot of the full buffer at session entry. Used to diff the
    /// affected row window at finish without being fooled by cursor
    /// navigation through rows the user never edited.
    before_lines: Vec<String>,
    reason: InsertReason,
}

#[derive(Debug, Clone)]
enum InsertReason {
    /// Plain entry via i/I/a/A — recorded as `InsertAt`.
    Enter(InsertEntry),
    /// Entry via `o`/`O` — records OpenLine on Esc.
    Open { above: bool },
    /// Entry via an operator's change side-effect. Retro-fills the
    /// stored last-change's `inserted` field on Esc.
    AfterChange,
    /// Entry via `C` (delete to EOL + insert).
    DeleteToEol,
    /// Entry via an insert triggered during dot-replay — don't touch
    /// last_change because the outer replay will restore it.
    ReplayOnly,
    /// `I` or `A` from VisualBlock: insert the typed text at `col` on
    /// every row in `top..=bot`. `col` is the start column for `I`, the
    /// one-past-block-end column for `A`.
    BlockEdge { top: usize, bot: usize, col: usize },
}

impl VimState {
    pub fn public_mode(&self) -> VimMode {
        match self.mode {
            Mode::Normal => VimMode::Normal,
            Mode::Insert => VimMode::Insert,
            Mode::Visual => VimMode::Visual,
            Mode::VisualLine => VimMode::VisualLine,
            Mode::VisualBlock => VimMode::VisualBlock,
        }
    }

    pub fn force_normal(&mut self) {
        self.mode = Mode::Normal;
        self.pending = Pending::None;
        self.count = 0;
        self.insert_session = None;
    }

    pub fn is_visual(&self) -> bool {
        matches!(
            self.mode,
            Mode::Visual | Mode::VisualLine | Mode::VisualBlock
        )
    }

    pub fn is_visual_char(&self) -> bool {
        self.mode == Mode::Visual
    }

    pub fn enter_visual(&mut self, anchor: (usize, usize)) {
        self.visual_anchor = anchor;
        self.mode = Mode::Visual;
    }
}

// ─── Entry point ───────────────────────────────────────────────────────────

/// Open the `/` (forward) or `?` (backward) search prompt. Clears any
/// live search highlight until the user commits a query.
fn enter_search(ed: &mut Editor<'_>, forward: bool) {
    ed.vim.search_prompt = Some(SearchPrompt {
        text: String::new(),
        cursor: 0,
        forward,
    });
    ed.vim.last_search = None;
    let _ = ed.textarea.set_search_pattern("");
}

fn step_search_prompt(ed: &mut Editor<'_>, input: Input) -> bool {
    match input.key {
        Key::Esc => {
            // Cancel. Drop the prompt but keep the highlighted matches
            // so `n` / `N` can repeat whatever was typed.
            let text = ed
                .vim
                .search_prompt
                .take()
                .map(|p| p.text)
                .unwrap_or_default();
            if !text.is_empty() {
                ed.vim.last_search = Some(text);
            }
        }
        Key::Enter => {
            let prompt = ed.vim.search_prompt.take();
            if let Some(p) = prompt {
                if !p.text.is_empty() {
                    let _ = ed.textarea.set_search_pattern(&p.text);
                    let pre = ed.textarea.cursor();
                    if p.forward {
                        ed.textarea.search_forward(false);
                    } else {
                        ed.textarea.search_back(false);
                    }
                    if ed.textarea.cursor() != pre {
                        push_jump(ed, pre);
                    }
                    ed.vim.last_search = Some(p.text);
                } else {
                    ed.vim.last_search = None;
                }
            }
        }
        Key::Backspace => {
            if let Some(p) = ed.vim.search_prompt.as_mut()
                && p.text.pop().is_some()
            {
                p.cursor = p.text.chars().count();
                let _ = ed.textarea.set_search_pattern(&p.text);
            }
        }
        Key::Char(c) => {
            if let Some(p) = ed.vim.search_prompt.as_mut() {
                p.text.push(c);
                p.cursor = p.text.chars().count();
                let _ = ed.textarea.set_search_pattern(&p.text);
            }
        }
        _ => {}
    }
    true
}

pub fn step(ed: &mut Editor<'_>, input: Input) -> bool {
    // Phase 7f port: any cursor / content the host changed between
    // steps (mouse jumps, paste, programmatic set_content, …) needs
    // to land in the migration buffer before motion handlers that
    // call into `Buffer::move_*` see a stale state.
    ed.sync_buffer_content_from_textarea();
    // Search prompt eats all keys until Enter / Esc.
    if ed.vim.search_prompt.is_some() {
        return step_search_prompt(ed, input);
    }
    let was_insert = ed.vim.mode == Mode::Insert;
    let consumed = match ed.vim.mode {
        Mode::Insert => step_insert(ed, input),
        _ => step_normal(ed, input),
    };
    // Ctrl-o in insert mode queues a single normal-mode command; once
    // that command finishes (pending cleared, not in operator / visual),
    // drop back to insert without replaying the insert session.
    if !was_insert
        && ed.vim.one_shot_normal
        && ed.vim.mode == Mode::Normal
        && matches!(ed.vim.pending, Pending::None)
    {
        ed.vim.one_shot_normal = false;
        ed.vim.mode = Mode::Insert;
    }
    // Phase 7c: every step ends with the migration buffer mirroring
    // the textarea's content + cursor + viewport. Edit-emitting paths
    // (insert_char, delete_char, …) inside `step_insert` /
    // `step_normal` thus all flow through here without each call
    // site needing to remember to sync.
    ed.sync_buffer_content_from_textarea();
    consumed
}

// ─── Insert mode ───────────────────────────────────────────────────────────

fn step_insert(ed: &mut Editor<'_>, input: Input) -> bool {
    if input.key == Key::Esc {
        finish_insert_session(ed);
        ed.vim.mode = Mode::Normal;
        // Vim convention: pull the cursor back one cell on exit when
        // possible. Sticky column then mirrors the *visible* post-Back
        // column so the next vertical motion lands where the user
        // actually sees the cursor — not one cell to the right.
        let col = ed.textarea.cursor().1;
        if col > 0 {
            ed.textarea.move_cursor(CursorMove::Back);
        }
        ed.vim.sticky_col = Some(ed.textarea.cursor().1);
        return true;
    }

    // Ctrl-prefixed insert-mode shortcuts.
    if input.ctrl {
        match input.key {
            Key::Char('w') => {
                ed.mutate(|t| t.delete_word());
                return true;
            }
            Key::Char('u') => {
                ed.mutate(|t| t.delete_line_by_head());
                return true;
            }
            Key::Char('h') => {
                ed.mutate(|t| t.delete_char());
                return true;
            }
            Key::Char('o') => {
                // One-shot normal: leave insert mode for the next full
                // normal-mode command, then come back.
                ed.vim.one_shot_normal = true;
                ed.vim.mode = Mode::Normal;
                return true;
            }
            Key::Char('t') => {
                // Insert-mode indent: prepend one shiftwidth to the
                // current line's leading whitespace. Cursor shifts
                // right by the same amount so the user keeps typing
                // at their logical position.
                let (row, col) = ed.textarea.cursor();
                indent_rows(ed, row, row, 1);
                ed.textarea
                    .move_cursor(CursorMove::Jump(row, col + SHIFTWIDTH));
                return true;
            }
            Key::Char('d') => {
                // Insert-mode outdent: drop up to one shiftwidth of
                // leading whitespace. Cursor shifts left by the amount
                // actually stripped.
                let (row, col) = ed.textarea.cursor();
                let before_len = ed.textarea.lines()[row].len();
                outdent_rows(ed, row, row, 1);
                let after_len = ed.textarea.lines()[row].len();
                let stripped = before_len.saturating_sub(after_len);
                let new_col = col.saturating_sub(stripped);
                ed.textarea.move_cursor(CursorMove::Jump(row, new_col));
                return true;
            }
            _ => {}
        }
    }

    // Widen the session's visited row window *before* handling the key
    // so navigation-only keystrokes (arrow keys) still extend the range.
    if let Some(ref mut session) = ed.vim.insert_session {
        let (row, _) = ed.textarea.cursor();
        session.row_min = session.row_min.min(row);
        session.row_max = session.row_max.max(row);
    }
    if ed.textarea.input(input) {
        ed.mark_content_dirty();
        if let Some(ref mut session) = ed.vim.insert_session {
            let (row, _) = ed.textarea.cursor();
            session.row_min = session.row_min.min(row);
            session.row_max = session.row_max.max(row);
        }
    }
    true
}

fn finish_insert_session(ed: &mut Editor<'_>) {
    let Some(session) = ed.vim.insert_session.take() else {
        return;
    };
    let lines = ed.textarea.lines();
    // Clamp both slices to their respective bounds — the buffer may have
    // grown (Enter splits rows) or shrunk (Backspace joins rows) during
    // the session, so row_max can overshoot either side.
    let after_end = session.row_max.min(lines.len().saturating_sub(1));
    let before_end = session
        .row_max
        .min(session.before_lines.len().saturating_sub(1));
    let before = if before_end >= session.row_min && session.row_min < session.before_lines.len() {
        session.before_lines[session.row_min..=before_end].join("\n")
    } else {
        String::new()
    };
    let after = if after_end >= session.row_min && session.row_min < lines.len() {
        lines[session.row_min..=after_end].join("\n")
    } else {
        String::new()
    };
    let inserted = extract_inserted(&before, &after);
    if !inserted.is_empty() && session.count > 1 && !ed.vim.replaying {
        for _ in 0..session.count - 1 {
            ed.mutate(|t| t.insert_str(&inserted));
        }
    }
    if let InsertReason::BlockEdge { top, bot, col } = session.reason {
        if !inserted.is_empty() && top < bot && !ed.vim.replaying {
            for r in (top + 1)..=bot {
                let line_len = ed.textarea.lines()[r].chars().count();
                if col > line_len {
                    ed.textarea.move_cursor(CursorMove::Jump(r, line_len));
                    let pad: String = std::iter::repeat_n(' ', col - line_len).collect();
                    ed.mutate(|t| t.insert_str(&pad));
                } else {
                    ed.textarea.move_cursor(CursorMove::Jump(r, col));
                }
                ed.mutate(|t| t.insert_str(&inserted));
            }
            ed.textarea.move_cursor(CursorMove::Jump(top, col));
        }
        return;
    }
    if ed.vim.replaying {
        return;
    }
    match session.reason {
        InsertReason::Enter(entry) => {
            ed.vim.last_change = Some(LastChange::InsertAt {
                entry,
                inserted,
                count: session.count,
            });
        }
        InsertReason::Open { above } => {
            ed.vim.last_change = Some(LastChange::OpenLine { above, inserted });
        }
        InsertReason::AfterChange => {
            if let Some(
                LastChange::OpMotion { inserted: ins, .. }
                | LastChange::OpTextObj { inserted: ins, .. }
                | LastChange::LineOp { inserted: ins, .. },
            ) = ed.vim.last_change.as_mut()
            {
                *ins = Some(inserted);
            }
        }
        InsertReason::DeleteToEol => {
            ed.vim.last_change = Some(LastChange::DeleteToEol {
                inserted: Some(inserted),
            });
        }
        InsertReason::ReplayOnly => {}
        InsertReason::BlockEdge { .. } => unreachable!("handled above"),
    }
}

fn begin_insert(ed: &mut Editor<'_>, count: usize, reason: InsertReason) {
    let record = !matches!(reason, InsertReason::ReplayOnly);
    if record {
        ed.push_undo();
    }
    let reason = if ed.vim.replaying {
        InsertReason::ReplayOnly
    } else {
        reason
    };
    let (row, _) = ed.textarea.cursor();
    ed.vim.insert_session = Some(InsertSession {
        count,
        row_min: row,
        row_max: row,
        before_lines: ed.textarea.lines().to_vec(),
        reason,
    });
    ed.vim.mode = Mode::Insert;
}

// ─── Normal / Visual / Operator-pending dispatcher ─────────────────────────

fn step_normal(ed: &mut Editor<'_>, input: Input) -> bool {
    // Consume digits first — except '0' at start of count (that's LineStart).
    if let Key::Char(d @ '0'..='9') = input.key
        && !input.ctrl
        && !input.alt
        && !matches!(
            ed.vim.pending,
            Pending::Replace
                | Pending::Find { .. }
                | Pending::OpFind { .. }
                | Pending::VisualTextObj { .. }
        )
        && (d != '0' || ed.vim.count > 0)
    {
        ed.vim.count = ed.vim.count.saturating_mul(10) + (d as usize - '0' as usize);
        return true;
    }

    // Handle pending two-key sequences first.
    match std::mem::take(&mut ed.vim.pending) {
        Pending::Replace => return handle_replace(ed, input),
        Pending::Find { forward, till } => return handle_find_target(ed, input, forward, till),
        Pending::OpFind {
            op,
            count1,
            forward,
            till,
        } => return handle_op_find_target(ed, input, op, count1, forward, till),
        Pending::G => return handle_after_g(ed, input),
        Pending::OpG { op, count1 } => return handle_op_after_g(ed, input, op, count1),
        Pending::Op { op, count1 } => return handle_after_op(ed, input, op, count1),
        Pending::OpTextObj { op, count1, inner } => {
            return handle_text_object(ed, input, op, count1, inner);
        }
        Pending::VisualTextObj { inner } => {
            return handle_visual_text_obj(ed, input, inner);
        }
        Pending::Z => return handle_after_z(ed, input),
        Pending::SetMark => return handle_set_mark(ed, input),
        Pending::GotoMarkLine => return handle_goto_mark(ed, input, true),
        Pending::GotoMarkChar => return handle_goto_mark(ed, input, false),
        Pending::None => {}
    }

    let count = take_count(&mut ed.vim);

    // Common normal / visual keys.
    match input.key {
        Key::Esc => {
            if ed.vim.is_visual() {
                ed.textarea.cancel_selection();
            }
            ed.vim.force_normal();
            return true;
        }
        Key::Char('v') if !input.ctrl && ed.vim.mode == Mode::Normal => {
            ed.textarea.cancel_selection();
            ed.vim.visual_anchor = ed.textarea.cursor();
            ed.vim.mode = Mode::Visual;
            return true;
        }
        Key::Char('V') if !input.ctrl && ed.vim.mode == Mode::Normal => {
            let (row, _) = ed.textarea.cursor();
            ed.vim.visual_line_anchor = row;
            refresh_visual_line_selection(ed);
            ed.vim.mode = Mode::VisualLine;
            return true;
        }
        Key::Char('v') if !input.ctrl && ed.vim.mode == Mode::VisualLine => {
            ed.textarea.cancel_selection();
            ed.vim.visual_anchor = ed.textarea.cursor();
            ed.vim.mode = Mode::Visual;
            return true;
        }
        Key::Char('V') if !input.ctrl && ed.vim.mode == Mode::Visual => {
            ed.textarea.cancel_selection();
            let (row, _) = ed.textarea.cursor();
            ed.vim.visual_line_anchor = row;
            refresh_visual_line_selection(ed);
            ed.vim.mode = Mode::VisualLine;
            return true;
        }
        Key::Char('v') if input.ctrl && ed.vim.mode == Mode::Normal => {
            ed.textarea.cancel_selection();
            let cur = ed.textarea.cursor();
            ed.vim.block_anchor = cur;
            ed.vim.block_vcol = cur.1;
            ed.vim.mode = Mode::VisualBlock;
            return true;
        }
        Key::Char('v') if input.ctrl && ed.vim.mode == Mode::VisualBlock => {
            // Second Ctrl-v exits block mode back to Normal.
            ed.vim.mode = Mode::Normal;
            return true;
        }
        _ => {}
    }

    // Visual mode: operators act on the current selection.
    if ed.vim.is_visual()
        && let Some(op) = visual_operator(&input)
    {
        apply_visual_operator(ed, op);
        return true;
    }

    // VisualBlock: extra commands beyond the standard y/d/c/x — `r`
    // replaces the block with a single char, `I` / `A` enter insert
    // mode at the block's left / right edge and repeat on every row.
    if ed.vim.mode == Mode::VisualBlock && !input.ctrl {
        match input.key {
            Key::Char('r') => {
                ed.vim.pending = Pending::Replace;
                return true;
            }
            Key::Char('I') => {
                let (top, bot, left, _right) = block_bounds(ed);
                ed.textarea.move_cursor(CursorMove::Jump(top, left));
                ed.vim.mode = Mode::Normal;
                begin_insert(
                    ed,
                    1,
                    InsertReason::BlockEdge {
                        top,
                        bot,
                        col: left,
                    },
                );
                return true;
            }
            Key::Char('A') => {
                let (top, bot, _left, right) = block_bounds(ed);
                let line_len = ed.textarea.lines()[top].chars().count();
                let col = (right + 1).min(line_len);
                ed.textarea.move_cursor(CursorMove::Jump(top, col));
                ed.vim.mode = Mode::Normal;
                begin_insert(ed, 1, InsertReason::BlockEdge { top, bot, col });
                return true;
            }
            _ => {}
        }
    }

    // Visual mode: `i` / `a` start a text-object extension.
    if matches!(ed.vim.mode, Mode::Visual | Mode::VisualLine)
        && !input.ctrl
        && matches!(input.key, Key::Char('i') | Key::Char('a'))
    {
        let inner = matches!(input.key, Key::Char('i'));
        ed.vim.pending = Pending::VisualTextObj { inner };
        return true;
    }

    // Ctrl-prefixed scrolling + misc. Vim semantics: Ctrl-d / Ctrl-u
    // move the cursor by half a window, Ctrl-f / Ctrl-b by a full
    // window. Viewport follows the cursor. Cursor lands on the first
    // non-blank of the target row (matches vim).
    if input.ctrl
        && let Key::Char(c) = input.key
    {
        match c {
            'd' => {
                scroll_cursor_rows(ed, viewport_half_rows(ed, count) as isize);
                return true;
            }
            'u' => {
                scroll_cursor_rows(ed, -(viewport_half_rows(ed, count) as isize));
                return true;
            }
            'f' => {
                scroll_cursor_rows(ed, viewport_full_rows(ed, count) as isize);
                return true;
            }
            'b' => {
                scroll_cursor_rows(ed, -(viewport_full_rows(ed, count) as isize));
                return true;
            }
            'r' => {
                do_redo(ed);
                return true;
            }
            'a' if ed.vim.mode == Mode::Normal => {
                adjust_number(ed, count.max(1) as i64);
                return true;
            }
            'x' if ed.vim.mode == Mode::Normal => {
                adjust_number(ed, -(count.max(1) as i64));
                return true;
            }
            'o' if ed.vim.mode == Mode::Normal => {
                for _ in 0..count.max(1) {
                    jump_back(ed);
                }
                return true;
            }
            'i' if ed.vim.mode == Mode::Normal => {
                for _ in 0..count.max(1) {
                    jump_forward(ed);
                }
                return true;
            }
            _ => {}
        }
    }

    // `Tab` in normal mode is also `Ctrl-i` — vim aliases them.
    if !input.ctrl && input.key == Key::Tab && ed.vim.mode == Mode::Normal {
        for _ in 0..count.max(1) {
            jump_forward(ed);
        }
        return true;
    }

    // Motion-only commands.
    if let Some(motion) = parse_motion(&input) {
        execute_motion(ed, motion.clone(), count);
        if ed.vim.mode == Mode::VisualLine {
            refresh_visual_line_selection(ed);
        }
        // Block mode: maintain the virtual column across j/k clamps.
        if ed.vim.mode == Mode::VisualBlock {
            update_block_vcol(ed, &motion);
        }
        if let Motion::Find { ch, forward, till } = motion {
            ed.vim.last_find = Some((ch, forward, till));
        }
        return true;
    }

    // Mode transitions + pure normal-mode commands (not applicable in visual).
    if ed.vim.mode == Mode::Normal && handle_normal_only(ed, &input, count) {
        return true;
    }

    // Operator triggers in normal mode.
    if ed.vim.mode == Mode::Normal
        && let Key::Char(op_ch) = input.key
        && !input.ctrl
        && let Some(op) = char_to_operator(op_ch)
    {
        ed.vim.pending = Pending::Op { op, count1: count };
        return true;
    }

    // `f`/`F`/`t`/`T` entry.
    if ed.vim.mode == Mode::Normal
        && let Some((forward, till)) = find_entry(&input)
    {
        ed.vim.count = count;
        ed.vim.pending = Pending::Find { forward, till };
        return true;
    }

    // `g` prefix.
    if !input.ctrl && input.key == Key::Char('g') && ed.vim.mode == Mode::Normal {
        ed.vim.count = count;
        ed.vim.pending = Pending::G;
        return true;
    }

    // `z` prefix (zz / zt / zb — cursor-relative viewport scrolls).
    if !input.ctrl && input.key == Key::Char('z') && ed.vim.mode == Mode::Normal {
        ed.vim.pending = Pending::Z;
        return true;
    }

    // Mark set / jump entries. `m` arms the set-mark pending state;
    // `'` and `` ` `` arm the goto states (linewise vs charwise). The
    // mark letter is consumed on the next keystroke.
    if !input.ctrl && ed.vim.mode == Mode::Normal {
        match input.key {
            Key::Char('m') => {
                ed.vim.pending = Pending::SetMark;
                return true;
            }
            Key::Char('\'') => {
                ed.vim.pending = Pending::GotoMarkLine;
                return true;
            }
            Key::Char('`') => {
                ed.vim.pending = Pending::GotoMarkChar;
                return true;
            }
            _ => {}
        }
    }

    // Unknown key — swallow so it doesn't bubble into the TUI layer.
    true
}

fn handle_set_mark(ed: &mut Editor<'_>, input: Input) -> bool {
    if let Key::Char(c) = input.key
        && c.is_ascii_lowercase()
    {
        ed.vim.marks.insert(c, ed.textarea.cursor());
    }
    true
}

fn handle_goto_mark(ed: &mut Editor<'_>, input: Input, linewise: bool) -> bool {
    let Key::Char(c) = input.key else {
        return true;
    };
    if !c.is_ascii_lowercase() {
        return true;
    }
    let Some(&(row, col)) = ed.vim.marks.get(&c) else {
        return true;
    };
    let pre = ed.textarea.cursor();
    let (r, c_clamped) = clamp_pos(ed, (row, col));
    if linewise {
        ed.textarea.move_cursor(CursorMove::Jump(r, 0));
        move_first_non_whitespace(ed);
    } else {
        ed.textarea.move_cursor(CursorMove::Jump(r, c_clamped));
    }
    if ed.textarea.cursor() != pre {
        push_jump(ed, pre);
    }
    ed.vim.sticky_col = Some(ed.textarea.cursor().1);
    true
}

fn take_count(vim: &mut VimState) -> usize {
    if vim.count > 0 {
        let n = vim.count;
        vim.count = 0;
        n
    } else {
        1
    }
}

fn char_to_operator(c: char) -> Option<Operator> {
    match c {
        'd' => Some(Operator::Delete),
        'c' => Some(Operator::Change),
        'y' => Some(Operator::Yank),
        '>' => Some(Operator::Indent),
        '<' => Some(Operator::Outdent),
        _ => None,
    }
}

fn visual_operator(input: &Input) -> Option<Operator> {
    if input.ctrl {
        return None;
    }
    match input.key {
        Key::Char('y') => Some(Operator::Yank),
        Key::Char('d') | Key::Char('x') => Some(Operator::Delete),
        Key::Char('c') | Key::Char('s') => Some(Operator::Change),
        // Case operators — shift forms apply to the active selection.
        Key::Char('U') => Some(Operator::Uppercase),
        Key::Char('u') => Some(Operator::Lowercase),
        Key::Char('~') => Some(Operator::ToggleCase),
        // Indent operators on selection.
        Key::Char('>') => Some(Operator::Indent),
        Key::Char('<') => Some(Operator::Outdent),
        _ => None,
    }
}

fn find_entry(input: &Input) -> Option<(bool, bool)> {
    if input.ctrl {
        return None;
    }
    match input.key {
        Key::Char('f') => Some((true, false)),
        Key::Char('F') => Some((false, false)),
        Key::Char('t') => Some((true, true)),
        Key::Char('T') => Some((false, true)),
        _ => None,
    }
}

// ─── Jumplist (Ctrl-o / Ctrl-i) ────────────────────────────────────────────

/// Max jumplist depth. Matches vim default.
const JUMPLIST_MAX: usize = 100;

/// Record a pre-jump cursor position. Called *before* a big-jump
/// motion runs (`gg`/`G`, `%`, `*`/`#`, `n`/`N`, `H`/`M`/`L`, `/`?
/// commit, `:{nr}`). Making a new jump while the forward stack had
/// entries trims them — branching off the history clears the "redo".
fn push_jump(ed: &mut Editor<'_>, from: (usize, usize)) {
    ed.vim.jump_back.push(from);
    if ed.vim.jump_back.len() > JUMPLIST_MAX {
        ed.vim.jump_back.remove(0);
    }
    ed.vim.jump_fwd.clear();
}

/// `Ctrl-o` — jump back to the most recent pre-jump position. Saves
/// the current cursor onto the forward stack so `Ctrl-i` can return.
fn jump_back(ed: &mut Editor<'_>) {
    let Some(target) = ed.vim.jump_back.pop() else {
        return;
    };
    let cur = ed.textarea.cursor();
    ed.vim.jump_fwd.push(cur);
    let (r, c) = clamp_pos(ed, target);
    ed.textarea.move_cursor(CursorMove::Jump(r, c));
    ed.vim.sticky_col = Some(c);
}

/// `Ctrl-i` / `Tab` — redo the last `Ctrl-o`. Saves the current cursor
/// onto the back stack.
fn jump_forward(ed: &mut Editor<'_>) {
    let Some(target) = ed.vim.jump_fwd.pop() else {
        return;
    };
    let cur = ed.textarea.cursor();
    ed.vim.jump_back.push(cur);
    if ed.vim.jump_back.len() > JUMPLIST_MAX {
        ed.vim.jump_back.remove(0);
    }
    let (r, c) = clamp_pos(ed, target);
    ed.textarea.move_cursor(CursorMove::Jump(r, c));
    ed.vim.sticky_col = Some(c);
}

/// Clamp a stored `(row, col)` to the live buffer in case edits
/// shrunk the document between push and pop.
fn clamp_pos(ed: &Editor<'_>, pos: (usize, usize)) -> (usize, usize) {
    let last_row = ed.textarea.lines().len().saturating_sub(1);
    let r = pos.0.min(last_row);
    let line_len = ed
        .textarea
        .lines()
        .get(r)
        .map(|l| l.chars().count())
        .unwrap_or(0);
    let c = pos.1.min(line_len.saturating_sub(1));
    (r, c)
}

/// True for motions that vim treats as jumps (pushed onto the jumplist).
fn is_big_jump(motion: &Motion) -> bool {
    matches!(
        motion,
        Motion::FileTop
            | Motion::FileBottom
            | Motion::MatchBracket
            | Motion::WordAtCursor { .. }
            | Motion::SearchNext { .. }
            | Motion::ViewportTop
            | Motion::ViewportMiddle
            | Motion::ViewportBottom
    )
}

// ─── Scroll helpers (Ctrl-d / Ctrl-u / Ctrl-f / Ctrl-b) ────────────────────

/// Half-viewport row count, with a floor of 1 so tiny / un-rendered
/// viewports still step by a single row. `count` multiplies.
fn viewport_half_rows(ed: &Editor<'_>, count: usize) -> usize {
    let h = ed.viewport_height_value() as usize;
    (h / 2).max(1).saturating_mul(count.max(1))
}

/// Full-viewport row count. Vim conventionally keeps 2 lines of overlap
/// between successive `Ctrl-f` pages; we approximate with `h - 2`.
fn viewport_full_rows(ed: &Editor<'_>, count: usize) -> usize {
    let h = ed.viewport_height_value() as usize;
    h.saturating_sub(2).max(1).saturating_mul(count.max(1))
}

/// Move the cursor by `delta` rows (positive = down, negative = up),
/// clamp to the document, then land at the first non-blank on the new
/// row. The textarea viewport auto-scrolls to keep the cursor visible
/// when the cursor pushes off-screen.
fn scroll_cursor_rows(ed: &mut Editor<'_>, delta: isize) {
    if delta == 0 {
        return;
    }
    ed.sync_buffer_content_from_textarea();
    let (row, _) = ed.cursor();
    let last_row = ed.buffer().row_count().saturating_sub(1);
    let target = (row as isize + delta).max(0).min(last_row as isize) as usize;
    ed.buffer_mut()
        .set_cursor(sqeel_buffer::Position::new(target, 0));
    ed.buffer_mut().move_first_non_blank();
    ed.push_buffer_cursor_to_textarea();
    ed.vim.sticky_col = Some(ed.buffer().cursor().col);
}

// ─── Motion parsing ────────────────────────────────────────────────────────

fn parse_motion(input: &Input) -> Option<Motion> {
    if input.ctrl {
        return None;
    }
    match input.key {
        Key::Char('h') | Key::Backspace | Key::Left => Some(Motion::Left),
        Key::Char('l') | Key::Right => Some(Motion::Right),
        Key::Char('j') | Key::Down | Key::Enter => Some(Motion::Down),
        Key::Char('k') | Key::Up => Some(Motion::Up),
        Key::Char('w') => Some(Motion::WordFwd),
        Key::Char('W') => Some(Motion::BigWordFwd),
        Key::Char('b') => Some(Motion::WordBack),
        Key::Char('B') => Some(Motion::BigWordBack),
        Key::Char('e') => Some(Motion::WordEnd),
        Key::Char('E') => Some(Motion::BigWordEnd),
        Key::Char('0') | Key::Home => Some(Motion::LineStart),
        Key::Char('^') => Some(Motion::FirstNonBlank),
        Key::Char('$') | Key::End => Some(Motion::LineEnd),
        Key::Char('G') => Some(Motion::FileBottom),
        Key::Char('%') => Some(Motion::MatchBracket),
        Key::Char(';') => Some(Motion::FindRepeat { reverse: false }),
        Key::Char(',') => Some(Motion::FindRepeat { reverse: true }),
        Key::Char('*') => Some(Motion::WordAtCursor { forward: true }),
        Key::Char('#') => Some(Motion::WordAtCursor { forward: false }),
        Key::Char('n') => Some(Motion::SearchNext { reverse: false }),
        Key::Char('N') => Some(Motion::SearchNext { reverse: true }),
        Key::Char('H') => Some(Motion::ViewportTop),
        Key::Char('M') => Some(Motion::ViewportMiddle),
        Key::Char('L') => Some(Motion::ViewportBottom),
        _ => None,
    }
}

// ─── Motion execution ──────────────────────────────────────────────────────

fn execute_motion(ed: &mut Editor<'_>, motion: Motion, count: usize) {
    let count = count.max(1);
    // FindRepeat needs the stored direction.
    let motion = match motion {
        Motion::FindRepeat { reverse } => match ed.vim.last_find {
            Some((ch, forward, till)) => Motion::Find {
                ch,
                forward: if reverse { !forward } else { forward },
                till,
            },
            None => return,
        },
        other => other,
    };
    let pre_pos = ed.textarea.cursor();
    let pre_col = pre_pos.1;
    apply_motion_cursor(ed, &motion, count);
    let post_pos = ed.textarea.cursor();
    if is_big_jump(&motion) && pre_pos != post_pos {
        push_jump(ed, pre_pos);
    }
    apply_sticky_col(ed, &motion, pre_col);
    // Phase 7b: keep the migration buffer's cursor + viewport in
    // lockstep with the textarea after every motion. Once 7c lands
    // (motions ported onto the buffer's API), this flips: the
    // buffer becomes authoritative and the textarea mirrors it.
    ed.sync_buffer_from_textarea();
}

/// Restore the cursor to the sticky column after vertical motions and
/// sync the sticky column to the current column after horizontal ones.
/// `pre_col` is the cursor column captured *before* the motion — used
/// to bootstrap the sticky value on the very first motion.
fn apply_sticky_col(ed: &mut Editor<'_>, motion: &Motion, pre_col: usize) {
    if is_vertical_motion(motion) {
        let want = ed.vim.sticky_col.unwrap_or(pre_col);
        // Record the desired column so the next vertical motion sees
        // it even if we currently clamped to a shorter row.
        ed.vim.sticky_col = Some(want);
        let (row, _) = ed.textarea.cursor();
        let line_len = ed.textarea.lines()[row].chars().count();
        // Clamp to the last char on non-empty lines (vim normal-mode
        // never parks the cursor one past end of line). Empty lines
        // collapse to col 0.
        let max_col = line_len.saturating_sub(1);
        let target = want.min(max_col);
        ed.textarea.move_cursor(CursorMove::Jump(row, target));
    } else {
        // Horizontal motion or non-motion: sticky column tracks the
        // new cursor column so the *next* vertical motion aims there.
        ed.vim.sticky_col = Some(ed.textarea.cursor().1);
    }
}

fn is_vertical_motion(motion: &Motion) -> bool {
    // Only j / k preserve the sticky column. Everything else (search,
    // gg / G, word jumps, etc.) lands at the match's own column so the
    // sticky value should sync to the new cursor column.
    matches!(motion, Motion::Up | Motion::Down)
}

fn apply_motion_cursor(ed: &mut Editor<'_>, motion: &Motion, count: usize) {
    apply_motion_cursor_ctx(ed, motion, count, false)
}

fn apply_motion_cursor_ctx(ed: &mut Editor<'_>, motion: &Motion, count: usize, as_operator: bool) {
    match motion {
        Motion::Left => {
            // `h` — Buffer clamps at col 0 (no wrap), matching vim.
            ed.buffer_mut().move_left(count);
            ed.push_buffer_cursor_to_textarea();
        }
        Motion::Right => {
            // `l` — operator-motion context (`dl`/`cl`/`yl`) is allowed
            // one past the last char so the range includes it; cursor
            // context clamps at the last char.
            if as_operator {
                ed.buffer_mut().move_right_to_end(count);
            } else {
                ed.buffer_mut().move_right_in_line(count);
            }
            ed.push_buffer_cursor_to_textarea();
        }
        Motion::Up => {
            // Final col is set by `apply_sticky_col` below — push the
            // post-move row to the textarea and let sticky tracking
            // finish the work.
            ed.buffer_mut().move_up(count);
            ed.push_buffer_cursor_to_textarea();
        }
        Motion::Down => {
            ed.buffer_mut().move_down(count);
            ed.push_buffer_cursor_to_textarea();
        }
        Motion::WordFwd => {
            ed.buffer_mut().move_word_fwd(false, count);
            ed.push_buffer_cursor_to_textarea();
        }
        Motion::WordBack => {
            ed.buffer_mut().move_word_back(false, count);
            ed.push_buffer_cursor_to_textarea();
        }
        Motion::WordEnd => {
            ed.buffer_mut().move_word_end(false, count);
            ed.push_buffer_cursor_to_textarea();
        }
        Motion::BigWordFwd => {
            ed.buffer_mut().move_word_fwd(true, count);
            ed.push_buffer_cursor_to_textarea();
        }
        Motion::BigWordBack => {
            ed.buffer_mut().move_word_back(true, count);
            ed.push_buffer_cursor_to_textarea();
        }
        Motion::BigWordEnd => {
            ed.buffer_mut().move_word_end(true, count);
            ed.push_buffer_cursor_to_textarea();
        }
        Motion::WordEndBack => {
            ed.buffer_mut().move_word_end_back(false, count);
            ed.push_buffer_cursor_to_textarea();
        }
        Motion::BigWordEndBack => {
            ed.buffer_mut().move_word_end_back(true, count);
            ed.push_buffer_cursor_to_textarea();
        }
        Motion::LineStart => {
            ed.buffer_mut().move_line_start();
            ed.push_buffer_cursor_to_textarea();
        }
        Motion::FirstNonBlank => {
            ed.buffer_mut().move_first_non_blank();
            ed.push_buffer_cursor_to_textarea();
        }
        Motion::LineEnd => {
            // Vim normal-mode `$` lands on the last char, not one past it.
            ed.buffer_mut().move_line_end();
            ed.push_buffer_cursor_to_textarea();
        }
        Motion::FileTop => {
            // `count gg` jumps to line `count` (first non-blank);
            // bare `gg` lands at the top.
            if count > 1 {
                ed.buffer_mut().move_bottom(count);
            } else {
                ed.buffer_mut().move_top();
            }
            ed.push_buffer_cursor_to_textarea();
        }
        Motion::FileBottom => {
            // `count G` jumps to line `count`; bare `G` lands at
            // the buffer bottom (`Buffer::move_bottom(0)`).
            if count > 1 {
                ed.buffer_mut().move_bottom(count);
            } else {
                ed.buffer_mut().move_bottom(0);
            }
            ed.push_buffer_cursor_to_textarea();
        }
        Motion::Find { ch, forward, till } => {
            for _ in 0..count {
                if !find_char_on_line(ed, *ch, *forward, *till) {
                    break;
                }
            }
        }
        Motion::FindRepeat { .. } => {} // already resolved upstream
        Motion::MatchBracket => {
            let _ = matching_bracket(ed);
        }
        Motion::WordAtCursor { forward } => {
            word_at_cursor_search(ed, *forward, count);
        }
        Motion::SearchNext { reverse } => {
            if ed.textarea.search_pattern().is_none() {
                return;
            }
            for _ in 0..count.max(1) {
                if *reverse {
                    let _ = ed.textarea.search_back(false);
                } else {
                    let _ = ed.textarea.search_forward(false);
                }
            }
        }
        Motion::ViewportTop => {
            ed.buffer_mut().move_viewport_top(count.saturating_sub(1));
            ed.push_buffer_cursor_to_textarea();
        }
        Motion::ViewportMiddle => {
            ed.buffer_mut().move_viewport_middle();
            ed.push_buffer_cursor_to_textarea();
        }
        Motion::ViewportBottom => {
            ed.buffer_mut()
                .move_viewport_bottom(count.saturating_sub(1));
            ed.push_buffer_cursor_to_textarea();
        }
    }
}

fn move_first_non_whitespace(ed: &mut Editor<'_>) {
    // Some call sites invoke this right after `dd` / `<<` / `>>` etc
    // mutates the textarea content, so the migration buffer hasn't
    // seen the new lines OR new cursor yet. Mirror the full content
    // across before delegating, then push the result back so the
    // textarea reflects the resolved column too.
    ed.sync_buffer_content_from_textarea();
    ed.buffer_mut().move_first_non_blank();
    ed.push_buffer_cursor_to_textarea();
}

fn find_char_on_line(ed: &mut Editor<'_>, ch: char, forward: bool, till: bool) -> bool {
    let moved = ed.buffer_mut().find_char_on_line(ch, forward, till);
    if moved {
        ed.push_buffer_cursor_to_textarea();
    }
    moved
}

fn matching_bracket(ed: &mut Editor<'_>) -> bool {
    let moved = ed.buffer_mut().match_bracket();
    if moved {
        ed.push_buffer_cursor_to_textarea();
    }
    moved
}

fn word_at_cursor_search(ed: &mut Editor<'_>, forward: bool, count: usize) {
    let (row, col) = ed.textarea.cursor();
    let line = &ed.textarea.lines()[row];
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return;
    }
    // Expand around cursor to a word boundary.
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut start = col.min(chars.len().saturating_sub(1));
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    let mut end = start;
    while end < chars.len() && is_word(chars[end]) {
        end += 1;
    }
    if end <= start {
        return;
    }
    let word: String = chars[start..end].iter().collect();
    let pattern = format!(r"\b{}\b", regex_escape(&word));
    if ed.textarea.set_search_pattern(&pattern).is_err() {
        return;
    }
    for _ in 0..count.max(1) {
        if forward {
            let _ = ed.textarea.search_forward(false);
        } else {
            let _ = ed.textarea.search_back(false);
        }
    }
}

fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// ─── Operator application ──────────────────────────────────────────────────

fn handle_after_op(ed: &mut Editor<'_>, input: Input, op: Operator, count1: usize) -> bool {
    // Inner count after operator (e.g. d3w): accumulate in state.count.
    if let Key::Char(d @ '0'..='9') = input.key
        && !input.ctrl
        && (d != '0' || ed.vim.count > 0)
    {
        ed.vim.count = ed.vim.count.saturating_mul(10) + (d as usize - '0' as usize);
        ed.vim.pending = Pending::Op { op, count1 };
        return true;
    }

    // Esc cancels.
    if input.key == Key::Esc {
        ed.vim.count = 0;
        return true;
    }

    // Same-letter: dd / cc / yy / gUU / guu / g~~ / >> / <<.
    let double_ch = match op {
        Operator::Delete => 'd',
        Operator::Change => 'c',
        Operator::Yank => 'y',
        Operator::Indent => '>',
        Operator::Outdent => '<',
        Operator::Uppercase => 'U',
        Operator::Lowercase => 'u',
        Operator::ToggleCase => '~',
    };
    if let Key::Char(c) = input.key
        && !input.ctrl
        && c == double_ch
    {
        let count2 = take_count(&mut ed.vim);
        let total = count1.max(1) * count2.max(1);
        execute_line_op(ed, op, total);
        if !ed.vim.replaying {
            ed.vim.last_change = Some(LastChange::LineOp {
                op,
                count: total,
                inserted: None,
            });
        }
        return true;
    }

    // Text object: `i` or `a`.
    if let Key::Char('i') | Key::Char('a') = input.key
        && !input.ctrl
    {
        let inner = matches!(input.key, Key::Char('i'));
        ed.vim.pending = Pending::OpTextObj { op, count1, inner };
        return true;
    }

    // `g` — awaiting `g` for `gg`.
    if input.key == Key::Char('g') && !input.ctrl {
        ed.vim.pending = Pending::OpG { op, count1 };
        return true;
    }

    // `f`/`F`/`t`/`T` with pending target.
    if let Some((forward, till)) = find_entry(&input) {
        ed.vim.pending = Pending::OpFind {
            op,
            count1,
            forward,
            till,
        };
        return true;
    }

    // Motion.
    let count2 = take_count(&mut ed.vim);
    let total = count1.max(1) * count2.max(1);
    if let Some(motion) = parse_motion(&input) {
        let motion = match motion {
            Motion::FindRepeat { reverse } => match ed.vim.last_find {
                Some((ch, forward, till)) => Motion::Find {
                    ch,
                    forward: if reverse { !forward } else { forward },
                    till,
                },
                None => return true,
            },
            // Vim quirk: `cw` / `cW` are `ce` / `cE` — don't include
            // trailing whitespace so the user's replacement text lands
            // before the following word's leading space.
            Motion::WordFwd if op == Operator::Change => Motion::WordEnd,
            Motion::BigWordFwd if op == Operator::Change => Motion::BigWordEnd,
            m => m,
        };
        apply_op_with_motion(ed, op, &motion, total);
        if let Motion::Find { ch, forward, till } = &motion {
            ed.vim.last_find = Some((*ch, *forward, *till));
        }
        if !ed.vim.replaying && op_is_change(op) {
            ed.vim.last_change = Some(LastChange::OpMotion {
                op,
                motion,
                count: total,
                inserted: None,
            });
        }
        return true;
    }

    // Unknown — cancel the operator.
    true
}

fn handle_op_after_g(ed: &mut Editor<'_>, input: Input, op: Operator, count1: usize) -> bool {
    if input.ctrl {
        return true;
    }
    let count2 = take_count(&mut ed.vim);
    let total = count1.max(1) * count2.max(1);
    // Case-op linewise form: `gUgU`, `gugu`, `g~g~` — same effect as
    // `gUU` / `guu` / `g~~`. The leading `g` was consumed into
    // `Pending::OpG`, so here we see the trailing U / u / ~.
    if matches!(
        op,
        Operator::Uppercase | Operator::Lowercase | Operator::ToggleCase
    ) {
        let op_char = match op {
            Operator::Uppercase => 'U',
            Operator::Lowercase => 'u',
            Operator::ToggleCase => '~',
            _ => unreachable!(),
        };
        if input.key == Key::Char(op_char) {
            execute_line_op(ed, op, total);
            if !ed.vim.replaying {
                ed.vim.last_change = Some(LastChange::LineOp {
                    op,
                    count: total,
                    inserted: None,
                });
            }
            return true;
        }
    }
    let motion = match input.key {
        Key::Char('g') => Motion::FileTop,
        Key::Char('e') => Motion::WordEndBack,
        Key::Char('E') => Motion::BigWordEndBack,
        _ => return true,
    };
    apply_op_with_motion(ed, op, &motion, total);
    if !ed.vim.replaying && op_is_change(op) {
        ed.vim.last_change = Some(LastChange::OpMotion {
            op,
            motion,
            count: total,
            inserted: None,
        });
    }
    true
}

fn handle_after_g(ed: &mut Editor<'_>, input: Input) -> bool {
    let count = take_count(&mut ed.vim);
    match input.key {
        Key::Char('g') => {
            // gg — top / jump to line count.
            let pre = ed.textarea.cursor();
            if count > 1 {
                ed.textarea.move_cursor(CursorMove::Jump(count - 1, 0));
            } else {
                ed.textarea.move_cursor(CursorMove::Top);
            }
            move_first_non_whitespace(ed);
            if ed.textarea.cursor() != pre {
                push_jump(ed, pre);
            }
        }
        Key::Char('e') => execute_motion(ed, Motion::WordEndBack, count),
        Key::Char('E') => execute_motion(ed, Motion::BigWordEndBack, count),
        // Case operators: `gU` / `gu` / `g~`. Enter operator-pending
        // so the next input is treated as the motion / text object /
        // shorthand double (`gUU`, `guu`, `g~~`).
        Key::Char('U') => {
            ed.vim.pending = Pending::Op {
                op: Operator::Uppercase,
                count1: count,
            };
        }
        Key::Char('u') => {
            ed.vim.pending = Pending::Op {
                op: Operator::Lowercase,
                count1: count,
            };
        }
        Key::Char('~') => {
            ed.vim.pending = Pending::Op {
                op: Operator::ToggleCase,
                count1: count,
            };
        }
        Key::Char('J') => {
            // `gJ` — join line below without inserting a space.
            for _ in 0..count.max(1) {
                ed.push_undo();
                join_line_raw(ed);
            }
            if !ed.vim.replaying {
                ed.vim.last_change = Some(LastChange::JoinLine {
                    count: count.max(1),
                });
            }
        }
        Key::Char('d') => {
            // `gd` — goto definition. sqeel-vim doesn't run an LSP
            // itself; raise an intent the host drains and routes to
            // `sqls`. The cursor stays put here — the host moves it
            // once it has the target location.
            ed.pending_lsp = Some(crate::editor::LspIntent::GotoDefinition);
        }
        _ => {}
    }
    true
}

fn handle_after_z(ed: &mut Editor<'_>, input: Input) -> bool {
    use crate::editor::CursorScrollTarget;
    match input.key {
        Key::Char('z') => ed.scroll_cursor_to(CursorScrollTarget::Center),
        Key::Char('t') => ed.scroll_cursor_to(CursorScrollTarget::Top),
        Key::Char('b') => ed.scroll_cursor_to(CursorScrollTarget::Bottom),
        _ => {}
    }
    true
}

fn handle_replace(ed: &mut Editor<'_>, input: Input) -> bool {
    if let Key::Char(ch) = input.key {
        if ed.vim.mode == Mode::VisualBlock {
            block_replace(ed, ch);
            return true;
        }
        let count = take_count(&mut ed.vim);
        replace_char(ed, ch, count.max(1));
        if !ed.vim.replaying {
            ed.vim.last_change = Some(LastChange::ReplaceChar {
                ch,
                count: count.max(1),
            });
        }
    }
    true
}

fn handle_find_target(ed: &mut Editor<'_>, input: Input, forward: bool, till: bool) -> bool {
    let Key::Char(ch) = input.key else {
        return true;
    };
    let count = take_count(&mut ed.vim);
    execute_motion(ed, Motion::Find { ch, forward, till }, count.max(1));
    ed.vim.last_find = Some((ch, forward, till));
    true
}

fn handle_op_find_target(
    ed: &mut Editor<'_>,
    input: Input,
    op: Operator,
    count1: usize,
    forward: bool,
    till: bool,
) -> bool {
    let Key::Char(ch) = input.key else {
        return true;
    };
    let count2 = take_count(&mut ed.vim);
    let total = count1.max(1) * count2.max(1);
    let motion = Motion::Find { ch, forward, till };
    apply_op_with_motion(ed, op, &motion, total);
    ed.vim.last_find = Some((ch, forward, till));
    if !ed.vim.replaying && op_is_change(op) {
        ed.vim.last_change = Some(LastChange::OpMotion {
            op,
            motion,
            count: total,
            inserted: None,
        });
    }
    true
}

fn handle_text_object(
    ed: &mut Editor<'_>,
    input: Input,
    op: Operator,
    _count1: usize,
    inner: bool,
) -> bool {
    let Key::Char(ch) = input.key else {
        return true;
    };
    let obj = match ch {
        'w' => TextObject::Word { big: false },
        'W' => TextObject::Word { big: true },
        '"' | '\'' | '`' => TextObject::Quote(ch),
        '(' | ')' | 'b' => TextObject::Bracket('('),
        '[' | ']' => TextObject::Bracket('['),
        '{' | '}' | 'B' => TextObject::Bracket('{'),
        '<' | '>' => TextObject::Bracket('<'),
        'p' => TextObject::Paragraph,
        _ => return true,
    };
    apply_op_with_text_object(ed, op, obj, inner);
    if !ed.vim.replaying && op_is_change(op) {
        ed.vim.last_change = Some(LastChange::OpTextObj {
            op,
            obj,
            inner,
            inserted: None,
        });
    }
    true
}

fn handle_visual_text_obj(ed: &mut Editor<'_>, input: Input, inner: bool) -> bool {
    let Key::Char(ch) = input.key else {
        return true;
    };
    let obj = match ch {
        'w' => TextObject::Word { big: false },
        'W' => TextObject::Word { big: true },
        '"' | '\'' | '`' => TextObject::Quote(ch),
        '(' | ')' | 'b' => TextObject::Bracket('('),
        '[' | ']' => TextObject::Bracket('['),
        '{' | '}' | 'B' => TextObject::Bracket('{'),
        '<' | '>' => TextObject::Bracket('<'),
        'p' => TextObject::Paragraph,
        _ => return true,
    };
    let Some((start, end, kind)) = text_object_range(ed, obj, inner) else {
        return true;
    };
    // Anchor + cursor position the char-wise highlight / operator range;
    // for linewise text-objects we switch into VisualLine with the
    // appropriate row anchor.
    ed.textarea.cancel_selection();
    match kind {
        MotionKind::Linewise => {
            ed.vim.visual_line_anchor = start.0;
            ed.vim.mode = Mode::VisualLine;
            ed.textarea.move_cursor(CursorMove::Jump(end.0, 0));
            refresh_visual_line_selection(ed);
        }
        _ => {
            ed.vim.mode = Mode::Visual;
            ed.vim.visual_anchor = (start.0, start.1);
            let (er, ec) = retreat_one(ed, end);
            ed.textarea.move_cursor(CursorMove::Jump(er, ec));
        }
    }
    true
}

/// Move `pos` back by one character, clamped to (0, 0).
fn retreat_one(ed: &Editor<'_>, pos: (usize, usize)) -> (usize, usize) {
    let (r, c) = pos;
    if c > 0 {
        (r, c - 1)
    } else if r > 0 {
        let prev_len = ed.textarea.lines()[r - 1].len();
        (r - 1, prev_len)
    } else {
        (0, 0)
    }
}

fn op_is_change(op: Operator) -> bool {
    matches!(op, Operator::Delete | Operator::Change)
}

// ─── Normal-only commands (not motion, not operator) ───────────────────────

fn handle_normal_only(ed: &mut Editor<'_>, input: &Input, count: usize) -> bool {
    if input.ctrl {
        return false;
    }
    match input.key {
        Key::Char('i') => {
            ed.textarea.cancel_selection();
            begin_insert(ed, count.max(1), InsertReason::Enter(InsertEntry::I));
            true
        }
        Key::Char('I') => {
            ed.textarea.cancel_selection();
            move_first_non_whitespace(ed);
            begin_insert(ed, count.max(1), InsertReason::Enter(InsertEntry::ShiftI));
            true
        }
        Key::Char('a') => {
            ed.textarea.cancel_selection();
            ed.textarea.move_cursor(CursorMove::Forward);
            begin_insert(ed, count.max(1), InsertReason::Enter(InsertEntry::A));
            true
        }
        Key::Char('A') => {
            ed.textarea.cancel_selection();
            ed.textarea.move_cursor(CursorMove::End);
            begin_insert(ed, count.max(1), InsertReason::Enter(InsertEntry::ShiftA));
            true
        }
        Key::Char('o') => {
            ed.push_undo();
            // Snapshot BEFORE the newline so replay sees "\n<text>" as the
            // delta and produces one fresh line per iteration.
            begin_insert_noundo(ed, count.max(1), InsertReason::Open { above: false });
            ed.textarea.move_cursor(CursorMove::End);
            ed.mutate(|t| t.insert_newline());
            true
        }
        Key::Char('O') => {
            ed.push_undo();
            begin_insert_noundo(ed, count.max(1), InsertReason::Open { above: true });
            ed.textarea.move_cursor(CursorMove::Head);
            ed.mutate(|t| t.insert_newline());
            ed.textarea.move_cursor(CursorMove::Up);
            true
        }
        Key::Char('x') => {
            do_char_delete(ed, true, count.max(1));
            if !ed.vim.replaying {
                ed.vim.last_change = Some(LastChange::CharDel {
                    forward: true,
                    count: count.max(1),
                });
            }
            true
        }
        Key::Char('X') => {
            do_char_delete(ed, false, count.max(1));
            if !ed.vim.replaying {
                ed.vim.last_change = Some(LastChange::CharDel {
                    forward: false,
                    count: count.max(1),
                });
            }
            true
        }
        Key::Char('~') => {
            for _ in 0..count.max(1) {
                ed.push_undo();
                toggle_case_at_cursor(ed);
            }
            if !ed.vim.replaying {
                ed.vim.last_change = Some(LastChange::ToggleCase {
                    count: count.max(1),
                });
            }
            true
        }
        Key::Char('J') => {
            for _ in 0..count.max(1) {
                ed.push_undo();
                join_line(ed);
            }
            if !ed.vim.replaying {
                ed.vim.last_change = Some(LastChange::JoinLine {
                    count: count.max(1),
                });
            }
            true
        }
        Key::Char('D') => {
            ed.push_undo();
            ed.mutate(|t| t.delete_line_by_end());
            let y = ed.textarea.yank_text();
            if !y.is_empty() {
                ed.last_yank = Some(y);
                ed.vim.yank_linewise = false;
            }
            if !ed.vim.replaying {
                ed.vim.last_change = Some(LastChange::DeleteToEol { inserted: None });
            }
            true
        }
        Key::Char('Y') => {
            // Vim 8 default: `Y` yanks to end of line (same as `y$`).
            apply_op_with_motion(ed, Operator::Yank, &Motion::LineEnd, count.max(1));
            true
        }
        Key::Char('C') => {
            ed.push_undo();
            ed.mutate(|t| t.delete_line_by_end());
            let y = ed.textarea.yank_text();
            if !y.is_empty() {
                ed.last_yank = Some(y);
                ed.vim.yank_linewise = false;
            }
            begin_insert_noundo(ed, 1, InsertReason::DeleteToEol);
            true
        }
        Key::Char('s') => {
            ed.push_undo();
            for _ in 0..count.max(1) {
                ed.mutate(|t| t.delete_next_char());
            }
            begin_insert_noundo(ed, 1, InsertReason::AfterChange);
            // `s` == `cl` — record as such.
            if !ed.vim.replaying {
                ed.vim.last_change = Some(LastChange::OpMotion {
                    op: Operator::Change,
                    motion: Motion::Right,
                    count: count.max(1),
                    inserted: None,
                });
            }
            true
        }
        Key::Char('p') => {
            do_paste(ed, false, count.max(1));
            if !ed.vim.replaying {
                ed.vim.last_change = Some(LastChange::Paste {
                    before: false,
                    count: count.max(1),
                });
            }
            true
        }
        Key::Char('P') => {
            do_paste(ed, true, count.max(1));
            if !ed.vim.replaying {
                ed.vim.last_change = Some(LastChange::Paste {
                    before: true,
                    count: count.max(1),
                });
            }
            true
        }
        Key::Char('u') => {
            do_undo(ed);
            true
        }
        Key::Char('r') => {
            ed.vim.count = count;
            ed.vim.pending = Pending::Replace;
            true
        }
        Key::Char('/') => {
            enter_search(ed, true);
            true
        }
        Key::Char('?') => {
            enter_search(ed, false);
            true
        }
        Key::Char('.') => {
            replay_last_change(ed, count);
            true
        }
        _ => false,
    }
}

/// Variant of begin_insert that doesn't push_undo (caller already did).
fn begin_insert_noundo(ed: &mut Editor<'_>, count: usize, reason: InsertReason) {
    let reason = if ed.vim.replaying {
        InsertReason::ReplayOnly
    } else {
        reason
    };
    let (row, _) = ed.textarea.cursor();
    ed.vim.insert_session = Some(InsertSession {
        count,
        row_min: row,
        row_max: row,
        before_lines: ed.textarea.lines().to_vec(),
        reason,
    });
    ed.vim.mode = Mode::Insert;
}

// ─── Operator × Motion application ─────────────────────────────────────────

fn apply_op_with_motion(ed: &mut Editor<'_>, op: Operator, motion: &Motion, count: usize) {
    let start = ed.textarea.cursor();
    // Tentatively apply motion to find the endpoint. Operator context
    // so `l` on the last char advances past-last (standard vim
    // exclusive-motion endpoint behaviour), enabling `dl` / `cl` /
    // `yl` to cover the final char.
    apply_motion_cursor_ctx(ed, motion, count, true);
    let end = ed.textarea.cursor();
    let kind = motion_kind(motion);
    // Restore cursor before selecting (so Yank leaves cursor at start).
    ed.textarea.move_cursor(CursorMove::Jump(start.0, start.1));
    run_operator_over_range(ed, op, start, end, kind);
}

fn apply_op_with_text_object(ed: &mut Editor<'_>, op: Operator, obj: TextObject, inner: bool) {
    let Some((start, end, kind)) = text_object_range(ed, obj, inner) else {
        return;
    };
    ed.textarea.move_cursor(CursorMove::Jump(start.0, start.1));
    run_operator_over_range(ed, op, start, end, kind);
}

fn motion_kind(motion: &Motion) -> MotionKind {
    match motion {
        Motion::Up | Motion::Down => MotionKind::Linewise,
        Motion::FileTop | Motion::FileBottom => MotionKind::Linewise,
        Motion::ViewportTop | Motion::ViewportMiddle | Motion::ViewportBottom => {
            MotionKind::Linewise
        }
        Motion::WordEnd | Motion::BigWordEnd | Motion::WordEndBack | Motion::BigWordEndBack => {
            MotionKind::Inclusive
        }
        Motion::Find { .. } => MotionKind::Inclusive,
        Motion::MatchBracket => MotionKind::Inclusive,
        // `$` now lands on the last char — operator ranges include it.
        Motion::LineEnd => MotionKind::Inclusive,
        _ => MotionKind::Exclusive,
    }
}

fn run_operator_over_range(
    ed: &mut Editor<'_>,
    op: Operator,
    start: (usize, usize),
    end: (usize, usize),
    kind: MotionKind,
) {
    let (top, bot) = order(start, end);
    if top == bot {
        return;
    }

    match kind {
        MotionKind::Linewise => {
            select_full_lines(ed, top.0, bot.0);
            ed.vim.yank_linewise = true;
        }
        MotionKind::Inclusive => {
            ed.textarea.move_cursor(CursorMove::Jump(top.0, top.1));
            ed.textarea.start_selection();
            ed.textarea.move_cursor(CursorMove::Jump(bot.0, bot.1));
            ed.textarea.move_cursor(CursorMove::Forward);
            ed.vim.yank_linewise = false;
        }
        MotionKind::Exclusive => {
            ed.textarea.move_cursor(CursorMove::Jump(top.0, top.1));
            ed.textarea.start_selection();
            ed.textarea.move_cursor(CursorMove::Jump(bot.0, bot.1));
            ed.vim.yank_linewise = false;
        }
    }

    match op {
        Operator::Yank => {
            let cursor_before = top;
            ed.textarea.copy();
            if let Some(y) = non_empty_yank(ed) {
                ed.last_yank = Some(y);
            }
            ed.textarea.cancel_selection();
            ed.textarea
                .move_cursor(CursorMove::Jump(cursor_before.0, cursor_before.1));
        }
        Operator::Delete => {
            ed.push_undo();
            ed.mutate(|t| t.cut());
            if let Some(y) = non_empty_yank(ed) {
                ed.last_yank = Some(y);
            }
            ed.vim.mode = Mode::Normal;
        }
        Operator::Change => {
            ed.push_undo();
            ed.mutate(|t| t.cut());
            if let Some(y) = non_empty_yank(ed) {
                ed.last_yank = Some(y);
            }
            begin_insert_noundo(ed, 1, InsertReason::AfterChange);
        }
        Operator::Uppercase | Operator::Lowercase | Operator::ToggleCase => {
            apply_case_op_to_selection(ed, op, top);
        }
        Operator::Indent | Operator::Outdent => {
            // Indent / outdent are always linewise even when triggered
            // by a char-wise motion (e.g. `>w` indents the whole line).
            ed.push_undo();
            ed.textarea.cancel_selection();
            if op == Operator::Indent {
                indent_rows(ed, top.0, bot.0, 1);
            } else {
                outdent_rows(ed, top.0, bot.0, 1);
            }
            ed.vim.mode = Mode::Normal;
        }
    }
}

/// Transform the active selection in place with the given case
/// operator. Cursor lands on `top` afterward — vim convention for
/// `gU{motion}` / `gu{motion}` / `g~{motion}`. Preserves the textarea
/// yank buffer (vim's case operators don't touch registers).
fn apply_case_op_to_selection(ed: &mut Editor<'_>, op: Operator, top: (usize, usize)) {
    ed.push_undo();
    let saved_yank = ed.textarea.yank_text().to_string();
    let saved_yank_linewise = ed.vim.yank_linewise;
    // Cut first — tui-textarea's `copy()` consumes the selection,
    // leaving a subsequent `cut()` with nothing to delete. Cutting
    // removes the range AND fills the yank buffer; we read from there
    // for the transform then restore the previous yank so vim's case
    // operators don't clobber registers.
    ed.mutate(|t| t.cut());
    let selection = ed.textarea.yank_text().to_string();
    let transformed = match op {
        Operator::Uppercase => selection.to_uppercase(),
        Operator::Lowercase => selection.to_lowercase(),
        Operator::ToggleCase => toggle_case_str(&selection),
        _ => unreachable!(),
    };
    ed.mutate(|t| t.insert_str(&transformed));
    ed.textarea.cancel_selection();
    ed.textarea.move_cursor(CursorMove::Jump(top.0, top.1));
    ed.textarea.set_yank_text(saved_yank);
    ed.vim.yank_linewise = saved_yank_linewise;
    ed.vim.mode = Mode::Normal;
}

/// Shift-width for indent operators / insert-mode indent helpers.
/// Vim is configurable via `:set shiftwidth`; we hard-code a reasonable
/// SQL default for now. 2 spaces matches the style in the existing
/// test fixtures.
pub(super) const SHIFTWIDTH: usize = 2;

/// Prepend `count * SHIFTWIDTH` spaces to each row in `[top, bot]`.
/// Rows that are empty are skipped (vim leaves blank lines alone when
/// indenting).
fn indent_rows(ed: &mut Editor<'_>, top: usize, bot: usize, count: usize) {
    let width = SHIFTWIDTH * count.max(1);
    let pad: String = " ".repeat(width);
    let mut lines: Vec<String> = ed.textarea.lines().to_vec();
    let bot = bot.min(lines.len().saturating_sub(1));
    for line in lines.iter_mut().take(bot + 1).skip(top) {
        if !line.is_empty() {
            line.insert_str(0, &pad);
        }
    }
    // Restore cursor to first non-blank of the top row so the next
    // vertical motion aims sensibly — matches vim's `>>` convention.
    ed.restore(lines, (top, 0));
    move_first_non_whitespace(ed);
}

/// Remove up to `count * SHIFTWIDTH` leading spaces (or tabs) from
/// each row in `[top, bot]`. Rows with less leading whitespace have
/// all their indent stripped, not clipped to zero length.
fn outdent_rows(ed: &mut Editor<'_>, top: usize, bot: usize, count: usize) {
    let width = SHIFTWIDTH * count.max(1);
    let mut lines: Vec<String> = ed.textarea.lines().to_vec();
    let bot = bot.min(lines.len().saturating_sub(1));
    for line in lines.iter_mut().take(bot + 1).skip(top) {
        let strip: usize = line
            .chars()
            .take(width)
            .take_while(|c| *c == ' ' || *c == '\t')
            .count();
        if strip > 0 {
            let byte_len: usize = line.chars().take(strip).map(|c| c.len_utf8()).sum();
            line.drain(..byte_len);
        }
    }
    ed.restore(lines, (top, 0));
    move_first_non_whitespace(ed);
}

fn toggle_case_str(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_lowercase() {
                c.to_uppercase().next().unwrap_or(c)
            } else if c.is_uppercase() {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                c
            }
        })
        .collect()
}

fn non_empty_yank(ed: &Editor<'_>) -> Option<String> {
    let y = ed.textarea.yank_text();
    if y.is_empty() {
        return None;
    }
    // Line-wise yanks — even on the final line of the buffer — must end
    // with `\n` so pasting into another app lands the text as a full line
    // (matches vim's convention).
    if ed.vim.yank_linewise {
        let trimmed = y.trim_start_matches('\n');
        if trimmed.ends_with('\n') {
            Some(trimmed.to_string())
        } else {
            Some(format!("{trimmed}\n"))
        }
    } else {
        Some(y)
    }
}

fn order(a: (usize, usize), b: (usize, usize)) -> ((usize, usize), (usize, usize)) {
    if a <= b { (a, b) } else { (b, a) }
}

fn select_full_lines(ed: &mut Editor<'_>, top_row: usize, bot_row: usize) {
    let total = ed.textarea.lines().len();
    if top_row > 0 {
        ed.textarea.move_cursor(CursorMove::Jump(top_row - 1, 0));
        ed.textarea.move_cursor(CursorMove::End);
        ed.textarea.start_selection();
        ed.textarea.move_cursor(CursorMove::Jump(bot_row, 0));
        ed.textarea.move_cursor(CursorMove::End);
    } else if total > 1 {
        ed.textarea.move_cursor(CursorMove::Jump(top_row, 0));
        ed.textarea.start_selection();
        if bot_row + 1 < total {
            ed.textarea.move_cursor(CursorMove::Jump(bot_row + 1, 0));
        } else {
            ed.textarea.move_cursor(CursorMove::Jump(bot_row, 0));
            ed.textarea.move_cursor(CursorMove::End);
        }
    } else {
        ed.textarea.move_cursor(CursorMove::Jump(top_row, 0));
        ed.textarea.start_selection();
        ed.textarea.move_cursor(CursorMove::End);
    }
}

/// Select the contents of rows `[top_row..=bot_row]` but stop at the end of
/// `bot_row` without crossing its trailing newline. Used by `cc` / `Vc` so
/// the cut removes line *contents* and leaves a blank line in place.
fn select_line_contents_only(ed: &mut Editor<'_>, top_row: usize, bot_row: usize) {
    ed.textarea.cancel_selection();
    ed.textarea.move_cursor(CursorMove::Jump(top_row, 0));
    ed.textarea.start_selection();
    ed.textarea.move_cursor(CursorMove::Jump(bot_row, 0));
    ed.textarea.move_cursor(CursorMove::End);
}

/// VisualLine finalize variant for the Change operator — preserves the
/// trailing newline so an empty line remains after the cut.
fn finalize_visual_line_selection_for_change(ed: &mut Editor<'_>) {
    let (cursor_row, _) = ed.textarea.cursor();
    let anchor_row = ed.vim.visual_line_anchor;
    let top = cursor_row.min(anchor_row);
    let bot = cursor_row.max(anchor_row);
    select_line_contents_only(ed, top, bot);
}

// ─── dd/cc/yy ──────────────────────────────────────────────────────────────

fn execute_line_op(ed: &mut Editor<'_>, op: Operator, count: usize) {
    let (row, col) = ed.textarea.cursor();
    let total = ed.textarea.lines().len();
    let end_row = (row + count.saturating_sub(1)).min(total.saturating_sub(1));

    match op {
        Operator::Yank => {
            // yy must not move the cursor.
            select_full_lines(ed, row, end_row);
            ed.vim.yank_linewise = true;
            ed.textarea.copy();
            if let Some(y) = non_empty_yank(ed) {
                ed.last_yank = Some(y);
            }
            ed.textarea.cancel_selection();
            ed.textarea.move_cursor(CursorMove::Jump(row, col));
            ed.vim.mode = Mode::Normal;
        }
        Operator::Delete => {
            ed.push_undo();
            let deleted_through_last = end_row + 1 >= total;
            select_full_lines(ed, row, end_row);
            ed.vim.yank_linewise = true;
            ed.mutate(|t| t.cut());
            if let Some(y) = non_empty_yank(ed) {
                ed.last_yank = Some(y);
            }
            // Vim's `dd` / `Ndd` leaves the cursor on the *first
            // non-blank* of the line that now occupies `row` — or, if
            // the deletion consumed the last line, the line above it.
            let total_after = ed.textarea.lines().len();
            let target_row = if total_after == 0 {
                0
            } else if deleted_through_last {
                row.saturating_sub(1).min(total_after.saturating_sub(1))
            } else {
                row.min(total_after.saturating_sub(1))
            };
            ed.textarea.move_cursor(CursorMove::Jump(target_row, 0));
            move_first_non_whitespace(ed);
            ed.vim.mode = Mode::Normal;
        }
        Operator::Change => {
            // `cc` / `3cc`: wipe contents of the covered lines but leave
            // a single blank line so insert-mode opens on it.
            ed.push_undo();
            select_line_contents_only(ed, row, end_row);
            ed.vim.yank_linewise = true;
            ed.mutate(|t| t.cut());
            if let Some(y) = non_empty_yank(ed) {
                ed.last_yank = Some(y);
            }
            ed.textarea.move_cursor(CursorMove::Jump(row, 0));
            begin_insert_noundo(ed, 1, InsertReason::AfterChange);
        }
        Operator::Uppercase | Operator::Lowercase | Operator::ToggleCase => {
            // `gUU` / `guu` / `g~~` — linewise case transform over
            // [row, end_row]. Preserve cursor on `row` (first non-blank
            // lines up with vim's behaviour).
            select_full_lines(ed, row, end_row);
            apply_case_op_to_selection(ed, op, (row, col));
            // After case-op on a linewise range vim puts the cursor on
            // the first non-blank of the starting line.
            move_first_non_whitespace(ed);
        }
        Operator::Indent | Operator::Outdent => {
            // `>>` / `N>>` / `<<` / `N<<` — linewise indent / outdent.
            ed.push_undo();
            if op == Operator::Indent {
                indent_rows(ed, row, end_row, 1);
            } else {
                outdent_rows(ed, row, end_row, 1);
            }
            ed.vim.mode = Mode::Normal;
        }
    }
}

// ─── Visual mode operators ─────────────────────────────────────────────────

fn apply_visual_operator(ed: &mut Editor<'_>, op: Operator) {
    match ed.vim.mode {
        Mode::VisualLine => {
            let (cursor_row, _) = ed.textarea.cursor();
            let top = cursor_row.min(ed.vim.visual_line_anchor);
            ed.vim.yank_linewise = true;
            match op {
                Operator::Yank => {
                    finalize_visual_line_selection(ed);
                    ed.textarea.copy();
                    if let Some(y) = non_empty_yank(ed) {
                        ed.last_yank = Some(y);
                    }
                    ed.textarea.cancel_selection();
                    ed.textarea.move_cursor(CursorMove::Jump(top, 0));
                    ed.vim.mode = Mode::Normal;
                }
                Operator::Delete => {
                    ed.push_undo();
                    finalize_visual_line_selection(ed);
                    ed.mutate(|t| t.cut());
                    if let Some(y) = non_empty_yank(ed) {
                        ed.last_yank = Some(y);
                    }
                    ed.vim.mode = Mode::Normal;
                }
                Operator::Change => {
                    // Vim `Vc`: wipe the line contents but leave a blank
                    // line in place so insert-mode starts on an empty row.
                    ed.push_undo();
                    finalize_visual_line_selection_for_change(ed);
                    ed.mutate(|t| t.cut());
                    if let Some(y) = non_empty_yank(ed) {
                        ed.last_yank = Some(y);
                    }
                    ed.textarea.move_cursor(CursorMove::Jump(top, 0));
                    begin_insert_noundo(ed, 1, InsertReason::AfterChange);
                }
                Operator::Uppercase | Operator::Lowercase | Operator::ToggleCase => {
                    finalize_visual_line_selection(ed);
                    apply_case_op_to_selection(ed, op, (top, 0));
                    move_first_non_whitespace(ed);
                }
                Operator::Indent | Operator::Outdent => {
                    ed.push_undo();
                    ed.textarea.cancel_selection();
                    let (cursor_row, _) = ed.textarea.cursor();
                    let bot = cursor_row.max(ed.vim.visual_line_anchor);
                    if op == Operator::Indent {
                        indent_rows(ed, top, bot, 1);
                    } else {
                        outdent_rows(ed, top, bot, 1);
                    }
                    ed.vim.mode = Mode::Normal;
                }
            }
        }
        Mode::Visual => {
            finalize_visual_char_selection(ed);
            // Reset linewise flag before copy/cut so non_empty_yank doesn't
            // mistakenly add a trailing newline from a prior linewise op.
            ed.vim.yank_linewise = false;
            match op {
                Operator::Yank => {
                    ed.textarea.copy();
                    if let Some(y) = non_empty_yank(ed) {
                        ed.last_yank = Some(y);
                    }
                    ed.textarea.cancel_selection();
                    ed.vim.mode = Mode::Normal;
                }
                Operator::Delete => {
                    ed.push_undo();
                    ed.mutate(|t| t.cut());
                    if let Some(y) = non_empty_yank(ed) {
                        ed.last_yank = Some(y);
                    }
                    ed.vim.mode = Mode::Normal;
                }
                Operator::Change => {
                    ed.push_undo();
                    ed.mutate(|t| t.cut());
                    if let Some(y) = non_empty_yank(ed) {
                        ed.last_yank = Some(y);
                    }
                    begin_insert_noundo(ed, 1, InsertReason::AfterChange);
                }
                Operator::Uppercase | Operator::Lowercase | Operator::ToggleCase => {
                    // Anchor stays where the visual selection started.
                    let anchor = ed.vim.visual_anchor;
                    let cursor = ed.textarea.cursor();
                    let (top, _) = order(anchor, cursor);
                    apply_case_op_to_selection(ed, op, top);
                }
                Operator::Indent | Operator::Outdent => {
                    ed.push_undo();
                    ed.textarea.cancel_selection();
                    let anchor = ed.vim.visual_anchor;
                    let cursor = ed.textarea.cursor();
                    let (top, bot) = order(anchor, cursor);
                    if op == Operator::Indent {
                        indent_rows(ed, top.0, bot.0, 1);
                    } else {
                        outdent_rows(ed, top.0, bot.0, 1);
                    }
                    ed.vim.mode = Mode::Normal;
                }
            }
        }
        Mode::VisualBlock => apply_block_operator(ed, op),
        _ => {}
    }
}

/// Compute `(top_row, bot_row, left_col, right_col)` for the current
/// VisualBlock selection. Columns are inclusive on both ends. Uses the
/// tracked virtual column (updated by h/l, preserved across j/k) so
/// ragged / empty rows don't collapse the block's width.
fn block_bounds(ed: &Editor<'_>) -> (usize, usize, usize, usize) {
    let (ar, ac) = ed.vim.block_anchor;
    let (cr, _) = ed.textarea.cursor();
    let cc = ed.vim.block_vcol;
    let top = ar.min(cr);
    let bot = ar.max(cr);
    let left = ac.min(cc);
    let right = ac.max(cc);
    (top, bot, left, right)
}

/// Update the virtual column after a motion in VisualBlock mode.
/// Horizontal motions sync `block_vcol` to the new cursor column;
/// vertical / non-h/l motions leave it alone so the intended column
/// survives clamping to shorter lines.
fn update_block_vcol(ed: &mut Editor<'_>, motion: &Motion) {
    match motion {
        Motion::Left
        | Motion::Right
        | Motion::WordFwd
        | Motion::BigWordFwd
        | Motion::WordBack
        | Motion::BigWordBack
        | Motion::WordEnd
        | Motion::BigWordEnd
        | Motion::WordEndBack
        | Motion::BigWordEndBack
        | Motion::LineStart
        | Motion::FirstNonBlank
        | Motion::LineEnd
        | Motion::Find { .. }
        | Motion::FindRepeat { .. }
        | Motion::MatchBracket => {
            ed.vim.block_vcol = ed.textarea.cursor().1;
        }
        // Up / Down / FileTop / FileBottom / Search — preserve vcol.
        _ => {}
    }
}

/// Yank / delete / change / replace a rectangular selection. Yanked text
/// is stored as one string per row joined with `\n` so pasting reproduces
/// the block as sequential lines. (Vim's true block-paste reinserts as
/// columns; we render the content with our char-wise paste path.)
fn apply_block_operator(ed: &mut Editor<'_>, op: Operator) {
    let (top, bot, left, right) = block_bounds(ed);
    // Snapshot the block text for yank / clipboard.
    let yank = block_yank(ed, top, bot, left, right);

    match op {
        Operator::Yank => {
            if !yank.is_empty() {
                ed.textarea.set_yank_text(yank.clone());
                ed.last_yank = Some(yank);
            }
            ed.vim.yank_linewise = false;
            ed.vim.mode = Mode::Normal;
            ed.textarea.move_cursor(CursorMove::Jump(top, left));
        }
        Operator::Delete => {
            ed.push_undo();
            delete_block_contents(ed, top, bot, left, right);
            if !yank.is_empty() {
                ed.textarea.set_yank_text(yank.clone());
                ed.last_yank = Some(yank);
            }
            ed.vim.yank_linewise = false;
            ed.vim.mode = Mode::Normal;
            ed.textarea.move_cursor(CursorMove::Jump(top, left));
        }
        Operator::Change => {
            ed.push_undo();
            delete_block_contents(ed, top, bot, left, right);
            if !yank.is_empty() {
                ed.textarea.set_yank_text(yank.clone());
                ed.last_yank = Some(yank);
            }
            ed.vim.yank_linewise = false;
            ed.textarea.move_cursor(CursorMove::Jump(top, left));
            begin_insert_noundo(
                ed,
                1,
                InsertReason::BlockEdge {
                    top,
                    bot,
                    col: left,
                },
            );
        }
        Operator::Uppercase | Operator::Lowercase | Operator::ToggleCase => {
            ed.push_undo();
            transform_block_case(ed, op, top, bot, left, right);
            ed.vim.mode = Mode::Normal;
            ed.textarea.move_cursor(CursorMove::Jump(top, left));
        }
        Operator::Indent | Operator::Outdent => {
            // VisualBlock `>` / `<` falls back to linewise indent over
            // the block's row range — vim does the same (column-wise
            // indent/outdent doesn't make sense).
            ed.push_undo();
            if op == Operator::Indent {
                indent_rows(ed, top, bot, 1);
            } else {
                outdent_rows(ed, top, bot, 1);
            }
            ed.vim.mode = Mode::Normal;
        }
    }
}

/// In-place case transform over the rectangular block
/// `(top..=bot, left..=right)`. Rows shorter than `left` are left
/// untouched — vim behaves the same way (ragged blocks).
fn transform_block_case(
    ed: &mut Editor<'_>,
    op: Operator,
    top: usize,
    bot: usize,
    left: usize,
    right: usize,
) {
    let mut lines: Vec<String> = ed.textarea.lines().to_vec();
    for r in top..=bot.min(lines.len().saturating_sub(1)) {
        let chars: Vec<char> = lines[r].chars().collect();
        if left >= chars.len() {
            continue;
        }
        let end = (right + 1).min(chars.len());
        let head: String = chars[..left].iter().collect();
        let mid: String = chars[left..end].iter().collect();
        let tail: String = chars[end..].iter().collect();
        let transformed = match op {
            Operator::Uppercase => mid.to_uppercase(),
            Operator::Lowercase => mid.to_lowercase(),
            Operator::ToggleCase => toggle_case_str(&mid),
            _ => mid,
        };
        lines[r] = format!("{head}{transformed}{tail}");
    }
    let saved_yank = ed.textarea.yank_text().to_string();
    let saved_linewise = ed.vim.yank_linewise;
    ed.restore(lines, (top, left));
    ed.textarea.set_yank_text(saved_yank);
    ed.vim.yank_linewise = saved_linewise;
}

fn block_yank(ed: &Editor<'_>, top: usize, bot: usize, left: usize, right: usize) -> String {
    let lines = ed.textarea.lines();
    let mut rows: Vec<String> = Vec::new();
    for r in top..=bot {
        let line = match lines.get(r) {
            Some(l) => l,
            None => break,
        };
        let chars: Vec<char> = line.chars().collect();
        let end = (right + 1).min(chars.len());
        if left >= chars.len() {
            rows.push(String::new());
        } else {
            rows.push(chars[left..end].iter().collect());
        }
    }
    rows.join("\n")
}

fn delete_block_contents(ed: &mut Editor<'_>, top: usize, bot: usize, left: usize, right: usize) {
    let mut lines: Vec<String> = ed.textarea.lines().to_vec();
    for r in top..=bot.min(lines.len().saturating_sub(1)) {
        let chars: Vec<char> = lines[r].chars().collect();
        if left >= chars.len() {
            continue;
        }
        let end = (right + 1).min(chars.len());
        let before: String = chars[..left].iter().collect();
        let after: String = chars[end..].iter().collect();
        lines[r] = format!("{before}{after}");
    }
    reset_textarea_lines(ed, lines);
}

/// Replace each character cell in the block with `ch`.
fn block_replace(ed: &mut Editor<'_>, ch: char) {
    let (top, bot, left, right) = block_bounds(ed);
    ed.push_undo();
    let mut lines: Vec<String> = ed.textarea.lines().to_vec();
    for r in top..=bot.min(lines.len().saturating_sub(1)) {
        let chars: Vec<char> = lines[r].chars().collect();
        if left >= chars.len() {
            continue;
        }
        let end = (right + 1).min(chars.len());
        let before: String = chars[..left].iter().collect();
        let middle: String = std::iter::repeat_n(ch, end - left).collect();
        let after: String = chars[end..].iter().collect();
        lines[r] = format!("{before}{middle}{after}");
    }
    reset_textarea_lines(ed, lines);
    ed.vim.mode = Mode::Normal;
    ed.textarea.move_cursor(CursorMove::Jump(top, left));
}

/// Replace the textarea's buffer with `lines` while preserving the yank
/// register and disabling the textarea's own history (we keep our own).
fn reset_textarea_lines(ed: &mut Editor<'_>, lines: Vec<String>) {
    let carried = ed.textarea.yank_text();
    ed.textarea = tui_textarea::TextArea::new(lines);
    ed.textarea.set_max_histories(0);
    if !carried.is_empty() {
        ed.textarea.set_yank_text(carried);
    }
    ed.mark_content_dirty();
}

// ─── Visual-line helpers ───────────────────────────────────────────────────

/// Build a tui-textarea selection from the stored visual anchor to the
/// live cursor, inclusive of the cell under the cursor. Called just
/// before copy / cut for the char-wise Visual operator — between
/// operators we keep the cursor free and paint the highlight through
/// the render overlay.
fn finalize_visual_char_selection(ed: &mut Editor<'_>) {
    let (ar, ac) = ed.vim.visual_anchor;
    let (cr, cc) = ed.textarea.cursor();
    let (start, end) = if (ar, ac) <= (cr, cc) {
        ((ar, ac), (cr, cc))
    } else {
        ((cr, cc), (ar, ac))
    };
    ed.textarea.cancel_selection();
    ed.textarea.move_cursor(CursorMove::Jump(start.0, start.1));
    ed.textarea.start_selection();
    ed.textarea.move_cursor(CursorMove::Jump(end.0, end.1));
    // Char-wise visual is inclusive of the cursor cell; tui-textarea
    // selection is exclusive-end, so step one forward.
    ed.textarea.move_cursor(CursorMove::Forward);
}

/// VisualLine keeps no live tui-textarea selection — the cursor is free
/// to sit wherever the user moved it, and the full-line highlight is
/// painted as a post-render overlay by the draw path. Operators build
/// a selection on demand via [`finalize_visual_line_selection`].
fn refresh_visual_line_selection(ed: &mut Editor<'_>) {
    ed.textarea.cancel_selection();
}

fn finalize_visual_line_selection(ed: &mut Editor<'_>) {
    let (cursor_row, _) = ed.textarea.cursor();
    let anchor_row = ed.vim.visual_line_anchor;
    let top = cursor_row.min(anchor_row);
    let bot = cursor_row.max(anchor_row);
    let total = ed.textarea.lines().len();
    ed.textarea.cancel_selection();
    ed.textarea.move_cursor(CursorMove::Jump(top, 0));
    ed.textarea.start_selection();
    if bot + 1 < total {
        ed.textarea.move_cursor(CursorMove::Jump(bot + 1, 0));
    } else {
        ed.textarea.move_cursor(CursorMove::Jump(bot, 0));
        ed.textarea.move_cursor(CursorMove::End);
    }
}

// ─── Text-object range computation ─────────────────────────────────────────

/// Cursor position as `(row, col)`.
type Pos = (usize, usize);

/// Returns `(start, end, kind)` where `end` is *exclusive* (one past the
/// last character to act on). `kind` is `Linewise` for line-oriented text
/// objects like paragraphs and `Exclusive` otherwise.
fn text_object_range(
    ed: &Editor<'_>,
    obj: TextObject,
    inner: bool,
) -> Option<(Pos, Pos, MotionKind)> {
    match obj {
        TextObject::Word { big } => {
            word_text_object(ed, inner, big).map(|(s, e)| (s, e, MotionKind::Exclusive))
        }
        TextObject::Quote(q) => {
            quote_text_object(ed, q, inner).map(|(s, e)| (s, e, MotionKind::Exclusive))
        }
        TextObject::Bracket(open) => {
            bracket_text_object(ed, open, inner).map(|(s, e)| (s, e, MotionKind::Exclusive))
        }
        TextObject::Paragraph => {
            paragraph_text_object(ed, inner).map(|(s, e)| (s, e, MotionKind::Linewise))
        }
    }
}

fn is_wordchar(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn word_text_object(
    ed: &Editor<'_>,
    inner: bool,
    big: bool,
) -> Option<((usize, usize), (usize, usize))> {
    let (row, col) = ed.textarea.cursor();
    let line = ed.textarea.lines().get(row)?;
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return None;
    }
    let at = col.min(chars.len().saturating_sub(1));
    let classify = |c: char| -> u8 {
        if c.is_whitespace() {
            0
        } else if big || is_wordchar(c) {
            1
        } else {
            2
        }
    };
    let cls = classify(chars[at]);
    let mut start = at;
    while start > 0 && classify(chars[start - 1]) == cls {
        start -= 1;
    }
    let mut end = at;
    while end + 1 < chars.len() && classify(chars[end + 1]) == cls {
        end += 1;
    }
    // Byte-offset helpers.
    let char_byte = |i: usize| {
        if i >= chars.len() {
            line.len()
        } else {
            line.char_indices().nth(i).map(|(b, _)| b).unwrap_or(0)
        }
    };
    let mut start_col = char_byte(start);
    // Exclusive end: byte index of char AFTER the last-included char.
    let mut end_col = char_byte(end + 1);
    if !inner {
        // `aw` — include trailing whitespace; if there's no trailing ws, absorb leading ws.
        let mut t = end + 1;
        let mut included_trailing = false;
        while t < chars.len() && chars[t].is_whitespace() {
            included_trailing = true;
            t += 1;
        }
        if included_trailing {
            end_col = char_byte(t);
        } else {
            let mut s = start;
            while s > 0 && chars[s - 1].is_whitespace() {
                s -= 1;
            }
            start_col = char_byte(s);
        }
    }
    Some(((row, start_col), (row, end_col)))
}

fn quote_text_object(
    ed: &Editor<'_>,
    q: char,
    inner: bool,
) -> Option<((usize, usize), (usize, usize))> {
    let (row, col) = ed.textarea.cursor();
    let line = ed.textarea.lines().get(row)?;
    let bytes = line.as_bytes();
    let q_byte = q as u8;
    // Find opening and closing quote on the same line.
    let mut positions: Vec<usize> = Vec::new();
    for (i, &b) in bytes.iter().enumerate() {
        if b == q_byte {
            positions.push(i);
        }
    }
    if positions.len() < 2 {
        return None;
    }
    let mut open_idx: Option<usize> = None;
    let mut close_idx: Option<usize> = None;
    for pair in positions.chunks(2) {
        if pair.len() < 2 {
            break;
        }
        if col >= pair[0] && col <= pair[1] {
            open_idx = Some(pair[0]);
            close_idx = Some(pair[1]);
            break;
        }
        if col < pair[0] {
            open_idx = Some(pair[0]);
            close_idx = Some(pair[1]);
            break;
        }
    }
    let open = open_idx?;
    let close = close_idx?;
    // End columns are *exclusive* — one past the last character to act on.
    if inner {
        if close <= open + 1 {
            return None;
        }
        Some(((row, open + 1), (row, close)))
    } else {
        Some(((row, open), (row, close + 1)))
    }
}

fn bracket_text_object(
    ed: &Editor<'_>,
    open: char,
    inner: bool,
) -> Option<((usize, usize), (usize, usize))> {
    let close = match open {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '<' => '>',
        _ => return None,
    };
    let (row, col) = ed.textarea.cursor();
    let lines = ed.textarea.lines();
    // Walk backward from cursor to find unbalanced opening.
    let open_pos = find_open_bracket(lines, row, col, open, close)?;
    let close_pos = find_close_bracket(lines, open_pos.0, open_pos.1 + 1, open, close)?;
    // End positions are *exclusive*.
    if inner {
        let inner_start = advance_pos(lines, open_pos);
        if inner_start.0 > close_pos.0
            || (inner_start.0 == close_pos.0 && inner_start.1 >= close_pos.1)
        {
            return None;
        }
        Some((inner_start, close_pos))
    } else {
        Some((open_pos, advance_pos(lines, close_pos)))
    }
}

fn find_open_bracket(
    lines: &[String],
    row: usize,
    col: usize,
    open: char,
    close: char,
) -> Option<(usize, usize)> {
    let mut depth: i32 = 0;
    let mut r = row;
    let mut c = col as isize;
    loop {
        let cur = &lines[r];
        let chars: Vec<char> = cur.chars().collect();
        while c >= 0 {
            let ch = chars[c as usize];
            if ch == close {
                depth += 1;
            } else if ch == open {
                if depth == 0 {
                    return Some((r, c as usize));
                }
                depth -= 1;
            }
            c -= 1;
        }
        if r == 0 {
            return None;
        }
        r -= 1;
        c = lines[r].chars().count() as isize - 1;
    }
}

fn find_close_bracket(
    lines: &[String],
    row: usize,
    start_col: usize,
    open: char,
    close: char,
) -> Option<(usize, usize)> {
    let mut depth: i32 = 0;
    let mut r = row;
    let mut c = start_col;
    loop {
        let cur = &lines[r];
        let chars: Vec<char> = cur.chars().collect();
        while c < chars.len() {
            let ch = chars[c];
            if ch == open {
                depth += 1;
            } else if ch == close {
                if depth == 0 {
                    return Some((r, c));
                }
                depth -= 1;
            }
            c += 1;
        }
        if r + 1 >= lines.len() {
            return None;
        }
        r += 1;
        c = 0;
    }
}

fn advance_pos(lines: &[String], pos: (usize, usize)) -> (usize, usize) {
    let (r, c) = pos;
    let line_len = lines[r].chars().count();
    if c < line_len {
        (r, c + 1)
    } else if r + 1 < lines.len() {
        (r + 1, 0)
    } else {
        pos
    }
}

fn paragraph_text_object(ed: &Editor<'_>, inner: bool) -> Option<((usize, usize), (usize, usize))> {
    let (row, _) = ed.textarea.cursor();
    let lines = ed.textarea.lines();
    if lines.is_empty() {
        return None;
    }
    // A paragraph is a run of non-blank lines.
    let is_blank = |r: usize| lines.get(r).map(|s| s.trim().is_empty()).unwrap_or(true);
    if is_blank(row) {
        return None;
    }
    let mut top = row;
    while top > 0 && !is_blank(top - 1) {
        top -= 1;
    }
    let mut bot = row;
    while bot + 1 < lines.len() && !is_blank(bot + 1) {
        bot += 1;
    }
    // For `ap`, include one trailing blank line if present.
    if !inner && bot + 1 < lines.len() && is_blank(bot + 1) {
        bot += 1;
    }
    let end_col = lines[bot].chars().count();
    Some(((top, 0), (bot, end_col)))
}

// ─── Individual commands ───────────────────────────────────────────────────

fn do_char_delete(ed: &mut Editor<'_>, forward: bool, count: usize) {
    ed.push_undo();
    for _ in 0..count {
        if forward {
            ed.mutate(|t| t.delete_next_char());
        } else {
            ed.mutate(|t| t.delete_char());
        }
    }
}

/// Vim `Ctrl-a` / `Ctrl-x` — find the next decimal number at or after the
/// cursor on the current line, add `delta`, leave the cursor on the last
/// digit of the result. No-op if the line has no digits to the right.
fn adjust_number(ed: &mut Editor<'_>, delta: i64) -> bool {
    use sqeel_buffer::{Edit, MotionKind, Position};
    ed.sync_buffer_content_from_textarea();
    let cursor = ed.buffer().cursor();
    let row = cursor.row;
    let chars: Vec<char> = match ed.buffer().line(row) {
        Some(l) => l.chars().collect(),
        None => return false,
    };
    let Some(digit_start) = (cursor.col..chars.len()).find(|&i| chars[i].is_ascii_digit()) else {
        return false;
    };
    let span_start = if digit_start > 0 && chars[digit_start - 1] == '-' {
        digit_start - 1
    } else {
        digit_start
    };
    let mut span_end = digit_start;
    while span_end < chars.len() && chars[span_end].is_ascii_digit() {
        span_end += 1;
    }
    let s: String = chars[span_start..span_end].iter().collect();
    let Ok(n) = s.parse::<i64>() else {
        return false;
    };
    let new_s = n.saturating_add(delta).to_string();

    ed.push_undo();
    let span_start_pos = Position::new(row, span_start);
    let span_end_pos = Position::new(row, span_end);
    ed.mutate_edit(Edit::DeleteRange {
        start: span_start_pos,
        end: span_end_pos,
        kind: MotionKind::Char,
    });
    ed.mutate_edit(Edit::InsertStr {
        at: span_start_pos,
        text: new_s.clone(),
    });
    let new_len = new_s.chars().count();
    ed.buffer_mut()
        .set_cursor(Position::new(row, span_start + new_len.saturating_sub(1)));
    ed.push_buffer_cursor_to_textarea();
    true
}

fn replace_char(ed: &mut Editor<'_>, ch: char, count: usize) {
    use sqeel_buffer::{Edit, MotionKind, Position};
    ed.push_undo();
    ed.sync_buffer_content_from_textarea();
    for _ in 0..count {
        let cursor = ed.buffer().cursor();
        let line_chars = ed
            .buffer()
            .line(cursor.row)
            .map(|l| l.chars().count())
            .unwrap_or(0);
        if cursor.col >= line_chars {
            break;
        }
        ed.mutate_edit(Edit::DeleteRange {
            start: cursor,
            end: Position::new(cursor.row, cursor.col + 1),
            kind: MotionKind::Char,
        });
        ed.mutate_edit(Edit::InsertChar { at: cursor, ch });
    }
    // Vim leaves the cursor on the last replaced char.
    ed.buffer_mut().move_left(1);
    ed.push_buffer_cursor_to_textarea();
}

fn toggle_case_at_cursor(ed: &mut Editor<'_>) {
    use sqeel_buffer::{Edit, MotionKind, Position};
    ed.sync_buffer_content_from_textarea();
    let cursor = ed.buffer().cursor();
    let Some(c) = ed
        .buffer()
        .line(cursor.row)
        .and_then(|l| l.chars().nth(cursor.col))
    else {
        return;
    };
    let toggled = if c.is_uppercase() {
        c.to_lowercase().next().unwrap_or(c)
    } else {
        c.to_uppercase().next().unwrap_or(c)
    };
    ed.mutate_edit(Edit::DeleteRange {
        start: cursor,
        end: Position::new(cursor.row, cursor.col + 1),
        kind: MotionKind::Char,
    });
    ed.mutate_edit(Edit::InsertChar {
        at: cursor,
        ch: toggled,
    });
}

fn join_line(ed: &mut Editor<'_>) {
    use sqeel_buffer::{Edit, Position};
    ed.sync_buffer_content_from_textarea();
    let row = ed.buffer().cursor().row;
    if row + 1 >= ed.buffer().row_count() {
        return;
    }
    let cur_line = ed.buffer().line(row).unwrap_or("").to_string();
    let next_raw = ed.buffer().line(row + 1).unwrap_or("").to_string();
    let next_trimmed = next_raw.trim_start();
    let cur_chars = cur_line.chars().count();
    let next_chars = next_raw.chars().count();
    // `J` inserts a single space iff both sides are non-empty after
    // stripping the next line's leading whitespace.
    let separator = if !cur_line.is_empty() && !next_trimmed.is_empty() {
        " "
    } else {
        ""
    };
    let joined = format!("{cur_line}{separator}{next_trimmed}");
    ed.mutate_edit(Edit::Replace {
        start: Position::new(row, 0),
        end: Position::new(row + 1, next_chars),
        with: joined,
    });
    // Vim parks the cursor on the inserted space — or at the join
    // point when no space went in (which is the same column either
    // way, since the space sits exactly at `cur_chars`).
    ed.buffer_mut().set_cursor(Position::new(row, cur_chars));
    ed.push_buffer_cursor_to_textarea();
}

/// `gJ` — join the next line onto the current one without inserting a
/// separating space or stripping leading whitespace.
fn join_line_raw(ed: &mut Editor<'_>) {
    use sqeel_buffer::{Edit, Position};
    ed.sync_buffer_content_from_textarea();
    let row = ed.buffer().cursor().row;
    if row + 1 >= ed.buffer().row_count() {
        return;
    }
    let join_col = ed
        .buffer()
        .line(row)
        .map(|l| l.chars().count())
        .unwrap_or(0);
    ed.mutate_edit(Edit::JoinLines {
        row,
        count: 1,
        with_space: false,
    });
    // Vim leaves the cursor at the join point (end of original line).
    ed.buffer_mut().set_cursor(Position::new(row, join_col));
    ed.push_buffer_cursor_to_textarea();
}

fn do_paste(ed: &mut Editor<'_>, before: bool, count: usize) {
    ed.push_undo();
    for _ in 0..count {
        if ed.vim.yank_linewise {
            let content = ed.textarea.yank_text();
            let text = content.trim_matches('\n').to_string();
            if before {
                ed.textarea.move_cursor(CursorMove::Head);
                ed.mutate(|t| t.insert_str(format!("{text}\n")));
                ed.textarea.move_cursor(CursorMove::Up);
            } else {
                ed.textarea.move_cursor(CursorMove::End);
                ed.mutate(|t| t.insert_str(format!("\n{text}")));
                ed.textarea.move_cursor(CursorMove::Head);
            }
            // Vim parks the cursor on the first non-blank of the pasted
            // line rather than col 0, so the user's next vertical motion
            // aims at something meaningful.
            move_first_non_whitespace(ed);
        } else if before {
            // P: paste at cursor, shifting existing char to the right.
            ed.mutate(|t| t.paste());
        } else {
            // p: paste *after* cursor. Advance one before inserting so the
            // first pasted char lands after the current char.
            ed.textarea.move_cursor(CursorMove::Forward);
            ed.mutate(|t| t.paste());
        }
    }
    // Any paste re-anchors the sticky column to the new cursor position.
    ed.vim.sticky_col = Some(ed.textarea.cursor().1);
}

fn do_undo(ed: &mut Editor<'_>) {
    if let Some((lines, cursor)) = ed.undo_stack.pop() {
        let current = ed.snapshot();
        ed.redo_stack.push(current);
        ed.restore(lines, cursor);
    }
    ed.vim.mode = Mode::Normal;
}

fn do_redo(ed: &mut Editor<'_>) {
    if let Some((lines, cursor)) = ed.redo_stack.pop() {
        let current = ed.snapshot();
        ed.undo_stack.push(current);
        ed.restore(lines, cursor);
    }
    ed.vim.mode = Mode::Normal;
}

// ─── Dot repeat ────────────────────────────────────────────────────────────

fn replay_last_change(ed: &mut Editor<'_>, outer_count: usize) {
    let Some(change) = ed.vim.last_change.clone() else {
        return;
    };
    ed.vim.replaying = true;
    let scale = if outer_count > 0 { outer_count } else { 1 };
    match change {
        LastChange::OpMotion {
            op,
            motion,
            count,
            inserted,
        } => {
            let total = count.max(1) * scale;
            apply_op_with_motion(ed, op, &motion, total);
            if let Some(text) = inserted {
                ed.mutate(|t| t.insert_str(&text));
                // Leave insert mode because the original c ended with Esc.
                if ed.vim.insert_session.take().is_some() {
                    let (row, col) = ed.textarea.cursor();
                    if col > 0 {
                        let _ = row;
                        ed.textarea.move_cursor(CursorMove::Back);
                    }
                    ed.vim.mode = Mode::Normal;
                }
            }
        }
        LastChange::OpTextObj {
            op,
            obj,
            inner,
            inserted,
        } => {
            apply_op_with_text_object(ed, op, obj, inner);
            if let Some(text) = inserted {
                ed.mutate(|t| t.insert_str(&text));
                if ed.vim.insert_session.take().is_some() {
                    let (row, col) = ed.textarea.cursor();
                    if col > 0 {
                        let _ = row;
                        ed.textarea.move_cursor(CursorMove::Back);
                    }
                    ed.vim.mode = Mode::Normal;
                }
            }
        }
        LastChange::LineOp {
            op,
            count,
            inserted,
        } => {
            let total = count.max(1) * scale;
            execute_line_op(ed, op, total);
            if let Some(text) = inserted {
                ed.mutate(|t| t.insert_str(&text));
                if ed.vim.insert_session.take().is_some() {
                    let (row, col) = ed.textarea.cursor();
                    if col > 0 {
                        let _ = row;
                        ed.textarea.move_cursor(CursorMove::Back);
                    }
                    ed.vim.mode = Mode::Normal;
                }
            }
        }
        LastChange::CharDel { forward, count } => {
            do_char_delete(ed, forward, count * scale);
        }
        LastChange::ReplaceChar { ch, count } => {
            replace_char(ed, ch, count * scale);
        }
        LastChange::ToggleCase { count } => {
            for _ in 0..count * scale {
                ed.push_undo();
                toggle_case_at_cursor(ed);
            }
        }
        LastChange::JoinLine { count } => {
            for _ in 0..count * scale {
                ed.push_undo();
                join_line(ed);
            }
        }
        LastChange::Paste { before, count } => {
            do_paste(ed, before, count * scale);
        }
        LastChange::DeleteToEol { inserted } => {
            ed.push_undo();
            ed.mutate(|t| t.delete_line_by_end());
            if let Some(text) = inserted {
                ed.mutate(|t| t.insert_str(&text));
            }
        }
        LastChange::OpenLine { above, inserted } => {
            ed.push_undo();
            if above {
                ed.textarea.move_cursor(CursorMove::Head);
                ed.mutate(|t| t.insert_newline());
                ed.textarea.move_cursor(CursorMove::Up);
            } else {
                ed.textarea.move_cursor(CursorMove::End);
                ed.mutate(|t| t.insert_newline());
            }
            ed.mutate(|t| t.insert_str(&inserted));
        }
        LastChange::InsertAt {
            entry,
            inserted,
            count,
        } => {
            ed.push_undo();
            match entry {
                InsertEntry::I => {}
                InsertEntry::ShiftI => move_first_non_whitespace(ed),
                InsertEntry::A => ed.textarea.move_cursor(CursorMove::Forward),
                InsertEntry::ShiftA => ed.textarea.move_cursor(CursorMove::End),
            }
            for _ in 0..count.max(1) {
                ed.mutate(|t| t.insert_str(&inserted));
            }
        }
    }
    ed.vim.replaying = false;
}

// ─── Extracting inserted text for replay ───────────────────────────────────

fn extract_inserted(before: &str, after: &str) -> String {
    let before_chars: Vec<char> = before.chars().collect();
    let after_chars: Vec<char> = after.chars().collect();
    if after_chars.len() <= before_chars.len() {
        return String::new();
    }
    let prefix = before_chars
        .iter()
        .zip(after_chars.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let max_suffix = before_chars.len() - prefix;
    let suffix = before_chars
        .iter()
        .rev()
        .zip(after_chars.iter().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    after_chars[prefix..after_chars.len() - suffix]
        .iter()
        .collect()
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::Editor;
    use crate::{KeybindingMode, VimMode};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn run_keys(e: &mut Editor<'_>, keys: &str) {
        // Minimal notation:
        //   <Esc> <CR> <BS> <Left/Right/Up/Down> <C-x>
        //   anything else = single char
        let mut iter = keys.chars().peekable();
        while let Some(c) = iter.next() {
            if c == '<' {
                let mut tag = String::new();
                for ch in iter.by_ref() {
                    if ch == '>' {
                        break;
                    }
                    tag.push(ch);
                }
                let ev = match tag.as_str() {
                    "Esc" => KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                    "CR" => KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    "BS" => KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                    "Space" => KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
                    "Up" => KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                    "Down" => KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                    "Left" => KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
                    "Right" => KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
                    // Vim-style literal `<` escape so tests can type
                    // the outdent operator without colliding with the
                    // `<tag>` notation this helper uses for special keys.
                    "lt" => KeyEvent::new(KeyCode::Char('<'), KeyModifiers::NONE),
                    s if s.starts_with("C-") => {
                        let ch = s.chars().nth(2).unwrap();
                        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
                    }
                    _ => continue,
                };
                e.handle_key(ev);
            } else {
                let mods = if c.is_uppercase() {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::NONE
                };
                e.handle_key(KeyEvent::new(KeyCode::Char(c), mods));
            }
        }
    }

    fn editor_with(content: &str) -> Editor<'static> {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content(content);
        e
    }

    #[test]
    fn f_char_jumps_on_line() {
        let mut e = editor_with("hello world");
        run_keys(&mut e, "fw");
        assert_eq!(e.textarea.cursor(), (0, 6));
    }

    #[test]
    fn cap_f_jumps_backward() {
        let mut e = editor_with("hello world");
        e.textarea.move_cursor(CursorMove::End);
        run_keys(&mut e, "Fo");
        assert_eq!(e.textarea.cursor().1, 7);
    }

    #[test]
    fn t_stops_before_char() {
        let mut e = editor_with("hello");
        run_keys(&mut e, "tl");
        assert_eq!(e.textarea.cursor(), (0, 1));
    }

    #[test]
    fn semicolon_repeats_find() {
        let mut e = editor_with("aa.bb.cc");
        run_keys(&mut e, "f.");
        assert_eq!(e.textarea.cursor().1, 2);
        run_keys(&mut e, ";");
        assert_eq!(e.textarea.cursor().1, 5);
    }

    #[test]
    fn comma_repeats_find_reverse() {
        let mut e = editor_with("aa.bb.cc");
        run_keys(&mut e, "f.");
        run_keys(&mut e, ";");
        run_keys(&mut e, ",");
        assert_eq!(e.textarea.cursor().1, 2);
    }

    #[test]
    fn di_quote_deletes_content() {
        let mut e = editor_with("foo \"bar\" baz");
        e.textarea.move_cursor(CursorMove::Jump(0, 6)); // inside quotes
        run_keys(&mut e, "di\"");
        assert_eq!(e.textarea.lines()[0], "foo \"\" baz");
    }

    #[test]
    fn da_quote_deletes_with_quotes() {
        let mut e = editor_with("foo \"bar\" baz");
        e.textarea.move_cursor(CursorMove::Jump(0, 6));
        run_keys(&mut e, "da\"");
        assert_eq!(e.textarea.lines()[0], "foo  baz");
    }

    #[test]
    fn ci_paren_deletes_and_inserts() {
        let mut e = editor_with("fn(a, b, c)");
        e.textarea.move_cursor(CursorMove::Jump(0, 5));
        run_keys(&mut e, "ci(");
        assert_eq!(e.vim_mode(), VimMode::Insert);
        assert_eq!(e.textarea.lines()[0], "fn()");
    }

    #[test]
    fn diw_deletes_inner_word() {
        let mut e = editor_with("hello world");
        e.textarea.move_cursor(CursorMove::Jump(0, 2));
        run_keys(&mut e, "diw");
        assert_eq!(e.textarea.lines()[0], " world");
    }

    #[test]
    fn daw_deletes_word_with_trailing_space() {
        let mut e = editor_with("hello world");
        run_keys(&mut e, "daw");
        assert_eq!(e.textarea.lines()[0], "world");
    }

    #[test]
    fn percent_jumps_to_matching_bracket() {
        let mut e = editor_with("foo(bar)");
        e.textarea.move_cursor(CursorMove::Jump(0, 3));
        run_keys(&mut e, "%");
        assert_eq!(e.textarea.cursor().1, 7);
        run_keys(&mut e, "%");
        assert_eq!(e.textarea.cursor().1, 3);
    }

    #[test]
    fn dot_repeats_last_change() {
        let mut e = editor_with("aaa bbb ccc");
        run_keys(&mut e, "dw");
        assert_eq!(e.textarea.lines()[0], "bbb ccc");
        run_keys(&mut e, ".");
        assert_eq!(e.textarea.lines()[0], "ccc");
    }

    #[test]
    fn dot_repeats_change_operator_with_text() {
        let mut e = editor_with("foo foo foo");
        run_keys(&mut e, "cwbar<Esc>");
        assert_eq!(e.textarea.lines()[0], "bar foo foo");
        // Move past the space.
        run_keys(&mut e, "w");
        run_keys(&mut e, ".");
        assert_eq!(e.textarea.lines()[0], "bar bar foo");
    }

    #[test]
    fn dot_repeats_x() {
        let mut e = editor_with("abcdef");
        run_keys(&mut e, "x");
        run_keys(&mut e, "..");
        assert_eq!(e.textarea.lines()[0], "def");
    }

    #[test]
    fn count_operator_motion_compose() {
        let mut e = editor_with("one two three four five");
        run_keys(&mut e, "d3w");
        assert_eq!(e.textarea.lines()[0], "four five");
    }

    #[test]
    fn two_dd_deletes_two_lines() {
        let mut e = editor_with("a\nb\nc");
        run_keys(&mut e, "2dd");
        assert_eq!(e.textarea.lines().len(), 1);
        assert_eq!(e.textarea.lines()[0], "c");
    }

    /// Vim's `dd` leaves the cursor on the first non-blank of the line
    /// that now sits at the deleted row — not at the end of the
    /// previous line, which is where tui-textarea's raw cut would
    /// park it.
    #[test]
    fn dd_in_middle_puts_cursor_on_first_non_blank_of_next() {
        let mut e = editor_with("one\ntwo\n    three\nfour");
        e.textarea.move_cursor(CursorMove::Jump(1, 2));
        run_keys(&mut e, "dd");
        // Buffer: ["one", "    three", "four"]
        assert_eq!(e.textarea.lines()[1], "    three");
        assert_eq!(e.textarea.cursor(), (1, 4));
    }

    #[test]
    fn dd_on_last_line_puts_cursor_on_first_non_blank_of_prev() {
        let mut e = editor_with("one\n  two\nthree");
        e.textarea.move_cursor(CursorMove::Jump(2, 0));
        run_keys(&mut e, "dd");
        // Buffer: ["one", "  two"]
        assert_eq!(e.textarea.lines().len(), 2);
        assert_eq!(e.textarea.cursor(), (1, 2));
    }

    #[test]
    fn dd_on_only_line_leaves_empty_buffer_and_cursor_at_zero() {
        let mut e = editor_with("lonely");
        run_keys(&mut e, "dd");
        assert_eq!(e.textarea.lines().len(), 1);
        assert_eq!(e.textarea.lines()[0], "");
        assert_eq!(e.textarea.cursor(), (0, 0));
    }

    #[test]
    fn count_dd_puts_cursor_on_first_non_blank_of_remaining() {
        let mut e = editor_with("a\nb\nc\n   d\ne");
        // Cursor on row 1, "3dd" deletes b/c/   d → lines become [a, e].
        e.textarea.move_cursor(CursorMove::Jump(1, 0));
        run_keys(&mut e, "3dd");
        assert_eq!(e.textarea.lines(), &["a".to_string(), "e".to_string()]);
        assert_eq!(e.textarea.cursor(), (1, 0));
    }

    #[test]
    fn gu_lowercases_motion_range() {
        let mut e = editor_with("HELLO WORLD");
        run_keys(&mut e, "guw");
        assert_eq!(e.textarea.lines()[0], "hello WORLD");
        assert_eq!(e.textarea.cursor(), (0, 0));
    }

    #[test]
    fn g_u_uppercases_text_object() {
        let mut e = editor_with("hello world");
        // gUiw uppercases the word at the cursor.
        run_keys(&mut e, "gUiw");
        assert_eq!(e.textarea.lines()[0], "HELLO world");
        assert_eq!(e.textarea.cursor(), (0, 0));
    }

    #[test]
    fn g_tilde_toggles_case_of_range() {
        let mut e = editor_with("Hello World");
        run_keys(&mut e, "g~iw");
        assert_eq!(e.textarea.lines()[0], "hELLO World");
    }

    #[test]
    fn g_uu_uppercases_current_line() {
        let mut e = editor_with("select 1\nselect 2");
        run_keys(&mut e, "gUU");
        assert_eq!(e.textarea.lines()[0], "SELECT 1");
        assert_eq!(e.textarea.lines()[1], "select 2");
    }

    #[test]
    fn gugu_lowercases_current_line() {
        let mut e = editor_with("FOO BAR\nBAZ");
        run_keys(&mut e, "gugu");
        assert_eq!(e.textarea.lines()[0], "foo bar");
    }

    #[test]
    fn visual_u_uppercases_selection() {
        let mut e = editor_with("hello world");
        // v + e selects "hello" (inclusive of last char), U uppercases.
        run_keys(&mut e, "veU");
        assert_eq!(e.textarea.lines()[0], "HELLO world");
    }

    #[test]
    fn visual_line_u_lowercases_line() {
        let mut e = editor_with("HELLO WORLD\nOTHER");
        run_keys(&mut e, "Vu");
        assert_eq!(e.textarea.lines()[0], "hello world");
        assert_eq!(e.textarea.lines()[1], "OTHER");
    }

    #[test]
    fn g_uu_with_count_uppercases_multiple_lines() {
        let mut e = editor_with("one\ntwo\nthree\nfour");
        // `3gUU` uppercases 3 lines starting from the cursor.
        run_keys(&mut e, "3gUU");
        assert_eq!(e.textarea.lines()[0], "ONE");
        assert_eq!(e.textarea.lines()[1], "TWO");
        assert_eq!(e.textarea.lines()[2], "THREE");
        assert_eq!(e.textarea.lines()[3], "four");
    }

    #[test]
    fn double_gt_indents_current_line() {
        let mut e = editor_with("hello");
        run_keys(&mut e, ">>");
        assert_eq!(e.textarea.lines()[0], "  hello");
        // Cursor lands on first non-blank.
        assert_eq!(e.textarea.cursor(), (0, 2));
    }

    #[test]
    fn double_lt_outdents_current_line() {
        let mut e = editor_with("    hello");
        run_keys(&mut e, "<lt><lt>");
        assert_eq!(e.textarea.lines()[0], "  hello");
        assert_eq!(e.textarea.cursor(), (0, 2));
    }

    #[test]
    fn count_double_gt_indents_multiple_lines() {
        let mut e = editor_with("a\nb\nc\nd");
        // `3>>` indents 3 lines starting at cursor.
        run_keys(&mut e, "3>>");
        assert_eq!(e.textarea.lines()[0], "  a");
        assert_eq!(e.textarea.lines()[1], "  b");
        assert_eq!(e.textarea.lines()[2], "  c");
        assert_eq!(e.textarea.lines()[3], "d");
    }

    #[test]
    fn outdent_clips_ragged_leading_whitespace() {
        // Only one space of indent — outdent should strip what's
        // there, not leave anything negative.
        let mut e = editor_with(" x");
        run_keys(&mut e, "<lt><lt>");
        assert_eq!(e.textarea.lines()[0], "x");
    }

    #[test]
    fn indent_motion_is_always_linewise() {
        // `>w` indents the current line (linewise) — it doesn't
        // insert spaces into the middle of the word.
        let mut e = editor_with("foo bar");
        run_keys(&mut e, ">w");
        assert_eq!(e.textarea.lines()[0], "  foo bar");
    }

    #[test]
    fn indent_text_object_extends_over_paragraph() {
        let mut e = editor_with("a\nb\n\nc\nd");
        // `>ap` indents the whole paragraph (rows 0..=1).
        run_keys(&mut e, ">ap");
        assert_eq!(e.textarea.lines()[0], "  a");
        assert_eq!(e.textarea.lines()[1], "  b");
        assert_eq!(e.textarea.lines()[2], "");
        assert_eq!(e.textarea.lines()[3], "c");
    }

    #[test]
    fn visual_line_indent_shifts_selected_rows() {
        let mut e = editor_with("x\ny\nz");
        // Vj selects rows 0..=1 linewise; `>` indents.
        run_keys(&mut e, "Vj>");
        assert_eq!(e.textarea.lines()[0], "  x");
        assert_eq!(e.textarea.lines()[1], "  y");
        assert_eq!(e.textarea.lines()[2], "z");
    }

    #[test]
    fn outdent_empty_line_is_noop() {
        let mut e = editor_with("\nfoo");
        run_keys(&mut e, "<lt><lt>");
        assert_eq!(e.textarea.lines()[0], "");
    }

    #[test]
    fn indent_skips_empty_lines() {
        // Vim convention: `>>` on an empty line doesn't pad it with
        // trailing whitespace.
        let mut e = editor_with("");
        run_keys(&mut e, ">>");
        assert_eq!(e.textarea.lines()[0], "");
    }

    #[test]
    fn insert_ctrl_t_indents_current_line() {
        let mut e = editor_with("x");
        // Enter insert, Ctrl-t indents the line; cursor advances too.
        run_keys(&mut e, "i<C-t>");
        assert_eq!(e.textarea.lines()[0], "  x");
        // After insert-mode start `i` cursor was at (0, 0); Ctrl-t
        // shifts it by SHIFTWIDTH=2.
        assert_eq!(e.textarea.cursor(), (0, 2));
    }

    #[test]
    fn insert_ctrl_d_outdents_current_line() {
        let mut e = editor_with("    x");
        // Enter insert-at-end `A`, Ctrl-d outdents by shiftwidth.
        run_keys(&mut e, "A<C-d>");
        assert_eq!(e.textarea.lines()[0], "  x");
    }

    #[test]
    fn h_at_col_zero_does_not_wrap_to_prev_line() {
        let mut e = editor_with("first\nsecond");
        e.textarea.move_cursor(CursorMove::Jump(1, 0));
        run_keys(&mut e, "h");
        // Cursor must stay on row 1 col 0 — vim default doesn't wrap.
        assert_eq!(e.textarea.cursor(), (1, 0));
    }

    #[test]
    fn l_at_last_char_does_not_wrap_to_next_line() {
        let mut e = editor_with("ab\ncd");
        // Move to last char of row 0 (col 1).
        e.textarea.move_cursor(CursorMove::Jump(0, 1));
        run_keys(&mut e, "l");
        // Cursor stays on last char — no wrap.
        assert_eq!(e.textarea.cursor(), (0, 1));
    }

    #[test]
    fn count_l_clamps_at_line_end() {
        let mut e = editor_with("abcde");
        // 20l starting at col 0 should land on last char (col 4),
        // not overflow / wrap.
        run_keys(&mut e, "20l");
        assert_eq!(e.textarea.cursor(), (0, 4));
    }

    #[test]
    fn count_h_clamps_at_col_zero() {
        let mut e = editor_with("abcde");
        e.textarea.move_cursor(CursorMove::Jump(0, 3));
        run_keys(&mut e, "20h");
        assert_eq!(e.textarea.cursor(), (0, 0));
    }

    #[test]
    fn dl_on_last_char_still_deletes_it() {
        // `dl` / `x`-equivalent at EOL must delete the last char —
        // operator motion allows endpoint past-last even though bare
        // `l` stops before.
        let mut e = editor_with("ab");
        e.textarea.move_cursor(CursorMove::Jump(0, 1));
        run_keys(&mut e, "dl");
        assert_eq!(e.textarea.lines()[0], "a");
    }

    #[test]
    fn case_op_preserves_yank_register() {
        let mut e = editor_with("target");
        run_keys(&mut e, "yy");
        let yank_before = e.textarea.yank_text().to_string();
        // gUU changes the line but must not clobber the yank register.
        run_keys(&mut e, "gUU");
        assert_eq!(e.textarea.lines()[0], "TARGET");
        assert_eq!(
            e.textarea.yank_text(),
            yank_before,
            "case ops must preserve the yank buffer"
        );
    }

    #[test]
    fn dap_deletes_paragraph() {
        let mut e = editor_with("a\nb\n\nc\nd");
        run_keys(&mut e, "dap");
        assert_eq!(e.textarea.lines().first().map(String::as_str), Some("c"));
    }

    #[test]
    fn star_finds_next_occurrence() {
        let mut e = editor_with("foo bar foo baz");
        run_keys(&mut e, "*");
        assert_eq!(e.textarea.cursor().1, 8);
    }

    #[test]
    fn n_repeats_last_search_forward() {
        let mut e = editor_with("foo bar foo baz foo");
        e.textarea.set_search_pattern("foo").unwrap();
        run_keys(&mut e, "n");
        assert_eq!(e.textarea.cursor().1, 8);
        run_keys(&mut e, "n");
        assert_eq!(e.textarea.cursor().1, 16);
    }

    #[test]
    fn shift_n_reverses_search() {
        let mut e = editor_with("foo bar foo baz foo");
        e.textarea.set_search_pattern("foo").unwrap();
        run_keys(&mut e, "nn");
        assert_eq!(e.textarea.cursor().1, 16);
        run_keys(&mut e, "N");
        assert_eq!(e.textarea.cursor().1, 8);
    }

    #[test]
    fn n_noop_without_pattern() {
        let mut e = editor_with("foo bar");
        run_keys(&mut e, "n");
        assert_eq!(e.textarea.cursor(), (0, 0));
    }

    #[test]
    fn visual_line_preserves_cursor_column() {
        // V should never drag the cursor off its natural column — the
        // highlight is painted as a post-render overlay instead.
        let mut e = editor_with("hello world\nanother one\nbye");
        run_keys(&mut e, "lllll"); // col 5
        run_keys(&mut e, "V");
        assert_eq!(e.vim_mode(), VimMode::VisualLine);
        assert_eq!(e.textarea.cursor(), (0, 5));
        run_keys(&mut e, "j");
        assert_eq!(e.textarea.cursor(), (1, 5));
    }

    #[test]
    fn visual_line_yank_includes_trailing_newline() {
        let mut e = editor_with("aaa\nbbb\nccc");
        run_keys(&mut e, "Vjy");
        // Two lines yanked — must be `aaa\nbbb\n`, trailing newline preserved.
        assert_eq!(e.last_yank.as_deref(), Some("aaa\nbbb\n"));
    }

    #[test]
    fn visual_line_yank_last_line_trailing_newline() {
        let mut e = editor_with("aaa\nbbb\nccc");
        // Move to the last line and yank with V (final buffer line).
        run_keys(&mut e, "jj");
        run_keys(&mut e, "Vy");
        assert_eq!(e.last_yank.as_deref(), Some("ccc\n"));
    }

    #[test]
    fn yy_on_last_line_has_trailing_newline() {
        let mut e = editor_with("aaa\nbbb\nccc");
        run_keys(&mut e, "jj");
        run_keys(&mut e, "yy");
        assert_eq!(e.last_yank.as_deref(), Some("ccc\n"));
    }

    #[test]
    fn yy_in_middle_has_trailing_newline() {
        let mut e = editor_with("aaa\nbbb\nccc");
        run_keys(&mut e, "j");
        run_keys(&mut e, "yy");
        assert_eq!(e.last_yank.as_deref(), Some("bbb\n"));
    }

    #[test]
    fn di_single_quote() {
        let mut e = editor_with("say 'hello world' now");
        e.textarea.move_cursor(CursorMove::Jump(0, 7));
        run_keys(&mut e, "di'");
        assert_eq!(e.textarea.lines()[0], "say '' now");
    }

    #[test]
    fn da_single_quote() {
        let mut e = editor_with("say 'hello' now");
        e.textarea.move_cursor(CursorMove::Jump(0, 7));
        run_keys(&mut e, "da'");
        assert_eq!(e.textarea.lines()[0], "say  now");
    }

    #[test]
    fn di_backtick() {
        let mut e = editor_with("say `hi` now");
        e.textarea.move_cursor(CursorMove::Jump(0, 5));
        run_keys(&mut e, "di`");
        assert_eq!(e.textarea.lines()[0], "say `` now");
    }

    #[test]
    fn di_brace() {
        let mut e = editor_with("fn { a; b; c }");
        e.textarea.move_cursor(CursorMove::Jump(0, 7));
        run_keys(&mut e, "di{");
        assert_eq!(e.textarea.lines()[0], "fn {}");
    }

    #[test]
    fn di_bracket() {
        let mut e = editor_with("arr[1, 2, 3]");
        e.textarea.move_cursor(CursorMove::Jump(0, 5));
        run_keys(&mut e, "di[");
        assert_eq!(e.textarea.lines()[0], "arr[]");
    }

    #[test]
    fn dab_deletes_around_paren() {
        let mut e = editor_with("fn(a, b) + 1");
        e.textarea.move_cursor(CursorMove::Jump(0, 4));
        run_keys(&mut e, "dab");
        assert_eq!(e.textarea.lines()[0], "fn + 1");
    }

    #[test]
    fn da_big_b_deletes_around_brace() {
        let mut e = editor_with("x = {a: 1}");
        e.textarea.move_cursor(CursorMove::Jump(0, 6));
        run_keys(&mut e, "daB");
        assert_eq!(e.textarea.lines()[0], "x = ");
    }

    #[test]
    fn di_big_w_deletes_bigword() {
        let mut e = editor_with("foo-bar baz");
        e.textarea.move_cursor(CursorMove::Jump(0, 2));
        run_keys(&mut e, "diW");
        assert_eq!(e.textarea.lines()[0], " baz");
    }

    #[test]
    fn visual_select_inner_word() {
        let mut e = editor_with("hello world");
        e.textarea.move_cursor(CursorMove::Jump(0, 2));
        run_keys(&mut e, "viw");
        assert_eq!(e.vim_mode(), VimMode::Visual);
        run_keys(&mut e, "y");
        assert_eq!(e.last_yank.as_deref(), Some("hello"));
    }

    #[test]
    fn visual_select_inner_quote() {
        let mut e = editor_with("foo \"bar\" baz");
        e.textarea.move_cursor(CursorMove::Jump(0, 6));
        run_keys(&mut e, "vi\"");
        run_keys(&mut e, "y");
        assert_eq!(e.last_yank.as_deref(), Some("bar"));
    }

    #[test]
    fn visual_select_inner_paren() {
        let mut e = editor_with("fn(a, b)");
        e.textarea.move_cursor(CursorMove::Jump(0, 4));
        run_keys(&mut e, "vi(");
        run_keys(&mut e, "y");
        assert_eq!(e.last_yank.as_deref(), Some("a, b"));
    }

    #[test]
    fn visual_select_outer_brace() {
        let mut e = editor_with("{x}");
        e.textarea.move_cursor(CursorMove::Jump(0, 1));
        run_keys(&mut e, "va{");
        run_keys(&mut e, "y");
        assert_eq!(e.last_yank.as_deref(), Some("{x}"));
    }

    #[test]
    fn caw_changes_word_with_trailing_space() {
        let mut e = editor_with("hello world");
        run_keys(&mut e, "cawfoo<Esc>");
        assert_eq!(e.textarea.lines()[0], "fooworld");
    }

    #[test]
    fn visual_char_yank_preserves_raw_text() {
        let mut e = editor_with("hello world");
        run_keys(&mut e, "vllly");
        assert_eq!(e.last_yank.as_deref(), Some("hell"));
    }

    #[test]
    fn single_line_visual_line_selects_full_line_on_yank() {
        let mut e = editor_with("hello world\nbye");
        run_keys(&mut e, "V");
        // Yank the selection — should include the full line + trailing
        // newline (linewise yank convention).
        run_keys(&mut e, "y");
        assert_eq!(e.last_yank.as_deref(), Some("hello world\n"));
    }

    #[test]
    fn visual_line_extends_both_directions() {
        let mut e = editor_with("aaa\nbbb\nccc\nddd");
        run_keys(&mut e, "jjj"); // row 3, col 0
        run_keys(&mut e, "V");
        assert_eq!(e.textarea.cursor(), (3, 0));
        run_keys(&mut e, "k");
        // Cursor is free to sit on its natural column — no forced Jump.
        assert_eq!(e.textarea.cursor(), (2, 0));
        run_keys(&mut e, "k");
        assert_eq!(e.textarea.cursor(), (1, 0));
    }

    #[test]
    fn visual_char_preserves_cursor_column() {
        let mut e = editor_with("hello world");
        run_keys(&mut e, "lllll"); // col 5
        run_keys(&mut e, "v");
        assert_eq!(e.textarea.cursor(), (0, 5));
        run_keys(&mut e, "ll");
        assert_eq!(e.textarea.cursor(), (0, 7));
    }

    #[test]
    fn visual_char_highlight_bounds_order() {
        let mut e = editor_with("abcdef");
        run_keys(&mut e, "lll"); // col 3
        run_keys(&mut e, "v");
        run_keys(&mut e, "hh"); // col 1
        // Anchor (0, 3), cursor (0, 1). Bounds ordered: start=(0,1) end=(0,3).
        assert_eq!(e.char_highlight(), Some(((0, 1), (0, 3))));
    }

    #[test]
    fn visual_line_highlight_bounds() {
        let mut e = editor_with("a\nb\nc");
        run_keys(&mut e, "V");
        assert_eq!(e.line_highlight(), Some((0, 0)));
        run_keys(&mut e, "j");
        assert_eq!(e.line_highlight(), Some((0, 1)));
        run_keys(&mut e, "j");
        assert_eq!(e.line_highlight(), Some((0, 2)));
    }

    // ─── Basic motions ─────────────────────────────────────────────────────

    #[test]
    fn h_moves_left() {
        let mut e = editor_with("hello");
        e.textarea.move_cursor(CursorMove::Jump(0, 3));
        run_keys(&mut e, "h");
        assert_eq!(e.textarea.cursor(), (0, 2));
    }

    #[test]
    fn l_moves_right() {
        let mut e = editor_with("hello");
        run_keys(&mut e, "l");
        assert_eq!(e.textarea.cursor(), (0, 1));
    }

    #[test]
    fn k_moves_up() {
        let mut e = editor_with("a\nb\nc");
        e.textarea.move_cursor(CursorMove::Jump(2, 0));
        run_keys(&mut e, "k");
        assert_eq!(e.textarea.cursor(), (1, 0));
    }

    #[test]
    fn zero_moves_to_line_start() {
        let mut e = editor_with("    hello");
        run_keys(&mut e, "$");
        run_keys(&mut e, "0");
        assert_eq!(e.textarea.cursor().1, 0);
    }

    #[test]
    fn caret_moves_to_first_non_blank() {
        let mut e = editor_with("    hello");
        run_keys(&mut e, "0");
        run_keys(&mut e, "^");
        assert_eq!(e.textarea.cursor().1, 4);
    }

    #[test]
    fn dollar_moves_to_last_char() {
        let mut e = editor_with("hello");
        run_keys(&mut e, "$");
        assert_eq!(e.textarea.cursor().1, 4);
    }

    #[test]
    fn dollar_on_empty_line_stays_at_col_zero() {
        let mut e = editor_with("");
        run_keys(&mut e, "$");
        assert_eq!(e.textarea.cursor().1, 0);
    }

    #[test]
    fn w_jumps_to_next_word() {
        let mut e = editor_with("foo bar baz");
        run_keys(&mut e, "w");
        assert_eq!(e.textarea.cursor().1, 4);
    }

    #[test]
    fn b_jumps_back_a_word() {
        let mut e = editor_with("foo bar");
        e.textarea.move_cursor(CursorMove::Jump(0, 6));
        run_keys(&mut e, "b");
        assert_eq!(e.textarea.cursor().1, 4);
    }

    #[test]
    fn e_jumps_to_word_end() {
        let mut e = editor_with("foo bar");
        run_keys(&mut e, "e");
        assert_eq!(e.textarea.cursor().1, 2);
    }

    // ─── Operators with line-edge and file-edge motions ───────────────────

    #[test]
    fn d_dollar_deletes_to_eol() {
        let mut e = editor_with("hello world");
        e.textarea.move_cursor(CursorMove::Jump(0, 5));
        run_keys(&mut e, "d$");
        assert_eq!(e.textarea.lines()[0], "hello");
    }

    #[test]
    fn d_zero_deletes_to_line_start() {
        let mut e = editor_with("hello world");
        e.textarea.move_cursor(CursorMove::Jump(0, 6));
        run_keys(&mut e, "d0");
        assert_eq!(e.textarea.lines()[0], "world");
    }

    #[test]
    fn d_caret_deletes_to_first_non_blank() {
        let mut e = editor_with("    hello");
        e.textarea.move_cursor(CursorMove::Jump(0, 6));
        run_keys(&mut e, "d^");
        assert_eq!(e.textarea.lines()[0], "    llo");
    }

    #[test]
    fn d_capital_g_deletes_to_end_of_file() {
        let mut e = editor_with("a\nb\nc\nd");
        e.textarea.move_cursor(CursorMove::Jump(1, 0));
        run_keys(&mut e, "dG");
        assert_eq!(e.textarea.lines(), &["a".to_string()]);
    }

    #[test]
    fn d_gg_deletes_to_start_of_file() {
        let mut e = editor_with("a\nb\nc\nd");
        e.textarea.move_cursor(CursorMove::Jump(2, 0));
        run_keys(&mut e, "dgg");
        assert_eq!(e.textarea.lines(), &["d".to_string()]);
    }

    #[test]
    fn cw_is_ce_quirk() {
        // `cw` on a non-blank word must NOT eat the trailing whitespace;
        // it behaves like `ce` so the replacement lands before the space.
        let mut e = editor_with("foo bar");
        run_keys(&mut e, "cwxyz<Esc>");
        assert_eq!(e.textarea.lines()[0], "xyz bar");
    }

    // ─── Single-char edits ────────────────────────────────────────────────

    #[test]
    fn big_d_deletes_to_eol() {
        let mut e = editor_with("hello world");
        e.textarea.move_cursor(CursorMove::Jump(0, 5));
        run_keys(&mut e, "D");
        assert_eq!(e.textarea.lines()[0], "hello");
    }

    #[test]
    fn big_c_deletes_to_eol_and_inserts() {
        let mut e = editor_with("hello world");
        e.textarea.move_cursor(CursorMove::Jump(0, 5));
        run_keys(&mut e, "C!<Esc>");
        assert_eq!(e.textarea.lines()[0], "hello!");
    }

    #[test]
    fn j_joins_next_line_with_space() {
        let mut e = editor_with("hello\nworld");
        run_keys(&mut e, "J");
        assert_eq!(e.textarea.lines(), &["hello world".to_string()]);
    }

    #[test]
    fn j_strips_leading_whitespace_on_join() {
        let mut e = editor_with("hello\n    world");
        run_keys(&mut e, "J");
        assert_eq!(e.textarea.lines(), &["hello world".to_string()]);
    }

    #[test]
    fn big_x_deletes_char_before_cursor() {
        let mut e = editor_with("hello");
        e.textarea.move_cursor(CursorMove::Jump(0, 3));
        run_keys(&mut e, "X");
        assert_eq!(e.textarea.lines()[0], "helo");
    }

    #[test]
    fn s_substitutes_char_and_enters_insert() {
        let mut e = editor_with("hello");
        run_keys(&mut e, "sX<Esc>");
        assert_eq!(e.textarea.lines()[0], "Xello");
    }

    #[test]
    fn count_x_deletes_many() {
        let mut e = editor_with("abcdef");
        run_keys(&mut e, "3x");
        assert_eq!(e.textarea.lines()[0], "def");
    }

    // ─── Paste ────────────────────────────────────────────────────────────

    #[test]
    fn p_pastes_charwise_after_cursor() {
        let mut e = editor_with("hello");
        run_keys(&mut e, "yw");
        run_keys(&mut e, "$p");
        assert_eq!(e.textarea.lines()[0], "hellohello");
    }

    #[test]
    fn capital_p_pastes_charwise_before_cursor() {
        let mut e = editor_with("hello");
        // Yank "he" (2 chars) then paste it before the cursor.
        run_keys(&mut e, "v");
        run_keys(&mut e, "l");
        run_keys(&mut e, "y");
        run_keys(&mut e, "$P");
        // After yank cursor is at 0; $ goes to end (col 4), P pastes
        // before cursor — "hell" + "he" + "o" = "hellheo".
        assert_eq!(e.textarea.lines()[0], "hellheo");
    }

    #[test]
    fn p_pastes_linewise_below() {
        let mut e = editor_with("one\ntwo\nthree");
        run_keys(&mut e, "yy");
        run_keys(&mut e, "p");
        assert_eq!(
            e.textarea.lines(),
            &[
                "one".to_string(),
                "one".to_string(),
                "two".to_string(),
                "three".to_string()
            ]
        );
    }

    #[test]
    fn capital_p_pastes_linewise_above() {
        let mut e = editor_with("one\ntwo");
        e.textarea.move_cursor(CursorMove::Jump(1, 0));
        run_keys(&mut e, "yy");
        run_keys(&mut e, "P");
        assert_eq!(
            e.textarea.lines(),
            &["one".to_string(), "two".to_string(), "two".to_string()]
        );
    }

    // ─── Reverse word search ──────────────────────────────────────────────

    #[test]
    fn hash_finds_previous_occurrence() {
        let mut e = editor_with("foo bar foo baz foo");
        // Move to the third 'foo' then #.
        e.textarea.move_cursor(CursorMove::Jump(0, 16));
        run_keys(&mut e, "#");
        assert_eq!(e.textarea.cursor().1, 8);
    }

    // ─── VisualLine delete / change ───────────────────────────────────────

    #[test]
    fn visual_line_delete_removes_full_lines() {
        let mut e = editor_with("a\nb\nc\nd");
        run_keys(&mut e, "Vjd");
        assert_eq!(e.textarea.lines(), &["c".to_string(), "d".to_string()]);
    }

    #[test]
    fn visual_line_change_leaves_blank_line() {
        let mut e = editor_with("a\nb\nc");
        run_keys(&mut e, "Vjc");
        assert_eq!(e.vim_mode(), VimMode::Insert);
        run_keys(&mut e, "X<Esc>");
        // `Vjc` wipes rows 0-1's contents and leaves a blank line in
        // their place (vim convention). Typing `X` lands on that blank
        // first line.
        assert_eq!(e.textarea.lines(), &["X".to_string(), "c".to_string()]);
    }

    #[test]
    fn cc_leaves_blank_line() {
        let mut e = editor_with("a\nb\nc");
        e.textarea.move_cursor(CursorMove::Jump(1, 0));
        run_keys(&mut e, "ccX<Esc>");
        assert_eq!(
            e.textarea.lines(),
            &["a".to_string(), "X".to_string(), "c".to_string()]
        );
    }

    // ─── Scrolling ────────────────────────────────────────────────────────

    // ─── WORD motions (W/B/E) ─────────────────────────────────────────────

    #[test]
    fn big_w_skips_hyphens() {
        // `w` stops at `-`; `W` treats the whole `foo-bar` as one WORD.
        let mut e = editor_with("foo-bar baz");
        run_keys(&mut e, "W");
        assert_eq!(e.textarea.cursor().1, 8);
    }

    #[test]
    fn big_w_crosses_lines() {
        let mut e = editor_with("foo-bar\nbaz-qux");
        run_keys(&mut e, "W");
        assert_eq!(e.textarea.cursor(), (1, 0));
    }

    #[test]
    fn big_b_skips_hyphens() {
        let mut e = editor_with("foo-bar baz");
        e.textarea.move_cursor(CursorMove::Jump(0, 9));
        run_keys(&mut e, "B");
        assert_eq!(e.textarea.cursor().1, 8);
        run_keys(&mut e, "B");
        assert_eq!(e.textarea.cursor().1, 0);
    }

    #[test]
    fn big_e_jumps_to_big_word_end() {
        let mut e = editor_with("foo-bar baz");
        run_keys(&mut e, "E");
        assert_eq!(e.textarea.cursor().1, 6);
        run_keys(&mut e, "E");
        assert_eq!(e.textarea.cursor().1, 10);
    }

    #[test]
    fn dw_with_big_word_variant() {
        // `dW` uses the WORD motion, so `foo-bar` deletes as a unit.
        let mut e = editor_with("foo-bar baz");
        run_keys(&mut e, "dW");
        assert_eq!(e.textarea.lines()[0], "baz");
    }

    // ─── Insert-mode Ctrl shortcuts ──────────────────────────────────────

    #[test]
    fn insert_ctrl_w_deletes_word_back() {
        let mut e = editor_with("");
        run_keys(&mut e, "i");
        for c in "hello world".chars() {
            e.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        run_keys(&mut e, "<C-w>");
        assert_eq!(e.textarea.lines()[0], "hello ");
    }

    #[test]
    fn insert_ctrl_u_deletes_to_line_start() {
        let mut e = editor_with("");
        run_keys(&mut e, "i");
        for c in "hello world".chars() {
            e.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        run_keys(&mut e, "<C-u>");
        assert_eq!(e.textarea.lines()[0], "");
    }

    #[test]
    fn insert_ctrl_o_runs_one_normal_command() {
        let mut e = editor_with("hello world");
        // Enter insert, then Ctrl-o dw (delete a word while in insert).
        run_keys(&mut e, "A");
        assert_eq!(e.vim_mode(), VimMode::Insert);
        // Move cursor back to start of "hello" for the Ctrl-o dw.
        e.textarea.move_cursor(CursorMove::Jump(0, 0));
        run_keys(&mut e, "<C-o>");
        assert_eq!(e.vim_mode(), VimMode::Normal);
        run_keys(&mut e, "dw");
        // After the command completes, back in insert.
        assert_eq!(e.vim_mode(), VimMode::Insert);
        assert_eq!(e.textarea.lines()[0], "world");
    }

    // ─── Sticky column across vertical motion ────────────────────────────

    #[test]
    fn j_through_empty_line_preserves_column() {
        let mut e = editor_with("hello world\n\nanother line");
        // Park cursor at col 6 on row 0.
        run_keys(&mut e, "llllll");
        assert_eq!(e.textarea.cursor(), (0, 6));
        // j into the empty line — cursor clamps to (1, 0) visually, but
        // sticky col stays at 6.
        run_keys(&mut e, "j");
        assert_eq!(e.textarea.cursor(), (1, 0));
        // j onto a longer row — sticky col restores us to col 6.
        run_keys(&mut e, "j");
        assert_eq!(e.textarea.cursor(), (2, 6));
    }

    #[test]
    fn j_through_shorter_line_preserves_column() {
        let mut e = editor_with("hello world\nhi\nanother line");
        run_keys(&mut e, "lllllll"); // col 7
        run_keys(&mut e, "j"); // short line — clamps to col 1
        assert_eq!(e.textarea.cursor(), (1, 1));
        run_keys(&mut e, "j");
        assert_eq!(e.textarea.cursor(), (2, 7));
    }

    #[test]
    fn esc_from_insert_sticky_matches_visible_cursor() {
        // Cursor at col 12, I (moves to col 4), type "X" (col 5), Esc
        // backs to col 4 — sticky must mirror that visible col so j
        // lands at col 4 of the next row, not col 5 or col 12.
        let mut e = editor_with("    this is a line\n    another one of a similar size");
        e.textarea.move_cursor(CursorMove::Jump(0, 12));
        run_keys(&mut e, "I");
        assert_eq!(e.textarea.cursor(), (0, 4));
        run_keys(&mut e, "X<Esc>");
        assert_eq!(e.textarea.cursor(), (0, 4));
        run_keys(&mut e, "j");
        assert_eq!(e.textarea.cursor(), (1, 4));
    }

    #[test]
    fn esc_from_insert_sticky_tracks_inserted_chars() {
        let mut e = editor_with("xxxxxxx\nyyyyyyy");
        run_keys(&mut e, "i");
        run_keys(&mut e, "abc<Esc>");
        assert_eq!(e.textarea.cursor(), (0, 2));
        run_keys(&mut e, "j");
        assert_eq!(e.textarea.cursor(), (1, 2));
    }

    #[test]
    fn esc_from_insert_sticky_tracks_arrow_nav() {
        let mut e = editor_with("xxxxxx\nyyyyyy");
        run_keys(&mut e, "i");
        run_keys(&mut e, "abc");
        for _ in 0..2 {
            e.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        }
        run_keys(&mut e, "<Esc>");
        assert_eq!(e.textarea.cursor(), (0, 0));
        run_keys(&mut e, "j");
        assert_eq!(e.textarea.cursor(), (1, 0));
    }

    #[test]
    fn esc_from_insert_at_col_14_followed_by_j() {
        // User-reported regression: cursor at col 14, i, type "test "
        // (5 chars → col 19), Esc → col 18. j must land at col 18.
        let line = "x".repeat(30);
        let buf = format!("{line}\n{line}");
        let mut e = editor_with(&buf);
        e.textarea.move_cursor(CursorMove::Jump(0, 14));
        run_keys(&mut e, "i");
        for c in "test ".chars() {
            e.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        run_keys(&mut e, "<Esc>");
        assert_eq!(e.textarea.cursor(), (0, 18));
        run_keys(&mut e, "j");
        assert_eq!(e.textarea.cursor(), (1, 18));
    }

    #[test]
    fn linewise_paste_resets_sticky_column() {
        // yy then p lands the cursor on the first non-blank of the
        // pasted line; the next j must not drag back to the old
        // sticky column.
        let mut e = editor_with("    hello\naaaaaaaa\nbye");
        run_keys(&mut e, "llllll"); // col 6, sticky = 6
        run_keys(&mut e, "yy");
        run_keys(&mut e, "j"); // into row 1 col 6
        run_keys(&mut e, "p"); // paste below row 1 — cursor on "    hello"
        // Cursor should be at (2, 4) — first non-blank of the pasted line.
        assert_eq!(e.textarea.cursor(), (2, 4));
        // j should then preserve col 4, not jump back to 6.
        run_keys(&mut e, "j");
        assert_eq!(e.textarea.cursor(), (3, 2));
    }

    #[test]
    fn horizontal_motion_resyncs_sticky_column() {
        // Starting col 6 on row 0, go back to col 3, then down through
        // an empty row. The sticky col should be 3 (from the last `h`
        // sequence), not 6.
        let mut e = editor_with("hello world\n\nanother line");
        run_keys(&mut e, "llllll"); // col 6
        run_keys(&mut e, "hhh"); // col 3
        run_keys(&mut e, "jj");
        assert_eq!(e.textarea.cursor(), (2, 3));
    }

    // ─── Visual block ────────────────────────────────────────────────────

    #[test]
    fn ctrl_v_enters_visual_block() {
        let mut e = editor_with("aaa\nbbb\nccc");
        run_keys(&mut e, "<C-v>");
        assert_eq!(e.vim_mode(), VimMode::VisualBlock);
    }

    #[test]
    fn visual_block_esc_returns_to_normal() {
        let mut e = editor_with("aaa\nbbb\nccc");
        run_keys(&mut e, "<C-v>");
        run_keys(&mut e, "<Esc>");
        assert_eq!(e.vim_mode(), VimMode::Normal);
    }

    #[test]
    fn visual_block_delete_removes_column_range() {
        let mut e = editor_with("hello\nworld\nhappy");
        // Move off col 0 first so the block starts mid-row.
        run_keys(&mut e, "l");
        run_keys(&mut e, "<C-v>");
        run_keys(&mut e, "jj");
        run_keys(&mut e, "ll");
        run_keys(&mut e, "d");
        // Deletes cols 1-3 on every row — "ell" / "orl" / "app".
        assert_eq!(
            e.textarea.lines(),
            &["ho".to_string(), "wd".to_string(), "hy".to_string()]
        );
    }

    #[test]
    fn visual_block_yank_joins_with_newlines() {
        let mut e = editor_with("hello\nworld\nhappy");
        run_keys(&mut e, "<C-v>");
        run_keys(&mut e, "jj");
        run_keys(&mut e, "ll");
        run_keys(&mut e, "y");
        assert_eq!(e.last_yank.as_deref(), Some("hel\nwor\nhap"));
    }

    #[test]
    fn visual_block_replace_fills_block() {
        let mut e = editor_with("hello\nworld\nhappy");
        run_keys(&mut e, "<C-v>");
        run_keys(&mut e, "jj");
        run_keys(&mut e, "ll");
        run_keys(&mut e, "rx");
        assert_eq!(
            e.textarea.lines(),
            &[
                "xxxlo".to_string(),
                "xxxld".to_string(),
                "xxxpy".to_string()
            ]
        );
    }

    #[test]
    fn visual_block_insert_repeats_across_rows() {
        let mut e = editor_with("hello\nworld\nhappy");
        run_keys(&mut e, "<C-v>");
        run_keys(&mut e, "jj");
        run_keys(&mut e, "I");
        run_keys(&mut e, "# <Esc>");
        assert_eq!(
            e.textarea.lines(),
            &[
                "# hello".to_string(),
                "# world".to_string(),
                "# happy".to_string()
            ]
        );
    }

    #[test]
    fn block_highlight_returns_none_outside_block_mode() {
        let mut e = editor_with("abc");
        assert!(e.block_highlight().is_none());
        run_keys(&mut e, "v");
        assert!(e.block_highlight().is_none());
        run_keys(&mut e, "<Esc>V");
        assert!(e.block_highlight().is_none());
    }

    #[test]
    fn block_highlight_bounds_track_anchor_and_cursor() {
        let mut e = editor_with("aaaa\nbbbb\ncccc");
        run_keys(&mut e, "ll"); // cursor (0, 2)
        run_keys(&mut e, "<C-v>");
        run_keys(&mut e, "jh"); // cursor (1, 1)
        // anchor = (0, 2), cursor = (1, 1) → top=0 bot=1 left=1 right=2.
        assert_eq!(e.block_highlight(), Some((0, 1, 1, 2)));
    }

    #[test]
    fn visual_block_delete_handles_short_lines() {
        // Middle row is shorter than the block's right column.
        let mut e = editor_with("hello\nhi\nworld");
        run_keys(&mut e, "l"); // col 1
        run_keys(&mut e, "<C-v>");
        run_keys(&mut e, "jjll"); // cursor (2, 3)
        run_keys(&mut e, "d");
        // Row 0: delete cols 1-3 ("ell") → "ho".
        // Row 1: only 2 chars ("hi"); block starts at col 1, so just "i"
        //        gets removed → "h".
        // Row 2: delete cols 1-3 ("orl") → "wd".
        assert_eq!(
            e.textarea.lines(),
            &["ho".to_string(), "h".to_string(), "wd".to_string()]
        );
    }

    #[test]
    fn visual_block_yank_pads_short_lines_with_empties() {
        let mut e = editor_with("hello\nhi\nworld");
        run_keys(&mut e, "l");
        run_keys(&mut e, "<C-v>");
        run_keys(&mut e, "jjll");
        run_keys(&mut e, "y");
        // Row 0 chars 1-3 = "ell"; row 1 chars 1- (only "i"); row 2 "orl".
        assert_eq!(e.last_yank.as_deref(), Some("ell\ni\norl"));
    }

    #[test]
    fn visual_block_replace_skips_past_eol() {
        // Block extends past the end of every row in column range;
        // replace should leave lines shorter than `left` untouched.
        let mut e = editor_with("ab\ncd\nef");
        // Put cursor at col 1 (last char), extend block 5 columns right.
        run_keys(&mut e, "l");
        run_keys(&mut e, "<C-v>");
        run_keys(&mut e, "jjllllll");
        run_keys(&mut e, "rX");
        // Every row had only col 0..=1; block covers col 1..=7 → only
        // col 1 is in range on each row, so just that cell changes.
        assert_eq!(
            e.textarea.lines(),
            &["aX".to_string(), "cX".to_string(), "eX".to_string()]
        );
    }

    #[test]
    fn visual_block_with_empty_line_in_middle() {
        let mut e = editor_with("abcd\n\nefgh");
        run_keys(&mut e, "<C-v>");
        run_keys(&mut e, "jjll"); // cursor (2, 2)
        run_keys(&mut e, "d");
        // Row 0 cols 0-2 removed → "d". Row 1 empty → untouched.
        // Row 2 cols 0-2 removed → "h".
        assert_eq!(
            e.textarea.lines(),
            &["d".to_string(), "".to_string(), "h".to_string()]
        );
    }

    #[test]
    fn block_insert_pads_empty_lines_to_block_column() {
        // Middle line is empty; block I at column 3 should pad the empty
        // line with spaces so the inserted text lines up.
        let mut e = editor_with("this is a line\n\nthis is a line");
        e.textarea.move_cursor(CursorMove::Jump(0, 3));
        run_keys(&mut e, "<C-v>");
        run_keys(&mut e, "jj");
        run_keys(&mut e, "I");
        run_keys(&mut e, "XX<Esc>");
        assert_eq!(
            e.textarea.lines(),
            &[
                "thiXXs is a line".to_string(),
                "   XX".to_string(),
                "thiXXs is a line".to_string()
            ]
        );
    }

    #[test]
    fn block_insert_pads_short_lines_to_block_column() {
        let mut e = editor_with("aaaaa\nbb\naaaaa");
        e.textarea.move_cursor(CursorMove::Jump(0, 3));
        run_keys(&mut e, "<C-v>");
        run_keys(&mut e, "jj");
        run_keys(&mut e, "I");
        run_keys(&mut e, "Y<Esc>");
        // Row 1 "bb" is shorter than col 3 — pad with one space then Y.
        assert_eq!(
            e.textarea.lines(),
            &[
                "aaaYaa".to_string(),
                "bb Y".to_string(),
                "aaaYaa".to_string()
            ]
        );
    }

    #[test]
    fn visual_block_append_repeats_across_rows() {
        let mut e = editor_with("foo\nbar\nbaz");
        run_keys(&mut e, "<C-v>");
        run_keys(&mut e, "jj");
        // Single-column block (anchor col = cursor col = 0); `A` appends
        // after column 0 on every row.
        run_keys(&mut e, "A");
        run_keys(&mut e, "!<Esc>");
        assert_eq!(
            e.textarea.lines(),
            &["f!oo".to_string(), "b!ar".to_string(), "b!az".to_string()]
        );
    }

    // ─── `/` / `?` search prompt ─────────────────────────────────────────

    #[test]
    fn slash_opens_forward_search_prompt() {
        let mut e = editor_with("hello world");
        run_keys(&mut e, "/");
        let p = e.search_prompt().expect("prompt should be active");
        assert!(p.text.is_empty());
        assert!(p.forward);
    }

    #[test]
    fn question_opens_backward_search_prompt() {
        let mut e = editor_with("hello world");
        run_keys(&mut e, "?");
        let p = e.search_prompt().expect("prompt should be active");
        assert!(!p.forward);
    }

    #[test]
    fn search_prompt_typing_updates_pattern_live() {
        let mut e = editor_with("foo bar\nbaz");
        run_keys(&mut e, "/bar");
        assert_eq!(e.search_prompt().unwrap().text, "bar");
        // Pattern set on textarea for live highlight.
        assert!(e.textarea.search_pattern().is_some());
    }

    #[test]
    fn search_prompt_backspace_and_enter() {
        let mut e = editor_with("hello world\nagain");
        run_keys(&mut e, "/worlx");
        e.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(e.search_prompt().unwrap().text, "worl");
        e.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // Prompt closed, last_search set, cursor advanced to match.
        assert!(e.search_prompt().is_none());
        assert_eq!(e.last_search(), Some("worl"));
        assert_eq!(e.textarea.cursor(), (0, 6));
    }

    #[test]
    fn search_prompt_esc_cancels_but_keeps_last_search() {
        let mut e = editor_with("foo bar\nbaz");
        run_keys(&mut e, "/bar");
        e.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(e.search_prompt().is_none());
        assert_eq!(e.last_search(), Some("bar"));
    }

    #[test]
    fn search_then_n_and_shift_n_navigate() {
        let mut e = editor_with("foo bar foo baz foo");
        run_keys(&mut e, "/foo");
        e.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // `/foo` + Enter jumps forward; we land on the next match after col 0.
        assert_eq!(e.textarea.cursor().1, 8);
        run_keys(&mut e, "n");
        assert_eq!(e.textarea.cursor().1, 16);
        run_keys(&mut e, "N");
        assert_eq!(e.textarea.cursor().1, 8);
    }

    #[test]
    fn question_mark_searches_backward_on_enter() {
        let mut e = editor_with("foo bar foo baz");
        e.textarea.move_cursor(CursorMove::Jump(0, 10));
        run_keys(&mut e, "?foo");
        e.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // Cursor jumps backward to the closest match before col 10.
        assert_eq!(e.textarea.cursor(), (0, 8));
    }

    // ─── P6 quick wins (Y, gJ, ge / gE) ──────────────────────────────────

    #[test]
    fn big_y_yanks_to_end_of_line() {
        let mut e = editor_with("hello world");
        e.textarea.move_cursor(CursorMove::Jump(0, 6));
        run_keys(&mut e, "Y");
        assert_eq!(e.last_yank.as_deref(), Some("world"));
    }

    #[test]
    fn big_y_from_line_start_yanks_full_line() {
        let mut e = editor_with("hello world");
        run_keys(&mut e, "Y");
        assert_eq!(e.last_yank.as_deref(), Some("hello world"));
    }

    #[test]
    fn gj_joins_without_inserting_space() {
        let mut e = editor_with("hello\n    world");
        run_keys(&mut e, "gJ");
        // No space inserted, leading whitespace preserved.
        assert_eq!(e.textarea.lines(), &["hello    world".to_string()]);
    }

    #[test]
    fn gj_noop_on_last_line() {
        let mut e = editor_with("only");
        run_keys(&mut e, "gJ");
        assert_eq!(e.textarea.lines(), &["only".to_string()]);
    }

    #[test]
    fn ge_jumps_to_previous_word_end() {
        let mut e = editor_with("foo bar baz");
        e.textarea.move_cursor(CursorMove::Jump(0, 5));
        run_keys(&mut e, "ge");
        assert_eq!(e.textarea.cursor(), (0, 2));
    }

    #[test]
    fn ge_respects_word_class() {
        // Small-word `ge` treats `-` as its own word, so from mid-"bar"
        // it lands on the `-` rather than end of "foo".
        let mut e = editor_with("foo-bar baz");
        e.textarea.move_cursor(CursorMove::Jump(0, 5));
        run_keys(&mut e, "ge");
        assert_eq!(e.textarea.cursor(), (0, 3));
    }

    #[test]
    fn big_ge_treats_hyphens_as_part_of_word() {
        // `gE` uses WORD (whitespace-delimited) semantics so it skips
        // over the `-` and lands on the end of "foo-bar".
        let mut e = editor_with("foo-bar baz");
        e.textarea.move_cursor(CursorMove::Jump(0, 10));
        run_keys(&mut e, "gE");
        assert_eq!(e.textarea.cursor(), (0, 6));
    }

    #[test]
    fn ge_crosses_line_boundary() {
        let mut e = editor_with("foo\nbar");
        e.textarea.move_cursor(CursorMove::Jump(1, 0));
        run_keys(&mut e, "ge");
        assert_eq!(e.textarea.cursor(), (0, 2));
    }

    #[test]
    fn dge_deletes_to_end_of_previous_word() {
        let mut e = editor_with("foo bar baz");
        e.textarea.move_cursor(CursorMove::Jump(0, 8));
        // d + ge from 'b' of "baz": range is ge → col 6 ('r' of bar),
        // inclusive, so cols 6-8 ("r b") are cut.
        run_keys(&mut e, "dge");
        assert_eq!(e.textarea.lines()[0], "foo baaz");
    }

    #[test]
    fn ctrl_scroll_keys_do_not_panic() {
        // Viewport-less test: just exercise the code paths so a regression
        // in the scroll dispatch surfaces as a panic or assertion failure.
        let mut e = editor_with(
            (0..50)
                .map(|i| format!("line{i}"))
                .collect::<Vec<_>>()
                .join("\n")
                .as_str(),
        );
        run_keys(&mut e, "<C-f>");
        run_keys(&mut e, "<C-b>");
        // No explicit assert beyond "didn't panic".
        assert!(!e.textarea.lines().is_empty());
    }

    /// Regression: arrow-navigation during a count-insert session must
    /// not pull unrelated rows into the "inserted" replay string.
    /// Before the fix, `before_lines` only snapshotted the entry row,
    /// so the diff at Esc spuriously saw the navigated-over row as
    /// part of the insert — count-replay then duplicated cross-row
    /// content across the buffer.
    #[test]
    fn count_insert_with_arrow_nav_does_not_leak_rows() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("row0\nrow1\nrow2");
        // `3i`, type X, arrow down, Esc.
        run_keys(&mut e, "3iX<Down><Esc>");
        // Row 0 keeps the originally-typed X.
        assert!(e.textarea.lines()[0].contains('X'));
        // Row 1 must not contain a fragment of row 0 ("row0") — that
        // was the buggy leak from the before-diff window.
        assert!(
            !e.textarea.lines()[1].contains("row0"),
            "row1 leaked row0 contents: {:?}",
            e.textarea.lines()[1]
        );
        // Buffer stays the same number of rows — no extra lines
        // injected by a multi-line "inserted" replay.
        assert_eq!(e.textarea.lines().len(), 3);
    }

    // ─── Viewport scroll / jump tests ─────────────────────────────────

    fn editor_with_rows(n: usize, viewport: u16) -> Editor<'static> {
        let mut e = Editor::new(KeybindingMode::Vim);
        let body = (0..n)
            .map(|i| format!("  line{}", i))
            .collect::<Vec<_>>()
            .join("\n");
        e.set_content(&body);
        e.set_viewport_height(viewport);
        e
    }

    #[test]
    fn ctrl_d_moves_cursor_half_page_down() {
        let mut e = editor_with_rows(100, 20);
        run_keys(&mut e, "<C-d>");
        assert_eq!(e.textarea.cursor().0, 10);
    }

    #[test]
    fn ctrl_u_moves_cursor_half_page_up() {
        let mut e = editor_with_rows(100, 20);
        e.textarea.move_cursor(CursorMove::Jump(50, 0));
        run_keys(&mut e, "<C-u>");
        assert_eq!(e.textarea.cursor().0, 40);
    }

    #[test]
    fn ctrl_f_moves_cursor_full_page_down() {
        let mut e = editor_with_rows(100, 20);
        run_keys(&mut e, "<C-f>");
        // One full page ≈ h - 2 (overlap).
        assert_eq!(e.textarea.cursor().0, 18);
    }

    #[test]
    fn ctrl_b_moves_cursor_full_page_up() {
        let mut e = editor_with_rows(100, 20);
        e.textarea.move_cursor(CursorMove::Jump(50, 0));
        run_keys(&mut e, "<C-b>");
        assert_eq!(e.textarea.cursor().0, 32);
    }

    #[test]
    fn ctrl_d_lands_on_first_non_blank() {
        let mut e = editor_with_rows(100, 20);
        run_keys(&mut e, "<C-d>");
        // "  line10" — first non-blank is col 2.
        assert_eq!(e.textarea.cursor().1, 2);
    }

    #[test]
    fn ctrl_d_clamps_at_end_of_buffer() {
        let mut e = editor_with_rows(5, 20);
        run_keys(&mut e, "<C-d>");
        assert_eq!(e.textarea.cursor().0, 4);
    }

    #[test]
    fn capital_h_jumps_to_viewport_top() {
        let mut e = editor_with_rows(100, 10);
        e.textarea.move_cursor(CursorMove::Jump(50, 0));
        e.textarea
            .scroll(tui_textarea::Scrolling::Delta { rows: 45, cols: 0 });
        let top = e.textarea.viewport_top_row();
        run_keys(&mut e, "H");
        assert_eq!(e.textarea.cursor().0, top);
        assert_eq!(e.textarea.cursor().1, 2);
    }

    #[test]
    fn capital_l_jumps_to_viewport_bottom() {
        let mut e = editor_with_rows(100, 10);
        e.textarea.move_cursor(CursorMove::Jump(50, 0));
        e.textarea
            .scroll(tui_textarea::Scrolling::Delta { rows: 45, cols: 0 });
        let top = e.textarea.viewport_top_row();
        run_keys(&mut e, "L");
        assert_eq!(e.textarea.cursor().0, top + 9);
    }

    #[test]
    fn capital_m_jumps_to_viewport_middle() {
        let mut e = editor_with_rows(100, 10);
        e.textarea.move_cursor(CursorMove::Jump(50, 0));
        e.textarea
            .scroll(tui_textarea::Scrolling::Delta { rows: 45, cols: 0 });
        let top = e.textarea.viewport_top_row();
        run_keys(&mut e, "M");
        // 10-row viewport: middle is top + 4.
        assert_eq!(e.textarea.cursor().0, top + 4);
    }

    #[test]
    fn capital_h_count_offsets_from_top() {
        let mut e = editor_with_rows(100, 10);
        e.textarea.move_cursor(CursorMove::Jump(50, 0));
        e.textarea
            .scroll(tui_textarea::Scrolling::Delta { rows: 45, cols: 0 });
        let top = e.textarea.viewport_top_row();
        run_keys(&mut e, "3H");
        assert_eq!(e.textarea.cursor().0, top + 2);
    }

    // ─── Jumplist tests ───────────────────────────────────────────────

    #[test]
    fn ctrl_o_returns_to_pre_g_position() {
        let mut e = editor_with_rows(50, 20);
        e.textarea.move_cursor(CursorMove::Jump(5, 2));
        run_keys(&mut e, "G");
        assert_eq!(e.textarea.cursor().0, 49);
        run_keys(&mut e, "<C-o>");
        assert_eq!(e.textarea.cursor(), (5, 2));
    }

    #[test]
    fn ctrl_i_redoes_jump_after_ctrl_o() {
        let mut e = editor_with_rows(50, 20);
        e.textarea.move_cursor(CursorMove::Jump(5, 2));
        run_keys(&mut e, "G");
        let post = e.textarea.cursor();
        run_keys(&mut e, "<C-o>");
        run_keys(&mut e, "<C-i>");
        assert_eq!(e.textarea.cursor(), post);
    }

    #[test]
    fn new_jump_clears_forward_stack() {
        let mut e = editor_with_rows(50, 20);
        e.textarea.move_cursor(CursorMove::Jump(5, 2));
        run_keys(&mut e, "G");
        run_keys(&mut e, "<C-o>");
        run_keys(&mut e, "gg");
        run_keys(&mut e, "<C-i>");
        assert_eq!(e.textarea.cursor().0, 0);
    }

    #[test]
    fn ctrl_o_on_empty_stack_is_noop() {
        let mut e = editor_with_rows(10, 20);
        e.textarea.move_cursor(CursorMove::Jump(3, 1));
        run_keys(&mut e, "<C-o>");
        assert_eq!(e.textarea.cursor(), (3, 1));
    }

    #[test]
    fn asterisk_search_pushes_jump() {
        let mut e = editor_with("foo bar\nbaz foo end");
        e.textarea.move_cursor(CursorMove::Jump(0, 0));
        run_keys(&mut e, "*");
        let after = e.textarea.cursor();
        assert_ne!(after, (0, 0));
        run_keys(&mut e, "<C-o>");
        assert_eq!(e.textarea.cursor(), (0, 0));
    }

    #[test]
    fn h_viewport_jump_is_recorded() {
        let mut e = editor_with_rows(100, 10);
        e.textarea.move_cursor(CursorMove::Jump(50, 0));
        e.textarea
            .scroll(tui_textarea::Scrolling::Delta { rows: 45, cols: 0 });
        let pre = e.textarea.cursor();
        run_keys(&mut e, "H");
        assert_ne!(e.textarea.cursor(), pre);
        run_keys(&mut e, "<C-o>");
        assert_eq!(e.textarea.cursor(), pre);
    }

    #[test]
    fn j_k_motion_does_not_push_jump() {
        let mut e = editor_with_rows(50, 20);
        e.textarea.move_cursor(CursorMove::Jump(5, 0));
        run_keys(&mut e, "jjj");
        run_keys(&mut e, "<C-o>");
        assert_eq!(e.textarea.cursor().0, 8);
    }

    #[test]
    fn jumplist_caps_at_100() {
        let mut e = editor_with_rows(200, 20);
        for i in 0..101 {
            e.textarea.move_cursor(CursorMove::Jump(i, 0));
            run_keys(&mut e, "G");
        }
        assert!(e.vim.jump_back.len() <= 100);
    }

    #[test]
    fn tab_acts_as_ctrl_i() {
        let mut e = editor_with_rows(50, 20);
        e.textarea.move_cursor(CursorMove::Jump(5, 2));
        run_keys(&mut e, "G");
        let post = e.textarea.cursor();
        run_keys(&mut e, "<C-o>");
        e.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(e.textarea.cursor(), post);
    }

    // ─── Mark tests ───────────────────────────────────────────────────

    #[test]
    fn ma_then_backtick_a_jumps_exact() {
        let mut e = editor_with_rows(50, 20);
        e.textarea.move_cursor(CursorMove::Jump(5, 3));
        run_keys(&mut e, "ma");
        e.textarea.move_cursor(CursorMove::Jump(20, 0));
        run_keys(&mut e, "`a");
        assert_eq!(e.textarea.cursor(), (5, 3));
    }

    #[test]
    fn ma_then_apostrophe_a_lands_on_first_non_blank() {
        let mut e = editor_with_rows(50, 20);
        // "  line5" — first non-blank is col 2.
        e.textarea.move_cursor(CursorMove::Jump(5, 6));
        run_keys(&mut e, "ma");
        e.textarea.move_cursor(CursorMove::Jump(30, 4));
        run_keys(&mut e, "'a");
        assert_eq!(e.textarea.cursor(), (5, 2));
    }

    #[test]
    fn goto_mark_pushes_jumplist() {
        let mut e = editor_with_rows(50, 20);
        e.textarea.move_cursor(CursorMove::Jump(10, 2));
        run_keys(&mut e, "mz");
        e.textarea.move_cursor(CursorMove::Jump(3, 0));
        run_keys(&mut e, "`z");
        assert_eq!(e.textarea.cursor(), (10, 2));
        run_keys(&mut e, "<C-o>");
        assert_eq!(e.textarea.cursor(), (3, 0));
    }

    #[test]
    fn goto_missing_mark_is_noop() {
        let mut e = editor_with_rows(50, 20);
        e.textarea.move_cursor(CursorMove::Jump(3, 1));
        run_keys(&mut e, "`q");
        assert_eq!(e.textarea.cursor(), (3, 1));
    }

    #[test]
    fn uppercase_mark_letter_ignored() {
        let mut e = editor_with_rows(50, 20);
        e.textarea.move_cursor(CursorMove::Jump(5, 3));
        run_keys(&mut e, "mA");
        // Uppercase marks aren't supported — entry bailed, nothing
        // stored under 'a' or 'A'.
        assert!(e.vim.marks.is_empty());
    }

    #[test]
    fn mark_survives_document_shrink_via_clamp() {
        let mut e = editor_with_rows(50, 20);
        e.textarea.move_cursor(CursorMove::Jump(40, 4));
        run_keys(&mut e, "mx");
        // Shrink the buffer to 10 rows.
        e.set_content("a\nb\nc\nd\ne");
        run_keys(&mut e, "`x");
        // Mark clamped to last row, col 0 (short line).
        let (r, _) = e.textarea.cursor();
        assert!(r <= 4);
    }

    // ─── Search / jumplist interaction ───────────────────────────────

    #[test]
    fn forward_search_commit_pushes_jump() {
        let mut e = editor_with("alpha beta\nfoo target end\nmore");
        e.textarea.move_cursor(CursorMove::Jump(0, 0));
        run_keys(&mut e, "/target<CR>");
        // Cursor moved to the match.
        assert_ne!(e.textarea.cursor(), (0, 0));
        // Ctrl-o returns to the pre-search position.
        run_keys(&mut e, "<C-o>");
        assert_eq!(e.textarea.cursor(), (0, 0));
    }

    #[test]
    fn search_commit_no_match_does_not_push_jump() {
        let mut e = editor_with("alpha beta\nfoo end");
        e.textarea.move_cursor(CursorMove::Jump(0, 3));
        let pre_len = e.vim.jump_back.len();
        run_keys(&mut e, "/zzznotfound<CR>");
        // No match → cursor stays, jumplist shouldn't grow.
        assert_eq!(e.vim.jump_back.len(), pre_len);
    }

    // ─── Phase 7b: migration buffer cursor sync ──────────────────────

    #[test]
    fn buffer_cursor_mirrors_textarea_after_horizontal_motion() {
        let mut e = editor_with("hello world");
        run_keys(&mut e, "lll");
        let (row, col) = e.textarea.cursor();
        assert_eq!(e.buffer.cursor().row, row);
        assert_eq!(e.buffer.cursor().col, col);
    }

    #[test]
    fn buffer_cursor_mirrors_textarea_after_vertical_motion() {
        let mut e = editor_with("aaaa\nbbbb\ncccc");
        run_keys(&mut e, "jj");
        let (row, col) = e.textarea.cursor();
        assert_eq!(e.buffer.cursor().row, row);
        assert_eq!(e.buffer.cursor().col, col);
    }

    #[test]
    fn buffer_cursor_mirrors_textarea_after_word_motion() {
        let mut e = editor_with("foo bar baz");
        run_keys(&mut e, "ww");
        let (row, col) = e.textarea.cursor();
        assert_eq!(e.buffer.cursor().row, row);
        assert_eq!(e.buffer.cursor().col, col);
    }

    #[test]
    fn buffer_cursor_mirrors_textarea_after_jump_motion() {
        let mut e = editor_with("a\nb\nc\nd\ne");
        run_keys(&mut e, "G");
        let (row, col) = e.textarea.cursor();
        assert_eq!(e.buffer.cursor().row, row);
        assert_eq!(e.buffer.cursor().col, col);
    }

    #[test]
    fn buffer_sticky_col_mirrors_vim_state() {
        let mut e = editor_with("longline\nhi\nlongline");
        run_keys(&mut e, "fl");
        run_keys(&mut e, "j");
        // Sticky col should be set; buffer carries the same value.
        assert_eq!(e.buffer.sticky_col(), e.vim.sticky_col);
    }

    #[test]
    fn buffer_content_mirrors_textarea_after_insert() {
        let mut e = editor_with("hello");
        run_keys(&mut e, "iXYZ<Esc>");
        let text = e.textarea.lines().join("\n");
        assert_eq!(e.buffer.as_string(), text);
    }

    #[test]
    fn buffer_content_mirrors_textarea_after_delete() {
        let mut e = editor_with("alpha bravo charlie");
        run_keys(&mut e, "dw");
        let text = e.textarea.lines().join("\n");
        assert_eq!(e.buffer.as_string(), text);
    }

    #[test]
    fn buffer_content_mirrors_textarea_after_dd() {
        let mut e = editor_with("a\nb\nc\nd");
        run_keys(&mut e, "jdd");
        let text = e.textarea.lines().join("\n");
        assert_eq!(e.buffer.as_string(), text);
    }

    #[test]
    fn buffer_content_mirrors_textarea_after_open_line() {
        let mut e = editor_with("foo\nbar");
        run_keys(&mut e, "oNEW<Esc>");
        let text = e.textarea.lines().join("\n");
        assert_eq!(e.buffer.as_string(), text);
    }

    #[test]
    fn buffer_content_mirrors_textarea_after_paste() {
        let mut e = editor_with("hello");
        run_keys(&mut e, "yy");
        run_keys(&mut e, "p");
        let text = e.textarea.lines().join("\n");
        assert_eq!(e.buffer.as_string(), text);
    }

    #[test]
    fn buffer_selection_none_in_normal_mode() {
        let e = editor_with("foo bar");
        assert!(e.buffer_selection().is_none());
    }

    #[test]
    fn buffer_selection_char_in_visual_mode() {
        use sqeel_buffer::{Position, Selection};
        let mut e = editor_with("hello world");
        run_keys(&mut e, "vlll");
        assert_eq!(
            e.buffer_selection(),
            Some(Selection::Char {
                anchor: Position::new(0, 0),
                head: Position::new(0, 3),
            })
        );
    }

    #[test]
    fn buffer_selection_line_in_visual_line_mode() {
        use sqeel_buffer::Selection;
        let mut e = editor_with("a\nb\nc\nd");
        run_keys(&mut e, "Vj");
        assert_eq!(
            e.buffer_selection(),
            Some(Selection::Line {
                anchor_row: 0,
                head_row: 1,
            })
        );
    }

    #[test]
    fn intern_style_dedups_repeated_styles() {
        use ratatui::style::{Color, Style};
        let mut e = editor_with("");
        let red = Style::default().fg(Color::Red);
        let blue = Style::default().fg(Color::Blue);
        let id_r1 = e.intern_style(red);
        let id_r2 = e.intern_style(red);
        let id_b = e.intern_style(blue);
        assert_eq!(id_r1, id_r2);
        assert_ne!(id_r1, id_b);
        assert_eq!(e.style_table().len(), 2);
    }

    #[test]
    fn sync_buffer_spans_translates_textarea_spans() {
        use ratatui::style::{Color, Style};
        let mut e = editor_with("SELECT foo");
        e.textarea
            .set_syntax_spans(vec![vec![(0, 6, Style::default().fg(Color::Red))]]);
        e.sync_buffer_spans_from_textarea();
        let by_row = e.buffer.spans();
        assert_eq!(by_row.len(), 1);
        assert_eq!(by_row[0].len(), 1);
        assert_eq!(by_row[0][0].start_byte, 0);
        assert_eq!(by_row[0][0].end_byte, 6);
        let id = by_row[0][0].style;
        assert_eq!(e.style_table()[id as usize].fg, Some(Color::Red));
    }

    #[test]
    fn sync_buffer_spans_clamps_sentinel_end() {
        use ratatui::style::{Color, Style};
        let mut e = editor_with("hello");
        e.textarea.set_syntax_spans(vec![vec![(
            0,
            usize::MAX,
            Style::default().fg(Color::Blue),
        )]]);
        e.sync_buffer_spans_from_textarea();
        let by_row = e.buffer.spans();
        assert_eq!(by_row[0][0].end_byte, 5);
    }

    #[test]
    fn sync_buffer_spans_drops_zero_width() {
        use ratatui::style::{Color, Style};
        let mut e = editor_with("abc");
        e.textarea
            .set_syntax_spans(vec![vec![(2, 2, Style::default().fg(Color::Red))]]);
        e.sync_buffer_spans_from_textarea();
        assert!(e.buffer.spans()[0].is_empty());
    }

    #[test]
    fn buffer_selection_block_in_visual_block_mode() {
        use sqeel_buffer::{Position, Selection};
        let mut e = editor_with("aaaa\nbbbb\ncccc");
        run_keys(&mut e, "<C-v>jl");
        assert_eq!(
            e.buffer_selection(),
            Some(Selection::Block {
                anchor: Position::new(0, 0),
                head: Position::new(1, 1),
            })
        );
    }
}
