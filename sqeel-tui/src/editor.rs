use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use sqeel_core::state::{KeybindingMode, VimMode};
use tui_textarea::{CursorMove, Input, Key, Scrolling, TextArea};

macro_rules! inp {
    ($key:expr) => {
        Input { key: $key, ctrl: false, alt: false, shift: false }
    };
    ($key:expr, ctrl) => {
        Input { key: $key, ctrl: true, alt: false, shift: false }
    };
    ($key:expr, shift) => {
        Input { key: $key, ctrl: false, alt: false, shift: true }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Insert,
    Visual,
    /// Operator pending: next motion completes d/c/y + motion (e.g. dw, c$, yy)
    Operator(char),
}

pub struct Editor<'a> {
    pub textarea: TextArea<'a>,
    mode: Mode,
    /// Saved first key for two-key sequences (gg, r<char>)
    pending: Input,
    pub keybinding_mode: KeybindingMode,
    /// Set when the user yanks/cuts; caller drains this to write to OS clipboard.
    pub last_yank: Option<String>,
}

impl<'a> Editor<'a> {
    pub fn new(keybinding_mode: KeybindingMode) -> Self {
        Self {
            textarea: TextArea::default(),
            mode: Mode::Normal,
            pending: Input::default(),
            keybinding_mode,
            last_yank: None,
        }
    }

    pub fn vim_mode(&self) -> VimMode {
        match self.mode {
            Mode::Normal | Mode::Operator(_) => VimMode::Normal,
            Mode::Insert => VimMode::Insert,
            Mode::Visual => VimMode::Visual,
        }
    }

    /// Force back to normal mode (used when dismissing completions etc.)
    pub fn force_normal(&mut self) {
        self.textarea.cancel_selection();
        self.mode = Mode::Normal;
        self.pending = Input::default();
    }

    pub fn content(&self) -> String {
        let mut s = self.textarea.lines().join("\n");
        if !s.ends_with('\n') {
            s.push('\n');
        }
        s
    }

    pub fn set_content(&mut self, text: &str) {
        let lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
        self.textarea = TextArea::new(lines);
    }

    pub fn insert_str(&mut self, text: &str) {
        self.textarea.insert_str(text);
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
            self.textarea.delete_char();
        }
        self.textarea.insert_str(completion);
    }

    /// Returns true if the key was consumed by the editor.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        match self.keybinding_mode {
            KeybindingMode::Vim => self.handle_vim(key),
            KeybindingMode::Emacs => self.handle_emacs(key),
        }
    }

    fn handle_vim(&mut self, key: KeyEvent) -> bool {
        let input = crossterm_to_input(key);
        if input.key == Key::Null {
            return false;
        }
        match self.mode {
            Mode::Insert => self.vim_insert(input),
            _ => self.vim_normal_visual_operator(input),
        }
    }

    fn vim_insert(&mut self, input: Input) -> bool {
        match input {
            Input { key: Key::Esc, .. } => {
                self.mode = Mode::Normal;
                true
            }
            input => {
                self.textarea.input(input);
                true
            }
        }
    }

    fn vim_normal_visual_operator(&mut self, input: Input) -> bool {
        let pending = std::mem::replace(&mut self.pending, Input::default());

        // r<char>: replace char under cursor
        if pending.key == Key::Char('r') {
            if let Key::Char(c) = input.key {
                self.textarea.delete_next_char();
                self.textarea.insert_char(c);
                self.textarea.move_cursor(CursorMove::Back);
                return true;
            }
            // Not a char key — drop the replace
            return false;
        }

        match input {
            // ── Movement ────────────────────────────────────────────────────
            Input { key: Key::Char('h'), .. } | Input { key: Key::Left, .. } => {
                self.textarea.move_cursor(CursorMove::Back)
            }
            Input { key: Key::Char('j'), .. } | Input { key: Key::Down, .. } => {
                self.textarea.move_cursor(CursorMove::Down)
            }
            Input { key: Key::Char('k'), .. } | Input { key: Key::Up, .. } => {
                self.textarea.move_cursor(CursorMove::Up)
            }
            Input { key: Key::Char('l'), .. } | Input { key: Key::Right, .. } => {
                self.textarea.move_cursor(CursorMove::Forward)
            }
            Input { key: Key::Char('w'), .. } | Input { key: Key::Char('W'), .. } => {
                self.textarea.move_cursor(CursorMove::WordForward)
            }
            Input { key: Key::Char('b'), ctrl: false, .. }
            | Input { key: Key::Char('B'), .. } => {
                self.textarea.move_cursor(CursorMove::WordBack)
            }
            Input { key: Key::Char('e'), ctrl: false, .. }
            | Input { key: Key::Char('E'), .. } => {
                self.textarea.move_cursor(CursorMove::WordEnd);
                if matches!(self.mode, Mode::Operator(_)) {
                    self.textarea.move_cursor(CursorMove::Forward);
                }
            }
            Input { key: Key::Char('0'), .. } | Input { key: Key::Home, .. } => {
                self.textarea.move_cursor(CursorMove::Head)
            }
            Input { key: Key::Char('^'), .. } => {
                self.move_first_non_whitespace()
            }
            Input { key: Key::Char('$'), .. } | Input { key: Key::End, .. } => {
                self.textarea.move_cursor(CursorMove::End)
            }
            Input { key: Key::Char('G'), ctrl: false, .. } => {
                self.textarea.move_cursor(CursorMove::Bottom)
            }
            // gg — second g when pending was g
            Input { key: Key::Char('g'), ctrl: false, .. }
                if pending.key == Key::Char('g') =>
            {
                self.textarea.move_cursor(CursorMove::Top)
            }

            // ── Scrolling ───────────────────────────────────────────────────
            Input { key: Key::Char('d'), ctrl: true, .. } => {
                self.textarea.scroll(Scrolling::HalfPageDown)
            }
            Input { key: Key::Char('u'), ctrl: true, .. } => {
                self.textarea.scroll(Scrolling::HalfPageUp)
            }
            Input { key: Key::Char('f'), ctrl: true, .. } => {
                self.textarea.scroll(Scrolling::PageDown)
            }
            Input { key: Key::Char('b'), ctrl: true, .. } => {
                self.textarea.scroll(Scrolling::PageUp)
            }

            // ── Mode transitions ────────────────────────────────────────────
            Input { key: Key::Char('i'), .. } if self.mode != Mode::Visual => {
                self.textarea.cancel_selection();
                self.mode = Mode::Insert;
                return true;
            }
            Input { key: Key::Char('I'), .. } if self.mode != Mode::Visual => {
                self.textarea.cancel_selection();
                self.move_first_non_whitespace();
                self.mode = Mode::Insert;
                return true;
            }
            Input { key: Key::Char('a'), .. } if self.mode != Mode::Visual => {
                self.textarea.cancel_selection();
                self.textarea.move_cursor(CursorMove::Forward);
                self.mode = Mode::Insert;
                return true;
            }
            Input { key: Key::Char('A'), .. } if self.mode != Mode::Visual => {
                self.textarea.cancel_selection();
                self.textarea.move_cursor(CursorMove::End);
                self.mode = Mode::Insert;
                return true;
            }
            Input { key: Key::Char('o'), .. } if self.mode != Mode::Visual => {
                self.textarea.move_cursor(CursorMove::End);
                self.textarea.insert_newline();
                self.mode = Mode::Insert;
                return true;
            }
            Input { key: Key::Char('O'), .. } if self.mode != Mode::Visual => {
                self.textarea.move_cursor(CursorMove::Head);
                self.textarea.insert_newline();
                self.textarea.move_cursor(CursorMove::Up);
                self.mode = Mode::Insert;
                return true;
            }
            Input { key: Key::Char('v'), ctrl: false, .. }
                if self.mode == Mode::Normal =>
            {
                self.textarea.start_selection();
                self.mode = Mode::Visual;
                return true;
            }
            Input { key: Key::Char('V'), ctrl: false, .. }
                if self.mode == Mode::Normal =>
            {
                self.textarea.move_cursor(CursorMove::Head);
                self.textarea.start_selection();
                self.textarea.move_cursor(CursorMove::End);
                self.mode = Mode::Visual;
                return true;
            }
            Input { key: Key::Esc, .. }
            | Input { key: Key::Char('v'), ctrl: false, .. }
                if self.mode == Mode::Visual =>
            {
                self.textarea.cancel_selection();
                self.mode = Mode::Normal;
                return true;
            }
            Input { key: Key::Esc, .. } if matches!(self.mode, Mode::Operator(_)) => {
                self.textarea.cancel_selection();
                self.mode = Mode::Normal;
                return true;
            }

            // ── Edit ────────────────────────────────────────────────────────
            Input { key: Key::Char('x'), .. } if self.mode != Mode::Visual => {
                self.textarea.delete_next_char();
                self.mode = Mode::Normal;
                return true;
            }
            Input { key: Key::Char('X'), .. } => {
                self.textarea.delete_char();
                return true;
            }
            Input { key: Key::Char('D'), .. } => {
                self.textarea.delete_line_by_end();
                self.mode = Mode::Normal;
                return true;
            }
            Input { key: Key::Char('C'), .. } => {
                self.textarea.delete_line_by_end();
                self.textarea.cancel_selection();
                self.mode = Mode::Insert;
                return true;
            }
            Input { key: Key::Char('p'), .. } => {
                self.textarea.paste();
                let y = self.textarea.yank_text();
                if !y.is_empty() { self.last_yank = Some(y); }
                self.mode = Mode::Normal;
                return true;
            }
            Input { key: Key::Char('P'), .. } => {
                // paste before: step back, paste, step forward past inserted text
                self.textarea.move_cursor(CursorMove::Back);
                self.textarea.paste();
                let y = self.textarea.yank_text();
                if !y.is_empty() { self.last_yank = Some(y); }
                return true;
            }
            Input { key: Key::Char('u'), ctrl: false, .. } => {
                self.textarea.undo();
                self.mode = Mode::Normal;
                return true;
            }
            Input { key: Key::Char('r'), ctrl: true, .. } => {
                self.textarea.redo();
                self.mode = Mode::Normal;
                return true;
            }
            Input { key: Key::Char('~'), .. } => {
                self.toggle_case_at_cursor();
                return true;
            }

            // ── yy: yank line without moving cursor ─────────────────────────
            Input { key: Key::Char('y'), ctrl: false, .. }
                if self.mode == Mode::Operator('y') =>
            {
                let (row, col) = self.textarea.cursor();
                self.textarea.move_cursor(CursorMove::Head);
                self.textarea.start_selection();
                let before = self.textarea.cursor();
                self.textarea.move_cursor(CursorMove::Down);
                if self.textarea.cursor() == before {
                    self.textarea.move_cursor(CursorMove::End);
                }
                self.textarea.copy();
                let y = self.textarea.yank_text();
                if !y.is_empty() { self.last_yank = Some(y); }
                self.textarea.cancel_selection();
                self.textarea.move_cursor(CursorMove::Jump(row as u16, col as u16));
                self.mode = Mode::Normal;
                return true;
            }

            // ── Operator + motion (dd / cc double-key) ───────────────────────
            Input { key: Key::Char(c), ctrl: false, .. }
                if self.mode == Mode::Operator(c) =>
            {
                // dd / cc: select whole line
                self.textarea.move_cursor(CursorMove::Head);
                self.textarea.start_selection();
                let before = self.textarea.cursor();
                self.textarea.move_cursor(CursorMove::Down);
                if self.textarea.cursor() == before {
                    self.textarea.move_cursor(CursorMove::End);
                }
                // fall through to operator apply below
            }

            // ── Operator activation (d / c / y in normal mode) ──────────────
            Input { key: Key::Char(op @ ('y' | 'd' | 'c')), ctrl: false, .. }
                if self.mode == Mode::Normal =>
            {
                self.textarea.start_selection();
                self.mode = Mode::Operator(op);
                return true;
            }

            // ── Visual mode operators ────────────────────────────────────────
            Input { key: Key::Char('y'), ctrl: false, .. }
                if self.mode == Mode::Visual =>
            {
                self.textarea.move_cursor(CursorMove::Forward);
                self.textarea.copy();
                let y = self.textarea.yank_text();
                if !y.is_empty() { self.last_yank = Some(y); }
                self.mode = Mode::Normal;
                return true;
            }
            Input { key: Key::Char('d') | Key::Char('x'), ctrl: false, .. }
                if self.mode == Mode::Visual =>
            {
                self.textarea.move_cursor(CursorMove::Forward);
                self.textarea.cut();
                let y = self.textarea.yank_text();
                if !y.is_empty() { self.last_yank = Some(y); }
                self.mode = Mode::Normal;
                return true;
            }
            Input { key: Key::Char('c'), ctrl: false, .. }
                if self.mode == Mode::Visual =>
            {
                self.textarea.move_cursor(CursorMove::Forward);
                self.textarea.cut();
                let y = self.textarea.yank_text();
                if !y.is_empty() { self.last_yank = Some(y); }
                self.mode = Mode::Insert;
                return true;
            }

            // ── Pending (first key of two-key sequence) ──────────────────────
            input => {
                self.pending = input;
                return true;
            }
        }

        // Apply pending operator after a motion
        match self.mode {
            Mode::Operator('y') => {
                self.textarea.copy();
                let y = self.textarea.yank_text();
                if !y.is_empty() { self.last_yank = Some(y); }
                self.mode = Mode::Normal;
            }
            Mode::Operator('d') => {
                self.textarea.cut();
                let y = self.textarea.yank_text();
                if !y.is_empty() { self.last_yank = Some(y); }
                self.mode = Mode::Normal;
            }
            Mode::Operator('c') => {
                self.textarea.cut();
                let y = self.textarea.yank_text();
                if !y.is_empty() { self.last_yank = Some(y); }
                self.mode = Mode::Insert;
            }
            _ => {}
        }

        true
    }

    fn move_first_non_whitespace(&mut self) {
        self.textarea.move_cursor(CursorMove::Head);
        let (row, _) = self.textarea.cursor();
        let indent = self.textarea.lines()[row]
            .chars()
            .take_while(|c| c.is_whitespace())
            .count();
        for _ in 0..indent {
            self.textarea.move_cursor(CursorMove::Forward);
        }
    }

    fn toggle_case_at_cursor(&mut self) {
        let (row, col) = self.textarea.cursor();
        let lines = self.textarea.lines();
        if row >= lines.len() { return; }
        let ch = lines[row][col..].chars().next();
        if let Some(c) = ch {
            let toggled = if c.is_uppercase() {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                c.to_uppercase().next().unwrap_or(c)
            };
            self.textarea.delete_next_char();
            self.textarea.insert_char(toggled);
            self.textarea.move_cursor(CursorMove::Back);
        }
    }

    fn handle_emacs(&mut self, key: KeyEvent) -> bool {
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('b')) => {
                self.textarea.input(inp!(Key::Left));
            }
            (KeyModifiers::CONTROL, KeyCode::Char('f')) => {
                self.textarea.input(inp!(Key::Right));
            }
            (KeyModifiers::CONTROL, KeyCode::Char('p')) => {
                self.textarea.input(inp!(Key::Up));
            }
            (KeyModifiers::CONTROL, KeyCode::Char('n')) => {
                self.textarea.input(inp!(Key::Down));
            }
            (KeyModifiers::CONTROL, KeyCode::Char('a')) => {
                self.textarea.input(inp!(Key::Home));
            }
            (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
                self.textarea.input(inp!(Key::End));
            }
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
                self.textarea.input(inp!(Key::Delete));
            }
            (KeyModifiers::CONTROL, KeyCode::Char('h')) => {
                self.textarea.input(inp!(Key::Backspace));
            }
            (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
                self.textarea.input(inp!(Key::End, ctrl));
            }
            _ => {
                self.textarea.input(crossterm_to_input(key));
            }
        }
        true
    }
}

fn crossterm_to_input(key: KeyEvent) -> Input {
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
    Input { key: k, ctrl, alt, shift }
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
        e.mode = Mode::Insert;
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
        e.mode = Mode::Visual;
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
        e.textarea.move_cursor(CursorMove::Down); // row 1
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
    fn vim_u_undoes() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.set_content("hello");
        e.mode = Mode::Insert;
        e.handle_key(key(KeyCode::Char('x')));
        e.mode = Mode::Normal;
        e.handle_key(key(KeyCode::Char('u')));
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
    fn vim_normal_unknown_key_pending() {
        let mut e = Editor::new(KeybindingMode::Vim);
        // Unknown keys go to pending, not consumed as false
        let consumed = e.handle_key(key(KeyCode::Char('z')));
        assert!(consumed); // now returns true (stored as pending)
    }

    #[test]
    fn emacs_mode_passthrough() {
        let mut e = Editor::new(KeybindingMode::Emacs);
        let consumed = e.handle_key(key(KeyCode::Char('a')));
        assert!(consumed);
    }

    #[test]
    fn emacs_ctrl_b_consumed() {
        let mut e = Editor::new(KeybindingMode::Emacs);
        let consumed = e.handle_key(ctrl_key(KeyCode::Char('b')));
        assert!(consumed);
    }

    #[test]
    fn force_normal_clears_operator() {
        let mut e = Editor::new(KeybindingMode::Vim);
        e.mode = Mode::Operator('d');
        e.force_normal();
        assert_eq!(e.vim_mode(), VimMode::Normal);
    }
}
