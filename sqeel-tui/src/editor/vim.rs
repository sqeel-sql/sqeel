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

use sqeel_core::state::VimMode;
use tui_textarea::{CursorMove, Input, Key, Scrolling};

use super::Editor;

// ─── Modes & parser state ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
    Visual,
    VisualLine,
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
    /// Row anchor for VisualLine mode.
    pub(super) visual_line_anchor: usize,
    /// Track whether the last yank/cut was linewise (drives `p`/`P` layout).
    pub(super) yank_linewise: bool,
    /// Set while replaying `.` / last-change so we don't re-record it.
    replaying: bool,
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
}

impl VimState {
    pub fn public_mode(&self) -> VimMode {
        match self.mode {
            Mode::Normal => VimMode::Normal,
            Mode::Insert => VimMode::Insert,
            Mode::Visual => VimMode::Visual,
            Mode::VisualLine => VimMode::VisualLine,
        }
    }

    pub fn force_normal(&mut self) {
        self.mode = Mode::Normal;
        self.pending = Pending::None;
        self.count = 0;
        self.insert_session = None;
    }

    pub fn is_visual(&self) -> bool {
        matches!(self.mode, Mode::Visual | Mode::VisualLine)
    }

    pub fn is_visual_char(&self) -> bool {
        self.mode == Mode::Visual
    }

    pub fn enter_visual(&mut self) {
        self.mode = Mode::Visual;
    }
}

// ─── Entry point ───────────────────────────────────────────────────────────

pub fn step(ed: &mut Editor<'_>, input: Input) -> bool {
    match ed.vim.mode {
        Mode::Insert => step_insert(ed, input),
        _ => step_normal(ed, input),
    }
}

// ─── Insert mode ───────────────────────────────────────────────────────────

fn step_insert(ed: &mut Editor<'_>, input: Input) -> bool {
    if input.key == Key::Esc {
        finish_insert_session(ed);
        ed.vim.mode = Mode::Normal;
        // Vim leaves the cursor one column to the left on exit when possible.
        let (row, col) = ed.textarea.cursor();
        if col > 0 {
            let _ = row;
            ed.textarea.move_cursor(CursorMove::Back);
        }
        return true;
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
            Pending::Replace | Pending::Find { .. } | Pending::OpFind { .. }
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
            ed.textarea.start_selection();
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
            ed.textarea.start_selection();
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
        _ => {}
    }

    // Visual mode: operators act on the current selection.
    if ed.vim.is_visual()
        && let Some(op) = visual_operator(&input)
    {
        apply_visual_operator(ed, op);
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
    apply_motion_cursor(ed, &motion, count);
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
        Motion::WordFwd | Motion::BigWordFwd => {
            for _ in 0..count {
                ed.textarea.move_cursor(CursorMove::WordForward);
            }
        }
        Motion::WordBack | Motion::BigWordBack => {
            for _ in 0..count {
                ed.textarea.move_cursor(CursorMove::WordBack);
            }
        }
        Motion::WordEnd | Motion::BigWordEnd => {
            for _ in 0..count {
                ed.textarea.move_cursor(CursorMove::WordEnd);
            }
        }
        Motion::LineStart => ed.textarea.move_cursor(CursorMove::Head),
        Motion::FirstNonBlank => move_first_non_whitespace(ed),
        Motion::LineEnd => ed.textarea.move_cursor(CursorMove::End),
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
    if input.key == Key::Char('g') && !input.ctrl {
        let count2 = take_count(&mut ed.vim);
        let total = count1.max(1) * count2.max(1);
        apply_op_with_motion(ed, op, &Motion::FileTop, total);
        if !ed.vim.replaying && op_is_change(op) {
            ed.vim.last_change = Some(LastChange::OpMotion {
                op,
                motion: Motion::FileTop,
                count: total,
                inserted: None,
            });
        }
        return true;
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
        Key::Char('e') => {
            // Approximation of `ge` — WordBack then WordEnd (good enough
            // for most cases; exact vim semantics land in a polish pass).
            for _ in 0..count.max(1) {
                ed.textarea.move_cursor(CursorMove::WordBack);
            }
            ed.textarea.move_cursor(CursorMove::WordEnd);
        }
        _ => {}
    }
    true
}

fn handle_replace(ed: &mut Editor<'_>, input: Input) -> bool {
    if let Key::Char(ch) = input.key {
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
        Motion::WordEnd | Motion::BigWordEnd => MotionKind::Inclusive,
        Motion::Find { .. } => MotionKind::Inclusive,
        Motion::MatchBracket => MotionKind::Inclusive,
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
    if y.is_empty() { None } else { Some(y) }
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
            ed.push_undo();
            select_full_lines(ed, row, end_row);
            ed.vim.yank_linewise = true;
            ed.mutate(|t| t.cut());
            if let Some(y) = non_empty_yank(ed) {
                ed.last_yank = Some(y);
            }
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
            finalize_visual_line_selection(ed);
            ed.vim.yank_linewise = true;
            match op {
                Operator::Yank => {
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
        Mode::Visual => {
            // tui-textarea selection is exclusive of end; include cursor char.
            ed.textarea.move_cursor(CursorMove::Forward);
            match op {
                Operator::Yank => {
                    ed.textarea.copy();
                    if let Some(y) = non_empty_yank(ed) {
                        ed.last_yank = Some(y);
                    }
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
            ed.vim.yank_linewise = false;
        }
        _ => {}
    }
}

// ─── Visual-line helpers ───────────────────────────────────────────────────

/// Rebuild the VisualLine selection after the cursor moved.
///
/// Cursor always lands at column 0 of the active row (matches vim's
/// "beginning of line" convention for V mode). The anchor end is pinned
/// to the far side of the opposite row so rows between the anchor and
/// cursor are fully highlighted. The active row shows only the cursor
/// indicator at col 0 — tui-textarea's single-range selection can't
/// simultaneously place the cursor at col 0 *and* extend the highlight
/// to the end of that same row when extending down.
fn refresh_visual_line_selection(ed: &mut Editor<'_>) {
    let (cursor_row, _) = ed.textarea.cursor();
    let anchor_row = ed.vim.visual_line_anchor;
    ed.textarea.cancel_selection();
    if cursor_row >= anchor_row {
        // Extending down: anchor at start of anchor row, cursor at start of active row.
        ed.textarea.move_cursor(CursorMove::Jump(anchor_row, 0));
        ed.textarea.start_selection();
        ed.textarea.move_cursor(CursorMove::Jump(cursor_row, 0));
    } else {
        // Extending up: anchor at end of anchor row, cursor at start of active row.
        ed.textarea.move_cursor(CursorMove::Jump(anchor_row, 0));
        ed.textarea.move_cursor(CursorMove::End);
        ed.textarea.start_selection();
        ed.textarea.move_cursor(CursorMove::Jump(cursor_row, 0));
    }
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
        } else if before {
            ed.textarea.move_cursor(CursorMove::Back);
            ed.mutate(|t| t.paste());
        } else {
            ed.mutate(|t| t.paste());
        }
    }
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
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use sqeel_core::state::{KeybindingMode, VimMode};

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
    fn visual_line_cursor_stays_on_active_row_at_col_zero() {
        let mut e = editor_with("aaa\nbbb\nccc\nddd");
        run_keys(&mut e, "V");
        assert_eq!(e.vim_mode(), VimMode::VisualLine);
        assert_eq!(e.textarea.cursor(), (0, 0));
        run_keys(&mut e, "j");
        assert_eq!(e.textarea.cursor(), (1, 0));
        run_keys(&mut e, "j");
        assert_eq!(e.textarea.cursor(), (2, 0));
    }

    #[test]
    fn visual_line_extending_up_keeps_cursor_at_col_zero() {
        let mut e = editor_with("aaa\nbbb\nccc\nddd");
        run_keys(&mut e, "jjj");
        run_keys(&mut e, "V");
        assert_eq!(e.textarea.cursor(), (3, 0));
        run_keys(&mut e, "k");
        assert_eq!(e.textarea.cursor(), (2, 0));
        run_keys(&mut e, "k");
        assert_eq!(e.textarea.cursor(), (1, 0));
    }
}
