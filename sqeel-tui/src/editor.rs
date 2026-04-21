use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use sqeel_core::state::{KeybindingMode, VimMode};
use tui_textarea::{Input, Key, TextArea};

macro_rules! inp {
    ($key:expr) => {
        Input {
            key: $key,
            ctrl: false,
            alt: false,
            shift: false,
        }
    };
    ($key:expr, ctrl) => {
        Input {
            key: $key,
            ctrl: true,
            alt: false,
            shift: false,
        }
    };
}

pub struct Editor<'a> {
    pub textarea: TextArea<'a>,
    pub vim_mode: VimMode,
    pub keybinding_mode: KeybindingMode,
    /// Set when the user yanks text; caller should drain this to write to clipboard.
    pub last_yank: Option<String>,
}

impl<'a> Editor<'a> {
    pub fn new(keybinding_mode: KeybindingMode) -> Self {
        let textarea = TextArea::default();
        Self {
            textarea,
            vim_mode: VimMode::Normal,
            keybinding_mode,
            last_yank: None,
        }
    }

    pub fn content(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// Replace the entire editor content with `text`.
    pub fn set_content(&mut self, text: &str) {
        let lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
        self.textarea = TextArea::new(lines);
    }

    /// Insert `text` at the current cursor position.
    pub fn insert_str(&mut self, text: &str) {
        self.textarea.insert_str(text);
    }

    /// Replace the word currently being typed before the cursor with `completion`.
    /// Scans back from cursor to find the start of the current word (alphanumeric / _),
    /// deletes those characters, then inserts the completion text.
    pub fn accept_completion(&mut self, completion: &str) {
        let (row, col) = self.textarea.cursor();
        let line = self.textarea.lines()[row].clone();
        let before = &line[..col.min(line.len())];
        // Count how many word chars sit immediately before the cursor
        let prefix_len = before
            .chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .count();
        // Delete those chars one at a time (backspace)
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
        match self.vim_mode {
            VimMode::Normal => self.vim_normal(key),
            VimMode::Insert => self.vim_insert(key),
            VimMode::Visual => self.vim_visual(key),
        }
    }

    fn vim_normal(&mut self, key: KeyEvent) -> bool {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char('i')) => {
                self.vim_mode = VimMode::Insert;
                true
            }
            (KeyModifiers::NONE, KeyCode::Char('a')) => {
                self.vim_mode = VimMode::Insert;
                self.textarea.input(inp!(Key::Right));
                true
            }
            (KeyModifiers::NONE, KeyCode::Char('o')) => {
                self.vim_mode = VimMode::Insert;
                self.textarea.input(inp!(Key::End));
                self.textarea.input(inp!(Key::Enter));
                true
            }
            (KeyModifiers::NONE, KeyCode::Char('v')) => {
                self.vim_mode = VimMode::Visual;
                self.textarea.start_selection();
                true
            }
            (KeyModifiers::NONE, KeyCode::Char('h')) => {
                self.textarea.input(inp!(Key::Left));
                true
            }
            (KeyModifiers::NONE, KeyCode::Char('j')) => {
                self.textarea.input(inp!(Key::Down));
                true
            }
            (KeyModifiers::NONE, KeyCode::Char('k')) => {
                self.textarea.input(inp!(Key::Up));
                true
            }
            (KeyModifiers::NONE, KeyCode::Char('l')) => {
                self.textarea.input(inp!(Key::Right));
                true
            }
            (KeyModifiers::NONE, KeyCode::Char('0')) => {
                self.textarea.input(inp!(Key::Home));
                true
            }
            (KeyModifiers::NONE, KeyCode::Char('$')) => {
                self.textarea.input(inp!(Key::End));
                true
            }
            (KeyModifiers::NONE, KeyCode::Char('w')) => {
                self.textarea.input(inp!(Key::Right, ctrl));
                true
            }
            (KeyModifiers::NONE, KeyCode::Char('b')) => {
                self.textarea.input(inp!(Key::Left, ctrl));
                true
            }
            (KeyModifiers::NONE, KeyCode::Char('x')) => {
                self.textarea.input(inp!(Key::Delete));
                true
            }
            (KeyModifiers::NONE, KeyCode::Char('d')) => {
                self.textarea.input(inp!(Key::Home));
                self.textarea.input(inp!(Key::End, ctrl));
                true
            }
            _ => false,
        }
    }

    fn vim_insert(&mut self, key: KeyEvent) -> bool {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.vim_mode = VimMode::Normal;
                true
            }
            _ => {
                self.textarea.input(crossterm_to_input(key));
                true
            }
        }
    }

    fn vim_visual(&mut self, key: KeyEvent) -> bool {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.textarea.cancel_selection();
                self.vim_mode = VimMode::Normal;
                true
            }
            (KeyModifiers::NONE, KeyCode::Char('y')) => {
                self.textarea.copy();
                let yanked = self.textarea.yank_text();
                if !yanked.is_empty() {
                    self.last_yank = Some(yanked);
                }
                self.textarea.cancel_selection();
                self.vim_mode = VimMode::Normal;
                true
            }
            (KeyModifiers::NONE, KeyCode::Char('h')) => {
                self.textarea.input(Input {
                    key: Key::Left,
                    ctrl: false,
                    alt: false,
                    shift: true,
                });
                true
            }
            (KeyModifiers::NONE, KeyCode::Char('j')) => {
                self.textarea.input(Input {
                    key: Key::Down,
                    ctrl: false,
                    alt: false,
                    shift: true,
                });
                true
            }
            (KeyModifiers::NONE, KeyCode::Char('k')) => {
                self.textarea.input(Input {
                    key: Key::Up,
                    ctrl: false,
                    alt: false,
                    shift: true,
                });
                true
            }
            (KeyModifiers::NONE, KeyCode::Char('l')) => {
                self.textarea.input(Input {
                    key: Key::Right,
                    ctrl: false,
                    alt: false,
                    shift: true,
                });
                true
            }
            _ => false,
        }
    }

    fn handle_emacs(&mut self, key: KeyEvent) -> bool {
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('b')) => {
                self.textarea.input(inp!(Key::Left));
                true
            }
            (KeyModifiers::CONTROL, KeyCode::Char('f')) => {
                self.textarea.input(inp!(Key::Right));
                true
            }
            (KeyModifiers::CONTROL, KeyCode::Char('p')) => {
                self.textarea.input(inp!(Key::Up));
                true
            }
            (KeyModifiers::CONTROL, KeyCode::Char('n')) => {
                self.textarea.input(inp!(Key::Down));
                true
            }
            (KeyModifiers::CONTROL, KeyCode::Char('a')) => {
                self.textarea.input(inp!(Key::Home));
                true
            }
            (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
                self.textarea.input(inp!(Key::End));
                true
            }
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
                self.textarea.input(inp!(Key::Delete));
                true
            }
            (KeyModifiers::CONTROL, KeyCode::Char('h')) => {
                self.textarea.input(inp!(Key::Backspace));
                true
            }
            (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
                self.textarea.input(inp!(Key::End, ctrl));
                true
            }
            _ => {
                self.textarea.input(crossterm_to_input(key));
                true
            }
        }
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

    #[test]
    fn vim_normal_to_insert() {
        let mut editor = Editor::new(KeybindingMode::Vim);
        assert_eq!(editor.vim_mode, VimMode::Normal);
        editor.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        assert_eq!(editor.vim_mode, VimMode::Insert);
    }

    #[test]
    fn vim_insert_to_normal() {
        let mut editor = Editor::new(KeybindingMode::Vim);
        editor.vim_mode = VimMode::Insert;
        editor.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(editor.vim_mode, VimMode::Normal);
    }

    #[test]
    fn vim_normal_to_visual() {
        let mut editor = Editor::new(KeybindingMode::Vim);
        editor.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        assert_eq!(editor.vim_mode, VimMode::Visual);
    }

    #[test]
    fn vim_visual_to_normal() {
        let mut editor = Editor::new(KeybindingMode::Vim);
        editor.vim_mode = VimMode::Visual;
        editor.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(editor.vim_mode, VimMode::Normal);
    }

    #[test]
    fn emacs_mode_passthrough() {
        let mut editor = Editor::new(KeybindingMode::Emacs);
        assert_eq!(editor.vim_mode, VimMode::Normal);
        let consumed = editor.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(consumed);
    }

    #[test]
    fn vim_normal_blocks_unknown_key() {
        let mut editor = Editor::new(KeybindingMode::Vim);
        let consumed = editor.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
        assert!(!consumed);
    }

    #[test]
    fn emacs_ctrl_b_consumed() {
        let mut editor = Editor::new(KeybindingMode::Emacs);
        let consumed = editor.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert!(consumed);
    }
}
