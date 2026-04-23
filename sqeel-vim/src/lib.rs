//! Vim-mode editor widget built on top of `tui-textarea`.
//!
//! Exposes an [`Editor`] you can drop into a ratatui layout, a command
//! grammar that covers the bulk of vim's normal / insert / visual /
//! visual-line / visual-block modes, text-object operators, dot-repeat,
//! ex-command handling (`:s/foo/bar/g`, `:w`, `:q`, `:noh`, ...) and
//! render-overlay helpers for painting selection highlights.
//!
//! This crate currently lives inside the sqeel workspace and will likely
//! be promoted to a standalone crate once the API stabilises. The public
//! surface is intentionally narrow:
//!
//! - [`Editor`] — the editor widget.
//! - [`KeybindingMode`] / [`VimMode`] — mode enums used by host apps.
//! - [`ex::run`] / [`ex::ExEffect`] — drive ex-mode commands.
//! - [`paint_char_overlay`] / [`paint_line_overlay`] /
//!   [`paint_block_overlay`] — post-render selection highlighting.

mod editor;
pub mod ex;
mod render;
mod vim;

pub use editor::Editor;
pub use render::{paint_block_overlay, paint_char_overlay, paint_line_overlay};

/// Which keyboard discipline the editor uses. Currently vim-only, but
/// kept as an enum so future emacs / plain bindings can slot in without
/// touching the public signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeybindingMode {
    #[default]
    Vim,
}

#[cfg(feature = "serde")]
impl serde::Serialize for KeybindingMode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("vim")
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for KeybindingMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let _ = String::deserialize(d)?;
        Ok(KeybindingMode::Vim)
    }
}

/// Coarse vim-mode a host app can display in its status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VimMode {
    #[default]
    Normal,
    Insert,
    Visual,
    VisualLine,
    VisualBlock,
}
