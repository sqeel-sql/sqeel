//! Vim-shaped text buffer for the sqeel editor.
//!
//! Replaces the vendored tui-textarea with a buffer model that has
//! vim semantics baked in: charwise/linewise/blockwise selection are
//! first-class, motions match vim's edge-case behaviour out of the
//! box (no `h` wrap, `$` clamp, sticky col on `j`/`k`), and the
//! render path writes ratatui cells directly without going through
//! `Paragraph`.
//!
//! This crate is intentionally not a general-purpose terminal text
//! widget — it's shaped for SQL editing inside sqeel and avoids the
//! surface-area bloat that comes from supporting every editor idiom
//! at once. See `TODO.md` at the repo root for the migration plan.

mod buffer;
mod motion;
mod position;
mod selection;
mod span;
mod viewport;

pub use buffer::Buffer;
pub use position::Position;
pub use selection::{RowSpan, Selection};
pub use span::Span;
pub use viewport::Viewport;
