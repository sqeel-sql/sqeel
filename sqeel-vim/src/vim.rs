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
use tui_textarea::{CursorMove, Input, Key, Scrolling};

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
}

// ─── Operator / Motion / TextObject ────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    Delete,
    Change,
    Yank,
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
    sticky_col: Option<usize>,
    /// Track whether the last yank/cut was linewise (drives `p`/`P` layout).
    pub(super) yank_linewise: bool,
    /// Set while replaying `.` / last-change so we don't re-record it.
    replaying: bool,
    /// Entered Normal from Insert via `Ctrl-o`; after the next complete
    /// normal-mode command we return to Insert.
    one_shot_normal: bool,
}

#[derive(Debug, Clone)]
struct InsertSession {
    count: usize,
    before: String,
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

pub fn step(ed: &mut Editor<'_>, input: Input) -> bool {
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
            _ => {}
        }
    }

    if ed.textarea.input(input) {
        ed.content_dirty = true;
    }
    true
}

fn finish_insert_session(ed: &mut Editor<'_>) {
    let Some(session) = ed.vim.insert_session.take() else {
        return;
    };
    let after = ed.textarea.lines().join("\n");
    let inserted = extract_inserted(&session.before, &after);
    // Replay the insert for `count - 1` more times.
    if !inserted.is_empty() && session.count > 1 && !ed.vim.replaying {
        for _ in 0..session.count - 1 {
            ed.mutate(|t| t.insert_str(&inserted));
        }
    }
    // VisualBlock `I` / `A` replay: apply the inserted text to every
    // other row in the block range. Not currently part of the dot-repeat
    // log — that's a polish item.
    if let InsertReason::BlockEdge { top, bot, col } = session.reason {
        if !inserted.is_empty() && top < bot && !ed.vim.replaying {
            for r in (top + 1)..=bot {
                let line_len = ed.textarea.lines()[r].chars().count();
                if col > line_len {
                    // Row is shorter than the block column — pad with
                    // spaces so the replayed text lines up with the
                    // anchor row, matching vim's block-insert behaviour
                    // on ragged / empty lines.
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
    ed.vim.insert_session = Some(InsertSession {
        count,
        before: ed.textarea.lines().join("\n"),
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

    // Ctrl-prefixed scrolling + misc.
    if input.ctrl
        && let Key::Char(c) = input.key
    {
        match c {
            'd' => {
                ed.textarea.scroll(Scrolling::HalfPageDown);
                return true;
            }
            'u' => {
                ed.textarea.scroll(Scrolling::HalfPageUp);
                return true;
            }
            'f' => {
                ed.textarea.scroll(Scrolling::PageDown);
                return true;
            }
            'b' => {
                ed.textarea.scroll(Scrolling::PageUp);
                return true;
            }
            'r' => {
                do_redo(ed);
                return true;
            }
            _ => {}
        }
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

    // Unknown key — swallow so it doesn't bubble into the TUI layer.
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
    let pre_col = ed.textarea.cursor().1;
    apply_motion_cursor(ed, &motion, count);
    apply_sticky_col(ed, &motion, pre_col);
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
    match motion {
        Motion::Left => {
            for _ in 0..count {
                ed.textarea.move_cursor(CursorMove::Back);
            }
        }
        Motion::Right => {
            for _ in 0..count {
                ed.textarea.move_cursor(CursorMove::Forward);
            }
        }
        Motion::Up => {
            for _ in 0..count {
                ed.textarea.move_cursor(CursorMove::Up);
            }
        }
        Motion::Down => {
            for _ in 0..count {
                ed.textarea.move_cursor(CursorMove::Down);
            }
        }
        Motion::WordFwd => {
            for _ in 0..count {
                ed.textarea.move_cursor(CursorMove::WordForward);
            }
        }
        Motion::WordBack => {
            for _ in 0..count {
                ed.textarea.move_cursor(CursorMove::WordBack);
            }
        }
        Motion::WordEnd => {
            for _ in 0..count {
                ed.textarea.move_cursor(CursorMove::WordEnd);
            }
        }
        Motion::BigWordFwd => {
            let (r, c) = big_word_fwd(ed, count);
            ed.textarea.move_cursor(CursorMove::Jump(r, c));
        }
        Motion::BigWordBack => {
            let (r, c) = big_word_back(ed, count);
            ed.textarea.move_cursor(CursorMove::Jump(r, c));
        }
        Motion::BigWordEnd => {
            let (r, c) = big_word_end(ed, count);
            ed.textarea.move_cursor(CursorMove::Jump(r, c));
        }
        Motion::WordEndBack => {
            let (r, c) = word_end_back(ed, false, count);
            ed.textarea.move_cursor(CursorMove::Jump(r, c));
        }
        Motion::BigWordEndBack => {
            let (r, c) = word_end_back(ed, true, count);
            ed.textarea.move_cursor(CursorMove::Jump(r, c));
        }
        Motion::LineStart => ed.textarea.move_cursor(CursorMove::Head),
        Motion::FirstNonBlank => move_first_non_whitespace(ed),
        Motion::LineEnd => {
            // Vim normal-mode `$` lands on the last char, not one past it.
            let (row, _) = ed.textarea.cursor();
            let len = ed.textarea.lines()[row].chars().count();
            let col = len.saturating_sub(1);
            ed.textarea.move_cursor(CursorMove::Jump(row, col));
        }
        Motion::FileTop => {
            // `count G` / `count gg` jumps to line `count`.
            if count > 1 {
                ed.textarea
                    .move_cursor(CursorMove::Jump(count.saturating_sub(1), 0));
            } else {
                ed.textarea.move_cursor(CursorMove::Top);
            }
        }
        Motion::FileBottom => {
            if count > 1 {
                ed.textarea
                    .move_cursor(CursorMove::Jump(count.saturating_sub(1), 0));
            } else {
                ed.textarea.move_cursor(CursorMove::Bottom);
            }
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
    }
}

/// True when `(row, col)` lands on whitespace — including the synthetic
/// "newline" position that sits at `col == chars().count()` for any row
/// that is followed by another row.
fn is_ws_at(lines: &[String], row: usize, col: usize) -> bool {
    let Some(line) = lines.get(row) else {
        return true;
    };
    let len = line.chars().count();
    // End of line always counts as whitespace for traversal — either it's
    // a literal newline (more rows follow) or the end of the buffer (so
    // motions stop there). Either way we don't want WORD traversal to
    // grab the sentinel position as part of a WORD.
    if col >= len {
        return true;
    }
    line.chars()
        .nth(col)
        .map(char::is_whitespace)
        .unwrap_or(true)
}

/// Advance one character forward, crossing into the next row when we
/// pass end of the current one. Returns false when we hit end-of-buffer.
fn step_forward(lines: &[String], row: &mut usize, col: &mut usize) -> bool {
    let len = lines[*row].chars().count();
    if *col < len {
        *col += 1;
        true
    } else if *row + 1 < lines.len() {
        *row += 1;
        *col = 0;
        true
    } else {
        false
    }
}

/// Step one character backward, wrapping to the previous row's trailing
/// "newline" position. Returns false at (0, 0).
fn step_back(lines: &[String], row: &mut usize, col: &mut usize) -> bool {
    if *col > 0 {
        *col -= 1;
        true
    } else if *row > 0 {
        *row -= 1;
        *col = lines[*row].chars().count();
        true
    } else {
        false
    }
}

/// `W` — WORD forward. A WORD is a maximal run of non-whitespace chars
/// (so `foo-bar` is one WORD, unlike `w` which stops at the `-`).
fn big_word_fwd(ed: &Editor<'_>, count: usize) -> (usize, usize) {
    let lines = ed.textarea.lines();
    let (mut row, mut col) = ed.textarea.cursor();
    for _ in 0..count.max(1) {
        while !is_ws_at(lines, row, col) {
            if !step_forward(lines, &mut row, &mut col) {
                return (row, col);
            }
        }
        while is_ws_at(lines, row, col) {
            if !step_forward(lines, &mut row, &mut col) {
                return (row, col);
            }
        }
    }
    (row, col)
}

/// `B` — WORD back.
fn big_word_back(ed: &Editor<'_>, count: usize) -> (usize, usize) {
    let lines = ed.textarea.lines();
    let (mut row, mut col) = ed.textarea.cursor();
    for _ in 0..count.max(1) {
        if !step_back(lines, &mut row, &mut col) {
            return (row, col);
        }
        while is_ws_at(lines, row, col) {
            if !step_back(lines, &mut row, &mut col) {
                return (row, col);
            }
        }
        loop {
            let (mut tr, mut tc) = (row, col);
            if !step_back(lines, &mut tr, &mut tc) {
                break;
            }
            if is_ws_at(lines, tr, tc) {
                break;
            }
            row = tr;
            col = tc;
        }
    }
    (row, col)
}

/// `E` — WORD end.
fn big_word_end(ed: &Editor<'_>, count: usize) -> (usize, usize) {
    let lines = ed.textarea.lines();
    let (mut row, mut col) = ed.textarea.cursor();
    for _ in 0..count.max(1) {
        if !step_forward(lines, &mut row, &mut col) {
            return (row, col);
        }
        while is_ws_at(lines, row, col) {
            if !step_forward(lines, &mut row, &mut col) {
                return (row, col);
            }
        }
        loop {
            let (mut tr, mut tc) = (row, col);
            if !step_forward(lines, &mut tr, &mut tc) {
                return (row, col);
            }
            if is_ws_at(lines, tr, tc) {
                break;
            }
            row = tr;
            col = tc;
        }
    }
    (row, col)
}

/// `ge` / `gE` — move cursor backward to the end of the previous word
/// (or WORD). `big = true` treats any run of non-whitespace as one
/// WORD; `big = false` distinguishes word-chars (alnum / `_`) from
/// separators the way vim does. Skips blank lines.
fn word_end_back(ed: &Editor<'_>, big: bool, count: usize) -> (usize, usize) {
    let lines = ed.textarea.lines();
    let (mut row, mut col) = ed.textarea.cursor();
    for _ in 0..count.max(1) {
        loop {
            if !step_back(lines, &mut row, &mut col) {
                return (row, col);
            }
            if is_ws_at(lines, row, col) {
                continue;
            }
            // Stop when `(row, col)` is the end of a (WORD-)word run —
            // the char immediately after it has a different class. For
            // `big = true` everything non-whitespace is the same class.
            let cur = char_class_at(lines, row, col);
            let next = char_class_after(lines, row, col);
            let same = if big {
                cur != CharClass::Ws && next != CharClass::Ws
            } else {
                cur == next
            };
            if !same {
                break;
            }
        }
    }
    (row, col)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Ws,
    Word,
    Sep,
}

fn char_class_at(lines: &[String], row: usize, col: usize) -> CharClass {
    match lines.get(row).and_then(|l| l.chars().nth(col)) {
        Some(c) if c.is_whitespace() => CharClass::Ws,
        Some(c) if c.is_alphanumeric() || c == '_' => CharClass::Word,
        Some(_) => CharClass::Sep,
        None => CharClass::Ws,
    }
}

fn char_class_after(lines: &[String], row: usize, col: usize) -> CharClass {
    let line_len = lines.get(row).map(|l| l.chars().count()).unwrap_or(0);
    if col + 1 < line_len {
        char_class_at(lines, row, col + 1)
    } else {
        // Newline / end-of-buffer — treat as whitespace so the current
        // position counts as an end-of-word.
        CharClass::Ws
    }
}

fn move_first_non_whitespace(ed: &mut Editor<'_>) {
    ed.textarea.move_cursor(CursorMove::Head);
    let (row, _) = ed.textarea.cursor();
    let indent = ed.textarea.lines()[row]
        .chars()
        .take_while(|c| c.is_whitespace())
        .count();
    for _ in 0..indent {
        ed.textarea.move_cursor(CursorMove::Forward);
    }
}

fn find_char_on_line(ed: &mut Editor<'_>, ch: char, forward: bool, till: bool) -> bool {
    let (row, col) = ed.textarea.cursor();
    let line = &ed.textarea.lines()[row];
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return false;
    }
    if forward {
        for (i, c) in chars.iter().enumerate().skip(col + 1) {
            if *c == ch {
                let target = if till { i.saturating_sub(1) } else { i };
                ed.textarea.move_cursor(CursorMove::Jump(row, target));
                return true;
            }
        }
    } else {
        for i in (0..col).rev() {
            if chars[i] == ch {
                let target = if till { i + 1 } else { i };
                ed.textarea.move_cursor(CursorMove::Jump(row, target));
                return true;
            }
        }
    }
    false
}

fn matching_bracket(ed: &mut Editor<'_>) -> bool {
    let (row, col) = ed.textarea.cursor();
    let lines = ed.textarea.lines();
    let line = &lines[row];
    let ch = match line[col..].chars().next() {
        Some(c) => c,
        None => return false,
    };
    let (open, close, forward) = match ch {
        '(' => ('(', ')', true),
        ')' => ('(', ')', false),
        '[' => ('[', ']', true),
        ']' => ('[', ']', false),
        '{' => ('{', '}', true),
        '}' => ('{', '}', false),
        '<' => ('<', '>', true),
        '>' => ('<', '>', false),
        _ => return false,
    };
    let mut depth: i32 = 0;
    if forward {
        let mut r = row;
        let mut c = col;
        loop {
            let cur_line = &lines[r];
            let chars: Vec<char> = cur_line.chars().collect();
            while c < chars.len() {
                let ch = chars[c];
                if ch == open {
                    depth += 1;
                } else if ch == close {
                    depth -= 1;
                    if depth == 0 {
                        ed.textarea.move_cursor(CursorMove::Jump(r, c));
                        return true;
                    }
                }
                c += 1;
            }
            if r + 1 >= lines.len() {
                return false;
            }
            r += 1;
            c = 0;
        }
    } else {
        let mut r = row;
        let mut c = col as isize;
        loop {
            let cur_line = &lines[r];
            let chars: Vec<char> = cur_line.chars().collect();
            while c >= 0 {
                let ch = chars[c as usize];
                if ch == close {
                    depth += 1;
                } else if ch == open {
                    depth -= 1;
                    if depth == 0 {
                        ed.textarea.move_cursor(CursorMove::Jump(r, c as usize));
                        return true;
                    }
                }
                c -= 1;
            }
            if r == 0 {
                return false;
            }
            r -= 1;
            c = lines[r].chars().count() as isize - 1;
        }
    }
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

    // Same-letter: dd / cc / yy.
    let double_ch = match op {
        Operator::Delete => 'd',
        Operator::Change => 'c',
        Operator::Yank => 'y',
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
            if count > 1 {
                ed.textarea.move_cursor(CursorMove::Jump(count - 1, 0));
            } else {
                ed.textarea.move_cursor(CursorMove::Top);
            }
            move_first_non_whitespace(ed);
        }
        Key::Char('e') => execute_motion(ed, Motion::WordEndBack, count),
        Key::Char('E') => execute_motion(ed, Motion::BigWordEndBack, count),
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
    ed.vim.insert_session = Some(InsertSession {
        count,
        before: ed.textarea.lines().join("\n"),
        reason,
    });
    ed.vim.mode = Mode::Insert;
}

// ─── Operator × Motion application ─────────────────────────────────────────

fn apply_op_with_motion(ed: &mut Editor<'_>, op: Operator, motion: &Motion, count: usize) {
    let start = ed.textarea.cursor();
    // Tentatively apply motion to find the endpoint.
    apply_motion_cursor(ed, motion, count);
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
    }
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
            select_full_lines(ed, row, end_row);
            ed.vim.yank_linewise = true;
            ed.mutate(|t| t.cut());
            if let Some(y) = non_empty_yank(ed) {
                ed.last_yank = Some(y);
            }
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
    }
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
    ed.content_dirty = true;
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

fn replace_char(ed: &mut Editor<'_>, ch: char, count: usize) {
    ed.push_undo();
    for _ in 0..count {
        ed.mutate(|t| t.delete_next_char());
        ed.mutate(|t| t.insert_char(ch));
    }
    // Vim leaves the cursor on the last replaced char.
    ed.textarea.move_cursor(CursorMove::Back);
}

fn toggle_case_at_cursor(ed: &mut Editor<'_>) {
    let (row, col) = ed.textarea.cursor();
    let lines = ed.textarea.lines();
    if row >= lines.len() {
        return;
    }
    let Some(c) = lines[row][col..].chars().next() else {
        return;
    };
    let toggled = if c.is_uppercase() {
        c.to_lowercase().next().unwrap_or(c)
    } else {
        c.to_uppercase().next().unwrap_or(c)
    };
    ed.mutate(|t| t.delete_next_char());
    ed.mutate(|t| t.insert_char(toggled));
}

fn join_line(ed: &mut Editor<'_>) {
    let (row, _) = ed.textarea.cursor();
    if row + 1 >= ed.textarea.lines().len() {
        return;
    }
    ed.textarea.move_cursor(CursorMove::End);
    let end_col = ed.textarea.cursor().1;
    ed.mutate(|t| t.delete_next_char());
    loop {
        let (r, c) = ed.textarea.cursor();
        let line = ed.textarea.lines()[r].clone();
        match line[c..].chars().next() {
            Some(ch) if ch.is_whitespace() => {
                ed.mutate(|t| t.delete_next_char());
            }
            _ => break,
        }
    }
    let (r, c) = ed.textarea.cursor();
    let has_right = c < ed.textarea.lines()[r].len();
    if end_col > 0 && has_right {
        ed.mutate(|t| t.insert_char(' '));
        ed.textarea.move_cursor(CursorMove::Back);
    }
}

/// `gJ` — join the next line onto the current one without inserting a
/// separating space or stripping leading whitespace.
fn join_line_raw(ed: &mut Editor<'_>) {
    let (row, _) = ed.textarea.cursor();
    if row + 1 >= ed.textarea.lines().len() {
        return;
    }
    ed.textarea.move_cursor(CursorMove::End);
    ed.mutate(|t| t.delete_next_char());
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
        run_keys(&mut e, "<C-d>");
        run_keys(&mut e, "<C-u>");
        run_keys(&mut e, "<C-f>");
        run_keys(&mut e, "<C-b>");
        // No explicit assert beyond "didn't panic".
        assert!(!e.textarea.lines().is_empty());
    }
}
