//! Backend-agnostic key input types used by the vim engine.
//!
//! Phase 8 of the sqeel-buffer migration replaced `tui_textarea::Input`
//! / `tui_textarea::Key` with these in-crate equivalents so the
//! editor can drop the `tui-textarea` dependency entirely.

/// A key code, mirroring the subset of [`crossterm::event::KeyCode`]
/// the vim engine actually consumes. `Null` is the conventional
/// sentinel for "no input" (matching the previous `tui_textarea::Key`
/// shape) so call sites can early-return on unsupported keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Key {
    Char(char),
    Backspace,
    Enter,
    Left,
    Right,
    Up,
    Down,
    Tab,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Esc,
    #[default]
    Null,
}

/// A key press with modifier flags. The vim engine reads modifiers
/// directly off this struct (e.g. `input.ctrl && input.key == Key::Char('d')`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Input {
    pub key: Key,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}
