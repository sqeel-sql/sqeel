//! `SqeelHost` — host adapter for the planned `Editor<B, H>` trait
//! extraction in `hjkl-engine`.
//!
//! Today the runtime [`hjkl_engine::Editor`] consumes its host
//! state through inherent fields; the trait isn't wired in yet. This
//! type sits ready so the migration over to `Editor<B, H>` is a
//! single-callsite change once phase 5 proper lands on hjkl side.
//!
//! The intent fan-out covers the LSP requests sqeel-tui already routes
//! out-of-band today (`gd` for goto-def, etc.), plus fold ops and
//! buffer switching. Until the engine actually emits intents, the
//! `intents` queue stays empty.

use crate::Clipboard;
use hjkl_engine::types::Viewport;
use hjkl_engine::{CursorShape, Host, Pos};
use std::time::Instant;

/// Buffer identifier in sqeel's tab manager. Opaque from the engine's
/// side; sqeel owns generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SqeelBufferId(pub u64);

/// Range within a buffer (line indexes) — used by `FormatRange` and
/// fold operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

/// Intents sqeel-tui drains from the engine each render pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqeelIntent {
    // ── LSP-equivalents ──
    /// `K` — request hover info at `pos`.
    Hover(Pos),
    /// Insert-mode autocomplete trigger.
    Complete(Pos, char),
    /// `gd` — goto-definition.
    GotoDef(Pos),
    /// Visual-mode rename (`gR`).
    Rename(Pos, String),
    /// Show diagnostic for `line`.
    Diagnostic(u32),
    /// Format the given line range.
    FormatRange(LineRange),

    // ── Fold ops ──
    /// `zo` / `zc` / `za` etc. Host applies to its fold table.
    FoldOp(FoldOp),

    // ── Buffer list ops ──
    /// `:b{n}` — switch to a known buffer.
    SwitchBuffer(SqeelBufferId),
    /// `:ls` — show buffer list.
    ListBuffers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FoldOp {
    Open,
    Close,
    Toggle,
    OpenAll,
    CloseAll,
    AtLine(u32),
}

/// Host adapter for the planned `Editor<B, H>` constructor.
pub struct SqeelHost {
    last_cursor_shape: CursorShape,
    clipboard: Clipboard,
    /// Cached system clipboard value. Refreshed on focus events / OSC52
    /// reply; reads return this slot directly (never block).
    clipboard_cache: Option<String>,
    /// Pending writes flushed by the host's tick loop. Engine never
    /// awaits.
    clipboard_outbox: Vec<String>,
    started: Instant,
    intents: Vec<SqeelIntent>,
    cancel: bool,
    /// Runtime viewport — relocated off `hjkl_buffer::Buffer` in
    /// hjkl 0.0.34 (Patch C-δ.1). Host owns the (top_row, top_col,
    /// width, height, text_width, wrap) tuple; the engine reads/writes
    /// scroll offsets, the renderer publishes width/height per frame.
    viewport: Viewport,
}

impl SqeelHost {
    pub fn new(clipboard: Clipboard) -> Self {
        Self {
            last_cursor_shape: CursorShape::Block,
            clipboard,
            clipboard_cache: None,
            clipboard_outbox: Vec::new(),
            started: Instant::now(),
            intents: Vec::new(),
            cancel: false,
            // Sensible default — renderer overwrites width/height per
            // frame from the editor pane's chunk rect.
            viewport: Viewport {
                top_row: 0,
                top_col: 0,
                width: 80,
                height: 24,
                ..Viewport::default()
            },
        }
    }

    /// Most recent cursor shape requested by the engine. Renderer reads.
    pub fn cursor_shape(&self) -> CursorShape {
        self.last_cursor_shape
    }

    /// Update the clipboard cache. Host calls this on focus events,
    /// OSC52 replies, or explicit poll.
    pub fn set_clipboard_cache(&mut self, text: Option<String>) {
        self.clipboard_cache = text;
    }

    /// Flush queued clipboard writes to the platform backend. Drops
    /// payloads that fail (logged via tracing in the future).
    pub fn flush_clipboard(&mut self) {
        let outbox = std::mem::take(&mut self.clipboard_outbox);
        for text in outbox {
            self.clipboard.set_text(&text);
        }
    }

    /// Drain queued intents. Host calls this once per render frame.
    pub fn drain_intents(&mut self) -> Vec<SqeelIntent> {
        std::mem::take(&mut self.intents)
    }

    /// Set / clear the cancellation flag (`Ctrl-C` handler hooks here).
    pub fn set_cancel(&mut self, cancel: bool) {
        self.cancel = cancel;
    }
}

impl Host for SqeelHost {
    type Intent = SqeelIntent;

    fn write_clipboard(&mut self, text: String) {
        self.clipboard_outbox.push(text);
    }

    fn read_clipboard(&mut self) -> Option<String> {
        self.clipboard_cache.clone()
    }

    fn now(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    fn should_cancel(&self) -> bool {
        self.cancel
    }

    fn prompt_search(&mut self) -> Option<String> {
        // Search prompt overlay is part of sqeel-tui's render loop;
        // when hjkl-engine starts driving search via Host, this hooks
        // into the existing prompt path. Until then, abort the search.
        None
    }

    fn emit_cursor_shape(&mut self, shape: CursorShape) {
        self.last_cursor_shape = shape;
    }

    fn emit_intent(&mut self, intent: Self::Intent) {
        self.intents.push(intent);
    }

    fn viewport(&self) -> &Viewport {
        &self.viewport
    }

    fn viewport_mut(&mut self) -> &mut Viewport {
        &mut self.viewport
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn satisfies_host_trait() {
        fn assert_host<H: Host>() {}
        assert_host::<SqeelHost>();
    }

    #[test]
    fn clipboard_outbox_drains() {
        let mut host = SqeelHost::new(Clipboard::new());
        host.write_clipboard("foo".into());
        host.write_clipboard("bar".into());
        // Don't actually flush — would touch arboard. Just confirm
        // queueing works.
        assert_eq!(host.clipboard_outbox.len(), 2);
        host.clipboard_outbox.clear();
    }

    #[test]
    fn cursor_shape_recorded() {
        let mut host = SqeelHost::new(Clipboard::new());
        assert_eq!(host.cursor_shape(), CursorShape::Block);
        host.emit_cursor_shape(CursorShape::Bar);
        assert_eq!(host.cursor_shape(), CursorShape::Bar);
    }

    #[test]
    fn intents_drain() {
        let mut host = SqeelHost::new(Clipboard::new());
        host.emit_intent(SqeelIntent::Hover(Pos::ORIGIN));
        host.emit_intent(SqeelIntent::ListBuffers);
        let drained = host.drain_intents();
        assert_eq!(drained.len(), 2);
        assert!(host.drain_intents().is_empty());
    }
}
