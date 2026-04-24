mod clipboard;
mod completion_thread;
mod highlight_thread;
mod spinner;
mod theme;

// Re-export the editor crate so existing call sites like
// `sqeel_tui::editor::ex::ExEffect` keep compiling.
pub use sqeel_vim as editor;

use clipboard::Clipboard;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use spinner::frame as spinner_frame;

use completion_thread::CompletionThread;
use crossterm::{
    cursor::SetCursorStyle,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEventKind,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use highlight_thread::{HighlightResult, HighlightThread};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use sqeel_core::{
    AppState, UiProvider,
    completion_ctx::{self, CompletionCtx},
    config::load_main_config,
    highlight::{
        Dialect, Highlighter, TokenKind, first_syntax_error, is_show_create, statement_at_byte,
        statement_ranges, strip_sql_comments,
    },
    lsp::{LspClient, LspEvent},
    schema::{self, SchemaItemKind, SchemaTreeItem},
    state::{AddConnectionField, Focus, KeybindingMode, ResultsCursor, ResultsPane, VimMode},
};
use sqeel_vim::{
    Editor, GutterSign, paint_block_overlay, paint_char_overlay, paint_gutter_signs,
    paint_line_overlay,
};
use theme::ui;

/// Bundle of schema-sidebar search state: query string, whether the input box has
/// focus (typing mode), and cursor position within the filtered list.
#[derive(Clone, Default)]
struct SchemaSearch {
    query: Option<TextInput>,
    focused: bool,
    cursor: usize,
}

impl SchemaSearch {
    fn from_initial(q: Option<String>) -> Self {
        Self {
            query: q.map(|s| TextInput::from_str(&s)),
            focused: false,
            cursor: 0,
        }
    }
    fn query(&self) -> Option<&str> {
        self.query.as_ref().map(|q| q.text.as_str())
    }
    fn is_filtering(&self) -> bool {
        self.query().is_some_and(|q| !q.is_empty())
    }
    fn clear(&mut self) {
        *self = Self::default();
    }
    fn start(&mut self) {
        if self.query.is_none() {
            self.query = Some(TextInput::default());
            self.cursor = 0;
        }
        self.focused = true;
    }
    fn push(&mut self, c: char) {
        if let Some(ref mut q) = self.query {
            q.insert_char(c);
            self.cursor = 0;
        }
    }
    fn handle_nav(&mut self, code: KeyCode) -> bool {
        if let Some(ref mut q) = self.query
            && q.handle_nav(code)
        {
            self.cursor = 0;
            return true;
        }
        false
    }
    fn cursor_down(&mut self, list_len: usize) {
        let max = list_len.saturating_sub(1);
        self.cursor = (self.cursor + 1).min(max);
    }
    fn cursor_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }
}

/// Single-line text input with caret movement. Used by every modal/dialog text
/// box (command palette, rename, file picker, schema search, add-connection)
/// so cursor behavior is uniform across the app.
#[derive(Clone, Default)]
struct TextInput {
    text: String,
    /// Caret position as a char index into `text`.
    cursor: usize,
}

impl TextInput {
    fn from_str(s: &str) -> Self {
        Self {
            text: s.to_string(),
            cursor: s.chars().count(),
        }
    }
    fn char_count(&self) -> usize {
        self.text.chars().count()
    }
    fn byte_at(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len())
    }
    fn insert_char(&mut self, c: char) {
        let b = self.byte_at(self.cursor);
        self.text.insert(b, c);
        self.cursor += 1;
    }
    /// Insert a string at the caret, advancing the caret past the end
    /// of the inserted text. Used by the bracketed-paste handler so a
    /// paste into a prompt lands as one chunk instead of N keystrokes.
    fn insert_str(&mut self, s: &str) {
        let b = self.byte_at(self.cursor);
        self.text.insert_str(b, s);
        self.cursor += s.chars().count();
    }
    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = self.byte_at(self.cursor);
        let start = self.byte_at(self.cursor - 1);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }
    fn delete(&mut self) {
        if self.cursor >= self.char_count() {
            return;
        }
        let start = self.byte_at(self.cursor);
        let end = self.byte_at(self.cursor + 1);
        self.text.replace_range(start..end, "");
    }
    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }
    fn right(&mut self) {
        if self.cursor < self.char_count() {
            self.cursor += 1;
        }
    }
    fn home(&mut self) {
        self.cursor = 0;
    }
    fn end(&mut self) {
        self.cursor = self.char_count();
    }
    /// Try to handle a navigation/edit key. Returns true if consumed.
    /// Char insertion is handled by the caller so it can layer chord logic.
    fn handle_nav(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Left => {
                self.left();
                true
            }
            KeyCode::Right => {
                self.right();
                true
            }
            KeyCode::Home => {
                self.home();
                true
            }
            KeyCode::End => {
                self.end();
                true
            }
            KeyCode::Backspace => {
                self.backspace();
                true
            }
            KeyCode::Delete => {
                self.delete();
                true
            }
            _ => false,
        }
    }
}

/// State for the leader+space file picker overlay.
#[derive(Clone, Default)]
struct FilePicker {
    query: TextInput,
    cursor: usize,
}

impl FilePicker {
    /// Filter `names` by fuzzy subsequence match against the query, ranked by
    /// the span of the match (tighter is better). Empty query returns all.
    fn matches<'a>(&self, names: &'a [String]) -> Vec<&'a String> {
        if self.query.text.is_empty() {
            return names.iter().collect();
        }
        let q: Vec<char> = self.query.text.to_lowercase().chars().collect();
        let mut scored: Vec<(usize, &String)> = names
            .iter()
            .filter_map(|n| fuzzy_score(&q, n).map(|s| (s, n)))
            .collect();
        scored.sort_by_key(|(s, _)| *s);
        scored.into_iter().map(|(_, n)| n).collect()
    }
}

/// Subsequence match: returns Some(span) where span = last_idx - first_idx.
/// Returns None if not all query chars appear in order.
fn fuzzy_score(q: &[char], name: &str) -> Option<usize> {
    let lower: Vec<char> = name.to_lowercase().chars().collect();
    let mut qi = 0;
    let mut first: Option<usize> = None;
    let mut last = 0;
    for (i, c) in lower.iter().enumerate() {
        if qi < q.len() && *c == q[qi] {
            if first.is_none() {
                first = Some(i);
            }
            last = i;
            qi += 1;
        }
    }
    if qi == q.len() {
        Some(last - first.unwrap_or(0))
    } else {
        None
    }
}

pub struct TuiProvider;

impl UiProvider for TuiProvider {
    fn run(state: Arc<Mutex<AppState>>) -> anyhow::Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async_run(state))
    }
}

async fn async_run(state: Arc<Mutex<AppState>>) -> anyhow::Result<()> {
    let theme_err = theme::load();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Inside tmux, wrap the Kitty keyboard protocol enable sequence in a DCS
    // passthrough so the outer terminal receives it; tmux itself silently
    // drops bare CSI > u. Requires `set -g allow-passthrough on` in tmux.
    let in_tmux = std::env::var_os("TMUX").is_some();
    if in_tmux {
        use std::io::Write;
        stdout.write_all(b"\x1bPtmux;\x1b\x1b[>1u\x1b\\")?;
        stdout.flush()?;
    }
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let keybinding_mode = state.lock().unwrap().keybinding_mode;
    let result = run_loop(&mut terminal, state, keybinding_mode, theme_err).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        SetCursorStyle::DefaultUserShape,
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture
    )?;
    if in_tmux {
        use std::io::Write;
        let mut out = io::stdout();
        out.write_all(b"\x1bPtmux;\x1b\x1b[<u\x1b\\")?;
        out.flush()?;
    }
    terminal.show_cursor()?;
    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: Arc<Mutex<AppState>>,
    keybinding_mode: KeybindingMode,
    theme_error: Option<String>,
) -> anyhow::Result<()> {
    let mut editor = Editor::new(keybinding_mode);
    let highlight_thread = HighlightThread::spawn()?;
    let completion_thread = CompletionThread::spawn()?;

    // Start LSP client if binary is configured and reachable
    let scratch_path = std::env::temp_dir().join("sqeel-scratch.sql");
    // Build a file:// URI from the OS temp path (works on Windows and Unix)
    let scratch_uri_str = {
        let p = scratch_path.to_string_lossy();
        if p.starts_with('/') {
            format!("file://{p}")
        } else {
            // Windows: C:\... → file:///C:/...
            format!("file:///{}", p.replace('\\', "/"))
        }
    };
    let scratch_uri: lsp_types::Uri = scratch_uri_str
        .parse()
        .unwrap_or_else(|_| "file:///tmp/sqeel-scratch.sql".parse().unwrap());
    let main_config = load_main_config().ok().unwrap_or_default();
    let lsp_binary = main_config.editor.lsp_binary.clone();
    let mouse_scroll_lines = main_config.editor.mouse_scroll_lines;
    let leader_char: char = main_config.editor.leader_key.chars().next().unwrap_or(' ');
    let lsp_start_result = LspClient::start(&lsp_binary, None, &[]).await;
    if let Ok(path) = std::env::var("SQEEL_DEBUG_HL_DUMP") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            match &lsp_start_result {
                Ok(_) => {
                    let _ = writeln!(f, "### lsp started: binary={lsp_binary}");
                }
                Err(e) => {
                    let _ = writeln!(f, "### lsp FAILED to start: binary={lsp_binary} err={e}");
                }
            }
        }
    }
    let mut lsp: Option<LspClient> = lsp_start_result.ok();
    if let Some(ref mut client) = lsp {
        let _ = client.open_document(scratch_uri.clone(), "").await;
    }
    {
        let mut s = state.lock().unwrap();
        s.lsp_available = lsp.is_some();
        s.lsp_binary = lsp_binary.clone();
    }

    // LSP restarts happen off the main loop (process spawn + initialize
    // handshake costs 100-500ms each). The main loop parks a config
    // path here, a helper task consumes it, and the finished
    // `LspClient` is shipped back through this channel to be swapped in
    // without blocking the render loop.
    let (lsp_restart_tx, mut lsp_restart_rx) =
        tokio::sync::mpsc::unbounded_channel::<anyhow::Result<LspClient>>();
    let mut lsp_restart_in_flight = false;

    // Cold-tab content loads run on `spawn_blocking` so a large file
    // or slow filesystem doesn't freeze the render loop on a tab
    // switch. Finished (tab_index, content) pairs arrive here.
    let (tab_load_tx, mut tab_load_rx) = tokio::sync::mpsc::unbounded_channel::<(usize, String)>();

    // Schema tree-flatten rebuilds run on `spawn_blocking` too — big
    // schemas (hundreds of tables × thousands of columns) allocate a
    // String per node and used to freeze the render loop when the
    // sidebar first populated.
    type SchemaCacheResult = (
        Vec<sqeel_core::schema::SchemaTreeItem>,
        Vec<sqeel_core::schema::SchemaTreeItem>,
        Vec<String>,
    );
    let (schema_cache_tx, mut schema_cache_rx) =
        tokio::sync::mpsc::unbounded_channel::<SchemaCacheResult>();

    let mut editor_dirty = false;
    // Prompt asking the user whether to save dirty buffers before exit.
    let mut quit_prompt: Option<()> = None;
    // Debounce the expensive content pipeline (full-buffer `String` build,
    // tree-sitter re-parse, LSP `didChange`, completion request).  Set when
    // the editor flags a change, cleared on publish.  On huge files this
    // collapses a burst of keystrokes into a single pipeline run.
    let mut content_dirty_since: Option<Instant> = None;
    // Last viewport top row we submitted to the highlight thread.  Seeded
    // to `usize::MAX` so the first iteration always triggers an initial
    // window highlight.
    let mut last_highlight_top: usize = usize::MAX;
    // Dialect of the last highlight submission. When the DB connection
    // resolves and `active_dialect` flips from `Generic` to a concrete
    // dialect, force a re-submit so the worker re-parses with the right
    // dialect-specific keyword promotions.
    let mut last_highlight_dialect: sqeel_core::highlight::Dialect =
        sqeel_core::highlight::Dialect::Generic;
    // Cached last highlight result so we can re-apply marker overlays
    // when the cursor line or insert-mode flips, without re-parsing.
    let mut last_highlight_result: Option<HighlightResult> = None;
    let mut last_marker_cursor_row: usize = usize::MAX;
    let mut last_marker_diag_len: usize = usize::MAX;
    let mut doc_version: i32 = 0;
    // Buffers larger than this are not streamed to the LSP — sqls (and most
    // SQL LSPs) re-parse the whole document on every `didChange` and balloon
    // to multi-GB RAM on huge dumps / seed files.  We still highlight +
    // offer schema completions locally; only the LSP-sourced completions +
    // diagnostics go dark above the threshold.
    const LSP_MAX_BYTES: usize = 512 * 1024;
    // True when we've sent an empty `didChange` to release the LSP's copy
    // of the document after crossing the size threshold.  Reset when we
    // drop back below the threshold so the server re-syncs the real text.
    let mut lsp_suspended = false;
    let mut last_completion_id: Option<i64> = None;
    // Most recent hover request id; responses with different ids are
    // ignored (stale request raced a newer one or the popup was
    // dismissed before the server answered).
    let mut last_hover_id: Option<i64> = None;
    let mut last_schema_completions: Vec<String> = Vec::new();
    // Last completion context + prefix, stashed so we can re-run the query
    // once a lazy schema load fills in tables/columns for that context.
    let mut last_completion_ctx: Option<(CompletionCtx, String)> = None;
    let mut last_pending_loads: usize = 0;
    let mut command_input: Option<TextInput> = None;
    let mut rename_input: Option<TextInput> = None;
    let mut file_picker: Option<FilePicker> = None;
    let mut delete_confirm: Option<String> = None;
    let mut schema_search =
        SchemaSearch::from_initial(state.lock().unwrap().schema_search_query.clone());

    let mut toasts: Vec<(String, ToastKind, std::time::Instant)> = Vec::new();
    if let Some(msg) = theme_error {
        toasts.push((msg, ToastKind::Error, std::time::Instant::now()));
    }
    let mut last_esc_at: Option<std::time::Instant> = None;
    // Leader-key chord state. Set when the leader is pressed in an eligible
    // context; cleared when the next key resolves the chord or it times out.
    let mut leader_pending_at: Option<std::time::Instant> = None;
    // Unified clipboard sink: native OS clipboard + OSC 52 fallback over SSH.
    let mut clipboard = Clipboard::new();
    // Tracks an unfinished `y` in the results pane so a follow-up `y` within
    // 500ms yanks the whole row (vim `yy`).
    let mut pending_results_y: Option<std::time::Instant> = None;
    // Mouse drag tracking
    let mut last_draw_areas = DrawAreas::default();
    let mut mouse_drag_pane: Option<Focus> = None;
    let mut mouse_did_drag = false;
    // Anchor cell captured on mouse-down over a grid (results / hover).
    // Promoted to a visual-block selection on the first drag event;
    // `None` means the press didn't land on a selectable cell.
    let mut mouse_drag_anchor: Option<(usize, usize)> = None;
    // Holds an event the drag coalescer peeked-past but couldn't
    // swallow, so the next iteration processes it instead of calling
    // `event::read` again.
    let mut pending_event: Option<Event> = None;
    // Last cursor shape sent to the terminal. Each `SetCursorStyle`
    // emit is an ANSI escape → blocking write to stdout; skip it when
    // the shape hasn't changed since the last draw.
    let mut last_cursor_shape: Option<CursorShape> = None;
    // Force redraw on first iteration and after every event.
    let mut event_triggered_redraw = true;
    // Last time we ran the schema-freshness sweep. Rate-limited to once a
    // second so we don't walk the tree every tick.
    let mut last_stale_check = Instant::now();
    let mut last_terminal_size = terminal.size()?;
    let mut last_schema_loading = false;
    // Pending first `g` for the schema-pane `gg` chord. Cleared by any other key.
    let mut schema_g_pending = false;
    // Pending first `g` for the results-pane `gg` chord.
    let mut results_g_pending = false;
    // Running count prefix for results-pane nav (digits before j/k/h/l).
    // `0` is context-dependent: a leading `0` is "row start", an `0` mid-
    // count is a digit. Cleared after the next nav keystroke.
    let mut results_count: usize = 0;
    // Live `/` prompt over the results pane. `Some` while the user is
    // typing; commit stashes the pattern and clears this.
    let mut results_search_prompt: Option<TextInput> = None;
    // Most recent committed results-pane search pattern — kept so
    // `n` / `N` have something to repeat after the prompt closes.
    let mut results_search_pattern: Option<String> = None;
    loop {
        let mut needs_redraw = event_triggered_redraw;
        event_triggered_redraw = false;

        // Expire toasts after 5 seconds each.
        let before = toasts.len();
        toasts.retain(|(_, _, t)| t.elapsed() < Duration::from_millis(5000));
        if toasts.len() != before {
            needs_redraw = true;
        }

        // Detect terminal size changes that don't produce Event::Resize (e.g. fullscreen toggle).
        if let Ok(size) = terminal.size()
            && size != last_terminal_size
        {
            last_terminal_size = size;
            terminal.autoresize()?;
            needs_redraw = true;
        }

        // Drain async-task result channels without touching the state
        // lock (they're pure mpsc try_recv). We apply them below in one
        // consolidated lock block so per-iter lock pressure stays low.
        let drained_tab_loads: Vec<(usize, String)> =
            std::iter::from_fn(|| tab_load_rx.try_recv().ok()).collect();
        let drained_schema_caches: Vec<(Vec<_>, Vec<_>, Vec<_>)> =
            std::iter::from_fn(|| schema_cache_rx.try_recv().ok()).collect();
        if !drained_tab_loads.is_empty() || !drained_schema_caches.is_empty() {
            needs_redraw = true;
        }

        // Single top-of-iter lock: apply drained channel results, run
        // periodic maintenance, and take any pending tasks/content.
        // Dropping to ~1 lock cycle here instead of 5+ reduces per-event
        // contention with the highlight / executor worker threads.
        let (pending_load, pending_tab_content) = {
            let mut s = state.lock().unwrap();
            for (tab_index, content) in drained_tab_loads {
                s.apply_loaded_tab_content(tab_index, content);
            }
            for (items, all, ids) in drained_schema_caches {
                s.apply_schema_cache_rebuild(items, all, ids);
            }
            s.evict_cold_tabs();
            let pending_load = s.pending_tab_load.take();
            let pending_tab_content = s.tab_content_pending.take();
            (pending_load, pending_tab_content)
        };

        // Kick off any pending cold-tab disk read on a blocking task
        // so a big file / slow FS doesn't stall the render loop.
        if let Some(load) = pending_load {
            let tx = tab_load_tx.clone();
            tokio::task::spawn_blocking(move || {
                let content =
                    sqeel_core::persistence::load_query(&load.slug, &load.name).unwrap_or_default();
                let _ = tx.send((load.tab_index, content));
            });
        }

        // Apply pending tab content (set when connection loads or tab switches).
        {
            if let Some(content) = pending_tab_content {
                editor.set_content(&content);
                // `set_content` flips the editor's dirty flag internally
                // (textarea rebuild). Consume it here so the main-loop
                // `take_dirty()` below doesn't mistake the programmatic
                // load for a user edit and mark the tab dirty.
                let _ = editor.take_dirty();
                editor_dirty = false;
                last_highlight_top = usize::MAX;
                needs_redraw = true;
                // Sync the LSP with the freshly loaded buffer so sqls can
                // emit diagnostics even when the user never touches the
                // editor after open / tab-switch.
                if let Some(ref client) = lsp
                    && content.len() <= LSP_MAX_BYTES
                {
                    // Fire the didChange off the render loop — serialize +
                    // send of a multi-MB buffer would otherwise freeze the
                    // UI for hundreds of ms the moment a large scratch
                    // loads.
                    doc_version += 1;
                    let writer = client.writer();
                    let uri = scratch_uri.clone();
                    let version = doc_version;
                    let text = std::sync::Arc::new(content.clone());
                    let debug_path = std::env::var("SQEEL_DEBUG_HL_DUMP").ok();
                    tokio::spawn(async move {
                        let _ = writer.change_document(uri, version, &text).await;
                        if let Some(path) = debug_path {
                            use std::io::Write;
                            if let Ok(mut f) = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&path)
                            {
                                let preview: String = text.chars().take(80).collect();
                                let _ = writeln!(
                                    f,
                                    "### lsp didChange (tab-load) v{version} bytes={} preview={preview:?}",
                                    text.len()
                                );
                            }
                        }
                    });
                    lsp_suspended = false;
                }
            }
        }

        // Sync editor content + submit to highlight thread when changed.
        // Cheap per-keystroke work stays here; the expensive full-buffer
        // `String` build + highlight + LSP + completion submission is
        // debounced below.
        let content_changed = editor.take_dirty();
        if content_changed {
            needs_redraw = true;
            editor_dirty = true;
            if content_dirty_since.is_none() {
                content_dirty_since = Some(Instant::now());
            }
            // mark_active_dirty is folded into the main lock block below.
        }
        // Trailing-edge debounce: publish once the dirty window has aged
        // past the threshold.  The 50 ms event-poll timeout guarantees we
        // revisit this branch quickly even while the user is idle.
        const CONTENT_PUBLISH_DEBOUNCE: Duration = Duration::from_millis(75);
        // Above this total-byte size we stop running the heavy pipeline
        // entirely — no `editor.content()` join, no highlight submit, no
        // completion context parse.  Syntax spans already applied stay
        // rendered; the editor keeps working as a plain text buffer.
        const HEAVY_PIPELINE_MAX_BYTES: usize = 2 * 1024 * 1024;
        let should_publish = content_dirty_since
            .map(|t| t.elapsed() >= CONTENT_PUBLISH_DEBOUNCE)
            .unwrap_or(false);
        let buffer_bytes = if should_publish {
            let lines = editor.textarea.lines();
            lines.iter().map(|l| l.len()).sum::<usize>() + lines.len()
        } else {
            0
        };
        let content: Option<Arc<String>> =
            if should_publish && buffer_bytes <= HEAVY_PIPELINE_MAX_BYTES {
                content_dirty_since = None;
                Some(editor.content_arc())
            } else if should_publish {
                // Over the size gate — clear the dirty timer so we don't
                // re-enter every iteration, and drop any completion popup so
                // the user isn't staring at stale suggestions.
                content_dirty_since = None;
                state.lock().unwrap().dismiss_completions();
                last_completion_id = None;
                last_completion_ctx = None;
                None
            } else {
                None
            };
        // Merged into the single big lock below: extracted here so
        // downstream code (highlight resubmit gate) can consume it
        // without reacquiring the lock.
        let current_dialect;
        {
            let mut s = state.lock().unwrap();
            current_dialect = s.active_dialect;
            if content_changed {
                s.mark_active_dirty();
            }
            // Kick the schema-cache rebuild off the render loop when
            // it's stale and nothing else is already rebuilding. The
            // snapshot + flatten work runs on a blocking task; the
            // finished caches come back through `schema_cache_rx`
            // below and get applied via `apply_schema_cache_rebuild`.
            if let Some(nodes) = s.schema_snapshot_for_rebuild() {
                let tx = schema_cache_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let items = sqeel_core::schema::flatten_tree(&nodes);
                    let all = sqeel_core::schema::flatten_all(&nodes);
                    let mut ids: Vec<String> = Vec::new();
                    let mut stack: Vec<&sqeel_core::schema::SchemaNode> = nodes.iter().collect();
                    while let Some(node) = stack.pop() {
                        ids.push(node.name().to_owned());
                        match node {
                            sqeel_core::schema::SchemaNode::Database { tables, .. } => {
                                stack.extend(tables.iter())
                            }
                            sqeel_core::schema::SchemaNode::Table { columns, .. } => {
                                stack.extend(columns.iter())
                            }
                            sqeel_core::schema::SchemaNode::Column { .. } => {}
                        }
                    }
                    ids.sort();
                    ids.dedup();
                    let _ = tx.send((items, all, ids));
                });
                needs_redraw = true;
            }
            // Leaving the schema pane exits search mode entirely.
            if s.focus != Focus::Schema && schema_search.query.is_some() {
                schema_search.clear();
                needs_redraw = true;
            }
            s.vim_mode = editor.vim_mode();
            s.schema_search_query = schema_search.query().map(|q| q.to_string());
            if let Some(ref c) = content {
                s.editor_content = c.clone();
                s.editor_content_synced = true;
            }
            // Apply any completed highlight results from the background
            // thread.  The thread parses a viewport-sized slice; splice
            // those spans into the existing row table in place so we
            // never allocate a fresh outer `Vec<Vec<…>>` the size of the
            // whole buffer on the main thread.
            if let Some(result) = highlight_thread.try_recv() {
                let row_count = editor.textarea.lines().len();
                let cursor_row = editor.textarea.cursor().0;
                let diagnostics = merged_diagnostics(&s.lsp_diagnostics, &result.parse_errors);
                apply_window_spans(
                    &mut editor.textarea,
                    &result,
                    row_count,
                    cursor_row,
                    &diagnostics,
                );
                s.set_highlights(result.spans.clone());
                last_marker_cursor_row = cursor_row;
                last_marker_diag_len = diagnostics.len();
                last_highlight_result = Some(result);
                needs_redraw = true;
            } else {
                // Cursor moved onto a different row OR diagnostics
                // changed: re-apply the cached highlight so the cursor
                // -line blending and diagnostic underlines update
                // without paying another tree-sitter parse.
                //
                // Skip while the user is mid-mouse-drag: every pixel-
                // row crossing during a drag would otherwise trigger a
                // full window splice (O(window_rows)), dominating the
                // drag event loop and producing visible selection lag.
                // The visual selection still renders via the post-
                // render overlay; only the cursor-line marker re-
                // blending is deferred until the drag ends.
                let cursor_row = editor.textarea.cursor().0;
                let dragging_editor = mouse_drag_pane == Some(Focus::Editor);
                if let Some(result) = last_highlight_result.as_ref()
                    && !dragging_editor
                {
                    let diagnostics = merged_diagnostics(&s.lsp_diagnostics, &result.parse_errors);
                    if cursor_row != last_marker_cursor_row
                        || diagnostics.len() != last_marker_diag_len
                    {
                        let row_count = editor.textarea.lines().len();
                        apply_window_spans(
                            &mut editor.textarea,
                            result,
                            row_count,
                            cursor_row,
                            &diagnostics,
                        );
                        last_marker_cursor_row = cursor_row;
                        last_marker_diag_len = diagnostics.len();
                        needs_redraw = true;
                    }
                }
            }
        }
        // Highlight the viewport window (with margin) rather than the
        // whole buffer.  Triggered on any edit or when the viewport has
        // scrolled past half the margin since last submit — bounded cost
        // regardless of buffer size, so no heavy-pipeline gate here.
        // The highlight thread coalesces (latest-wins) so bursts are cheap.
        const HIGHLIGHT_WINDOW_MARGIN: usize = 500;
        let viewport_top = editor.textarea.viewport_top_row();
        let viewport_height = editor.viewport_height_value() as usize;
        let viewport_scrolled = last_highlight_top == usize::MAX
            || viewport_top.abs_diff(last_highlight_top) >= HIGHLIGHT_WINDOW_MARGIN / 2;
        let should_submit = should_resubmit_highlight(
            content_changed,
            viewport_scrolled,
            current_dialect,
            last_highlight_dialect,
        );
        if should_submit && viewport_height > 0 {
            let lines = editor.textarea.lines();
            if !lines.is_empty() {
                let start = viewport_top.saturating_sub(HIGHLIGHT_WINDOW_MARGIN);
                let end =
                    (viewport_top + viewport_height + HIGHLIGHT_WINDOW_MARGIN).min(lines.len());
                let slice = &lines[start..end];
                let mut src = String::with_capacity(slice.iter().map(|l| l.len() + 1).sum());
                for (i, l) in slice.iter().enumerate() {
                    if i > 0 {
                        src.push('\n');
                    }
                    src.push_str(l);
                }
                highlight_thread.submit(Arc::new(src), start, slice.len(), current_dialect);
                last_highlight_dialect = current_dialect;
                last_highlight_top = viewport_top;
            }
        }

        // Auto-complete: on every content change, submit a schema completion query to the
        // background thread and (if LSP is available) request supplemental completions.
        // Gate on Insert mode — popping up completions while the user is in
        // Normal / Visual / any-visual mode is always a distraction.
        if let Some(ref content) = content {
            doc_version += 1;

            let (row, col) = editor.textarea.cursor();

            // Suppress completions after `;` or on empty buffer. Whitespace
            // only suppresses when ctx is `Any` — inside Table/Column/Qualified
            // contexts, an empty prefix should still surface candidates (e.g.
            // right after `where `).
            let char_left = editor.textarea.lines().get(row).and_then(|line| {
                let before = &line[..col.min(line.len())];
                before.chars().next_back()
            });
            let hard_suppress = matches!(char_left, Some(';')) || char_left.is_none();

            let prefix = word_prefix_at(editor.textarea.lines(), row, col);
            let byte_offset = row_col_to_byte(editor.textarea.lines(), row, col);
            let ctx = completion_ctx::parse_context(content, byte_offset);

            let whitespace_left = matches!(char_left, Some(c) if c.is_whitespace());
            let mode_is_insert = editor.vim_mode() == VimMode::Insert;
            let suppress = !mode_is_insert
                || hard_suppress
                || (whitespace_left && matches!(ctx, CompletionCtx::Any));

            if suppress {
                state.lock().unwrap().dismiss_completions();
                last_completion_id = None;
                last_completion_ctx = None;
            } else {
                // Context-scoped pool (unfiltered) fed to the prefix-filter
                // thread; empty prefix returns the full sorted pool.
                let (pool, _) = {
                    let mut s = state.lock().unwrap();
                    s.lazy_load_for_context(&ctx);
                    let pool = s.completions_for_context(&ctx, "");
                    (pool, ())
                };
                last_completion_ctx = Some((ctx, prefix.clone()));
                completion_thread.submit(prefix, Arc::new(pool));

                if let Some(ref mut client) = lsp {
                    let too_big = content.len() > LSP_MAX_BYTES;
                    if too_big {
                        // First crossing: release the LSP's in-memory copy
                        // once so sqls can free whatever it parsed, then go
                        // silent until the buffer shrinks again.
                        if !lsp_suspended {
                            let _ = client
                                .change_document(scratch_uri.clone(), doc_version, "")
                                .await;
                            lsp_suspended = true;
                        }
                    } else {
                        if lsp_suspended {
                            lsp_suspended = false;
                        }
                        // Fire didChange from a spawned task so the
                        // per-keystroke JSON serialization of the full
                        // buffer doesn't block the render loop on
                        // multi-MB files. The LSP write-queue is an
                        // mpsc so latest-wins coalescing still happens
                        // on the other side.
                        let writer = client.writer();
                        let uri = scratch_uri.clone();
                        let version = doc_version;
                        let text = Arc::clone(content);
                        let debug_path = std::env::var("SQEEL_DEBUG_HL_DUMP").ok();
                        tokio::spawn(async move {
                            let _ = writer.change_document(uri, version, &text).await;
                            if let Some(path) = debug_path {
                                use std::io::Write;
                                if let Ok(mut f) = std::fs::OpenOptions::new()
                                    .create(true)
                                    .append(true)
                                    .open(&path)
                                {
                                    let preview: String = text.chars().take(80).collect();
                                    let _ = writeln!(
                                        f,
                                        "### lsp didChange v{version} bytes={} preview={preview:?}",
                                        text.len()
                                    );
                                }
                            }
                        });
                        // Fire the completion request off the render
                        // loop too — we get the id synchronously from
                        // the shared counter, the serialize + send run
                        // in a spawned task.
                        let id = client.writer().request_completion(
                            scratch_uri.clone(),
                            row as u32,
                            col as u32,
                        );
                        last_completion_id = Some(id);
                    }
                }
            }
        }

        // Leaving Insert mode: drop any lingering popup so the user
        // isn't stuck with stale completions while navigating in Normal.
        if editor.vim_mode() != VimMode::Insert {
            let mut s = state.lock().unwrap();
            if s.show_completions {
                s.dismiss_completions();
                last_completion_id = None;
                last_completion_ctx = None;
                needs_redraw = true;
            }
        }

        // Poll schema completion thread results.
        if let Some(schema_items) = completion_thread.try_recv() {
            last_schema_completions = schema_items.clone();
            state.lock().unwrap().set_completions(schema_items);
            needs_redraw = true;
        }

        // When a DB connection resolves, `connect_and_spawn` writes a
        // sqls config file and parks the path on the state. Spawn the
        // LSP restart off the main loop so the 100-500ms process spawn
        // + initialize handshake doesn't freeze render. The finished
        // client ships back via `lsp_restart_rx`.
        if !lsp_restart_in_flight {
            let pending_cfg = state.lock().unwrap().pending_sqls_config.take();
            if let Some(cfg_path) = pending_cfg {
                lsp = None; // kill_on_drop SIGKILLs the previous sqls
                let args: Vec<String> =
                    vec!["-config".into(), cfg_path.to_string_lossy().into_owned()];
                let binary = lsp_binary.clone();
                let tx = lsp_restart_tx.clone();
                lsp_restart_in_flight = true;
                tokio::spawn(async move {
                    let result = LspClient::start(&binary, None, &args).await;
                    if let Ok(path) = std::env::var("SQEEL_DEBUG_HL_DUMP") {
                        use std::io::Write;
                        if let Ok(mut f) = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&path)
                        {
                            match &result {
                                Ok(_) => {
                                    let _ =
                                        writeln!(f, "### lsp restarted with config={cfg_path:?}");
                                }
                                Err(e) => {
                                    let _ = writeln!(
                                        f,
                                        "### lsp restart FAILED config={cfg_path:?} err={e}"
                                    );
                                }
                            }
                        }
                    }
                    let _ = tx.send(result);
                });
            }
        }
        // Swap in a finished restart if one is ready. The `open_document`
        // call is cheap (just writes to the child's stdin) so leave it
        // inline.
        while let Ok(result) = lsp_restart_rx.try_recv() {
            lsp_restart_in_flight = false;
            if let Ok(mut client) = result {
                let content = editor.content();
                let _ = client.open_document(scratch_uri.clone(), &content).await;
                doc_version = 1;
                // Warm-up hover request right after opening the doc.
                // sqls fetches the DB schema on its first
                // symbol-resolution request, which would otherwise
                // penalise the user's *real* first `K` by several
                // hundred ms. Firing it now paves the cache before
                // the user interacts. Response is discarded — we
                // don't set `last_hover_id`, so the TUI arm's id
                // check drops the payload silently.
                let _ = client.writer().request_hover(scratch_uri.clone(), 0, 0);
                lsp = Some(client);
                lsp_suspended = false;
                needs_redraw = true;
            }
        }

        // Drain LSP events
        if let Some(ref mut client) = lsp {
            while let Ok(event) = client.events.try_recv() {
                needs_redraw = true;
                match event {
                    LspEvent::Diagnostics(diags) => {
                        if let Ok(path) = std::env::var("SQEEL_DEBUG_HL_DUMP") {
                            use std::io::Write;
                            if let Ok(mut f) = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&path)
                            {
                                let _ = writeln!(
                                    f,
                                    "### lsp diagnostics received ({} items)",
                                    diags.len()
                                );
                                for d in &diags {
                                    let _ = writeln!(
                                        f,
                                        "  {}:{} .. {}:{} [{:?}] {}",
                                        d.line, d.col, d.end_line, d.end_col, d.severity, d.message
                                    );
                                }
                            }
                        }
                        state.lock().unwrap().set_diagnostics(diags);
                    }
                    LspEvent::Hover(id, text) => {
                        if Some(id) == last_hover_id {
                            let mut s = state.lock().unwrap();
                            // Both forms capture focus so the popup
                            // stays interactive: tables for cell nav +
                            // yank, plain text for scroll + Esc.
                            if let Some(table) = AppState::parse_hover_table(&text) {
                                s.open_hover_table(table);
                            } else {
                                s.open_hover_text(text);
                            }
                            needs_redraw = true;
                        }
                    }
                    LspEvent::Completion(id, lsp_items) => {
                        if Some(id) == last_completion_id {
                            // LSP results lead; schema identifiers fill in any gaps.
                            let mut merged = lsp_items;
                            let seen: std::collections::HashSet<&str> =
                                merged.iter().map(String::as_str).collect();
                            let extras: Vec<String> = last_schema_completions
                                .iter()
                                .filter(|item| !seen.contains(item.as_str()))
                                .cloned()
                                .collect();
                            merged.extend(extras);
                            state.lock().unwrap().set_completions(merged);
                        }
                    }
                }
            }
        }

        // Single end-of-iter lock: schema loading flags, periodic stale
        // sweep, and results_dirty — collapsed from four separate
        // lock/unlock cycles so drag frames don't pay lock thrash here.
        let stale_due = last_stale_check.elapsed() >= Duration::from_secs(1);
        let (schema_loading, pending_loads, lazy_pool) = {
            let mut s = state.lock().unwrap();
            if stale_due {
                s.refresh_stale_schema();
            }
            if s.results_dirty {
                needs_redraw = true;
                s.results_dirty = false;
            }
            let pending_loads = s.schema_pending_loads;
            // If lazy schema loads just drained, stage a fresh completion
            // pool under the same lock to avoid reacquiring below.
            let lazy_pool = if last_pending_loads > 0
                && pending_loads < last_pending_loads
                && let Some((ctx, prefix)) = last_completion_ctx.clone()
            {
                Some((prefix, s.completions_for_context(&ctx, "")))
            } else {
                None
            };
            // Hover-loading state piggybacks on the same tick — its
            // spinner needs periodic redraws to animate while we
            // wait on the LSP response.
            if s.hover_loading {
                needs_redraw = true;
            }
            (s.schema_loading, pending_loads, lazy_pool)
        };
        if stale_due {
            last_stale_check = Instant::now();
        }
        if schema_loading || last_schema_loading != schema_loading {
            needs_redraw = true;
        }
        last_schema_loading = schema_loading;
        if let Some((prefix, pool)) = lazy_pool {
            completion_thread.submit(prefix, Arc::new(pool));
        }
        last_pending_loads = pending_loads;

        if needs_redraw {
            // Only the two things that can't be referenced directly
            // (need the state lock or a type adapt) get snapshotted;
            // everything else passes through by ref.
            let quit_prompt_dirty: Option<Vec<String>> = quit_prompt
                .as_ref()
                .map(|_| state.lock().unwrap().dirty_tab_names());
            // toast_snap only materializes when there are toasts to
            // render — the empty case is the common one during drag /
            // steady-state and skipping the Vec alloc keeps per-frame
            // work minimal.
            let toast_snap: Vec<(String, ToastKind)> = if toasts.is_empty() {
                Vec::new()
            } else {
                toasts
                    .iter()
                    .map(|(msg, kind, _)| (msg.clone(), *kind))
                    .collect()
            };
            let editor_search_text = editor.search_prompt().map(|p| p.text.as_str().to_owned());
            let last_editor_search = editor.last_search().map(str::to_owned);
            let results_search_text = results_search_prompt.as_ref().map(|p| p.text.clone());
            terminal.draw(|f| {
                let s = state.lock().unwrap();
                last_draw_areas = draw(
                    f,
                    &s,
                    &mut editor,
                    command_input.as_ref(),
                    rename_input.as_ref(),
                    file_picker.as_ref(),
                    delete_confirm.as_deref(),
                    quit_prompt_dirty.as_deref(),
                    &schema_search,
                    editor_search_text.as_deref(),
                    last_editor_search.as_deref(),
                    results_search_text.as_deref(),
                    &toast_snap,
                );
            })?;
            // Apply the cursor shape requested by draw(). Hidden is handled by
            // ratatui (no set_cursor_position call leaves the cursor hidden).
            // Skip the ANSI escape when the shape hasn't changed — this
            // runs on every frame otherwise and each emit is a blocking
            // stdout write.
            if last_cursor_shape != Some(last_draw_areas.cursor_shape) {
                match last_draw_areas.cursor_shape {
                    CursorShape::Bar => {
                        let _ = execute!(terminal.backend_mut(), SetCursorStyle::SteadyBar);
                    }
                    CursorShape::Block => {
                        let _ = execute!(terminal.backend_mut(), SetCursorStyle::SteadyBlock);
                    }
                    CursorShape::Hidden => {}
                }
                last_cursor_shape = Some(last_draw_areas.cursor_shape);
            }
            last_terminal_size = terminal.size()?;
        }

        let mut ev = if let Some(e) = pending_event.take() {
            e
        } else {
            if !event::poll(Duration::from_millis(50))? {
                continue;
            }
            event::read()?
        };

        // Coalesce consecutive mouse-drag events: the terminal can
        // emit them faster than we can redraw, so N queued drags mean
        // N full frame redraws for what visually is one cursor jump.
        // Drain intermediate drags (keeping the latest) and stash any
        // non-drag follow-up for the next loop iteration.
        if matches!(&ev, Event::Mouse(m) if matches!(m.kind, MouseEventKind::Drag(_))) {
            while event::poll(Duration::ZERO)? {
                let next = event::read()?;
                if matches!(&next, Event::Mouse(m) if matches!(m.kind, MouseEventKind::Drag(_))) {
                    ev = next;
                } else {
                    pending_event = Some(next);
                    break;
                }
            }
        }

        event_triggered_redraw = true;
        match ev {
            Event::Mouse(mouse) => {
                // Hover popup steals focus; a stray click on the
                // underlying editor would otherwise move `state.focus`
                // back out of `Focus::Hover` and break Esc/nav.
                // Clicks inside the popup's body area move the hover
                // cursor to the clicked cell; clicks outside the body
                // are swallowed so they don't leak to the editor.
                if state.lock().unwrap().focus == Focus::Hover {
                    match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            let mut s = state.lock().unwrap();
                            if let Some((row, col)) = s.hover_click_to_cell(mouse.column, mouse.row)
                            {
                                s.hover_cursor = ResultsCursor::Cell { row, col };
                                s.clamp_hover_scroll();
                                mouse_drag_anchor = Some((row, col));
                            } else {
                                mouse_drag_anchor = None;
                            }
                            mouse_drag_pane = Some(Focus::Hover);
                            mouse_did_drag = false;
                        }
                        MouseEventKind::Drag(MouseButton::Left) => {
                            if mouse_drag_pane == Some(Focus::Hover)
                                && let Some(anchor) = mouse_drag_anchor
                            {
                                use sqeel_core::state::{ResultsSelection, ResultsSelectionMode};
                                let mut s = state.lock().unwrap();
                                if !mouse_did_drag {
                                    s.hover_selection = Some(ResultsSelection {
                                        anchor,
                                        mode: ResultsSelectionMode::Block,
                                    });
                                }
                                if let Some((row, col)) =
                                    s.hover_drag_to_cell(mouse.column, mouse.row)
                                {
                                    s.hover_cursor = ResultsCursor::Cell { row, col };
                                    s.clamp_hover_scroll();
                                }
                            }
                            mouse_did_drag = true;
                        }
                        MouseEventKind::Up(MouseButton::Left) => {
                            mouse_drag_pane = None;
                            mouse_did_drag = false;
                            mouse_drag_anchor = None;
                        }
                        MouseEventKind::ScrollDown => {
                            state.lock().unwrap().hover_cursor_move(1, 0);
                        }
                        MouseEventKind::ScrollUp => {
                            state.lock().unwrap().hover_cursor_move(-1, 0);
                        }
                        _ => {}
                    }
                    continue;
                }
                // Help overlay swallows clicks / drags so they don't
                // fall through to whatever pane sits underneath — but
                // scroll events need to pass through so the user can
                // mouse-scroll the help content itself.
                if state.lock().unwrap().show_help
                    && !matches!(
                        mouse.kind,
                        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
                    )
                {
                    continue;
                }
                let area = terminal.size()?;
                let schema_width = (area.width * 15 / 100).max(30);
                let show_results = state.lock().unwrap().has_results();
                let editor_ratio = state.lock().unwrap().editor_ratio;
                let s = state.lock().unwrap();
                let bottom_rows = 1 + (!s.lsp_available) as u16;
                drop(s);
                let main_height = area.height.saturating_sub(bottom_rows);
                let editor_height = if show_results {
                    (main_height as f32 * editor_ratio) as u16
                } else {
                    main_height
                };

                // Determine which pane the mouse is over
                let pane = if mouse.column < schema_width {
                    Focus::Schema
                } else if show_results && mouse.row >= editor_height {
                    Focus::Results
                } else {
                    Focus::Editor
                };

                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        use ratatui::layout::Position;
                        let pos = Position {
                            x: mouse.column,
                            y: mouse.row,
                        };
                        if last_draw_areas.tab_bar.contains(pos) {
                            // Click on editor tab bar — determine which tab
                            let rel_x =
                                mouse.column.saturating_sub(last_draw_areas.tab_bar.x) as usize;
                            let clicked = {
                                let s = state.lock().unwrap();
                                let mut offset = 0usize;
                                let mut found = None;
                                for (i, tab) in s.tabs.iter().enumerate() {
                                    let w = tab.name.len()
                                        + 2
                                        + if i + 1 < s.tabs.len() { 1 } else { 0 };
                                    if rel_x < offset + w {
                                        found = Some(i);
                                        break;
                                    }
                                    offset += w;
                                }
                                found
                            };
                            if let Some(idx) = clicked {
                                let content = {
                                    let mut s = state.lock().unwrap();
                                    s.focus = Focus::Editor;
                                    if editor_dirty {
                                        s.editor_content = editor.content_arc();
                                        s.mark_active_dirty();
                                        editor_dirty = false;
                                    }
                                    s.switch_to_tab(idx);
                                    s.tab_content_pending.take()
                                };
                                if let Some(c) = content {
                                    editor.set_content(&c);
                                    let _ = editor.take_dirty();
                                    editor_dirty = false;
                                    last_highlight_top = usize::MAX;
                                }
                            } else {
                                state.lock().unwrap().focus = Focus::Editor;
                            }
                            mouse_did_drag = false;
                        } else if let Some(rtb) = last_draw_areas.results_tab_bar
                            && rtb.contains(pos)
                        {
                            // Click on results tab bar — select tab and focus results
                            let rel_x = mouse.column.saturating_sub(rtb.x) as usize;
                            let clicked = {
                                let s = state.lock().unwrap();
                                let mut offset = 0usize;
                                let mut found = None;
                                for (i, _tab) in s.result_tabs.iter().enumerate() {
                                    let label_w = format!(" {} ", i + 1).chars().count();
                                    let w =
                                        label_w + if i + 1 < s.result_tabs.len() { 1 } else { 0 };
                                    if rel_x < offset + w {
                                        found = Some(i);
                                        break;
                                    }
                                    offset += w;
                                }
                                found
                            };
                            if let Some(idx) = clicked {
                                let mut s = state.lock().unwrap();
                                s.active_result_tab = idx;
                                s.focus = Focus::Results;
                            }
                            mouse_did_drag = false;
                        } else {
                            let mut s = state.lock().unwrap();
                            s.focus = pane;
                            if pane == Focus::Schema {
                                let la = last_draw_areas.schema_list_area;
                                if mouse.row < la.y {
                                    // Click in the search box row: enter search mode.
                                    schema_search.start();
                                } else if mouse.row >= la.y
                                    && mouse.column >= la.x
                                    && mouse.column < la.x + la.width
                                {
                                    let rel = (mouse.row - la.y) as usize;
                                    let idx = rel + last_draw_areas.schema_list_offset;
                                    if last_draw_areas.schema_list_filtered {
                                        let query = schema_search.query().unwrap_or("");
                                        let filtered =
                                            schema::filter_items(s.all_schema_items(), query);
                                        if idx < filtered.len() {
                                            schema_search.cursor = idx;
                                            schema_search.focused = false;
                                            let path_str = schema::path_to_string(
                                                &filtered[idx].node_path,
                                                &s.schema_nodes,
                                            );
                                            s.restore_schema_cursor_by_path(&path_str);
                                            s.schema_toggle_current();
                                        }
                                    } else {
                                        let max = s.visible_schema_items().len();
                                        if idx < max {
                                            s.schema_cursor = idx;
                                            s.schema_toggle_current();
                                        }
                                    }
                                }
                            }
                            // Capture the anchor cell for a potential
                            // drag-select before we drop the lock. The
                            // first drag event promotes this to a live
                            // block selection; a plain click leaves
                            // the anchor unused.
                            mouse_drag_anchor = if pane == Focus::Results {
                                s.results_click_to_cell(mouse.column, mouse.row)
                            } else {
                                None
                            };
                            drop(s);
                            if pane == Focus::Editor {
                                editor.mouse_click(last_draw_areas.editor, mouse.column, mouse.row);
                            }
                            mouse_drag_pane = Some(pane);
                            mouse_did_drag = false;
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if mouse_drag_pane == Some(Focus::Editor) {
                            if !mouse_did_drag {
                                editor.mouse_begin_drag();
                            }
                            editor.mouse_extend_drag(
                                last_draw_areas.editor,
                                mouse.column,
                                mouse.row,
                            );
                        } else if mouse_drag_pane == Some(Focus::Results)
                            && let Some(anchor) = mouse_drag_anchor
                        {
                            // First drag frame: install a Block
                            // selection anchored at the mouse-down
                            // cell. Subsequent frames extend it by
                            // moving the results cursor.
                            use sqeel_core::state::{
                                ResultsCursor, ResultsSelection, ResultsSelectionMode,
                            };
                            let mut s = state.lock().unwrap();
                            if !mouse_did_drag {
                                let idx = s.active_result_tab;
                                if let Some(t) = s.result_tabs.get_mut(idx) {
                                    t.cursor = ResultsCursor::Cell {
                                        row: anchor.0,
                                        col: anchor.1,
                                    };
                                    t.selection = Some(ResultsSelection {
                                        anchor,
                                        mode: ResultsSelectionMode::Block,
                                    });
                                }
                            }
                            if let Some((row, col)) =
                                s.results_drag_to_cell(mouse.column, mouse.row)
                            {
                                let idx = s.active_result_tab;
                                if let Some(t) = s.result_tabs.get_mut(idx) {
                                    t.cursor = ResultsCursor::Cell { row, col };
                                }
                                s.clamp_results_cursor();
                            }
                        }
                        mouse_did_drag = true;
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if !mouse_did_drag && mouse_drag_pane == Some(Focus::Results) {
                            let click = {
                                let s = state.lock().unwrap();
                                extract_results_left_click(
                                    mouse.column,
                                    mouse.row,
                                    &last_draw_areas,
                                    &s,
                                )
                            };
                            if let Some((text, label, cur)) = click {
                                {
                                    let mut s = state.lock().unwrap();
                                    let idx = s.active_result_tab;
                                    if let Some(t) = s.result_tabs.get_mut(idx) {
                                        t.cursor = cur;
                                    }
                                    s.clamp_results_cursor();
                                }
                                let ok = clipboard.set_text(&text);
                                toasts.push((
                                    if ok {
                                        format!("{label} copied to clipboard")
                                    } else {
                                        format!("{label}: clipboard copy failed (too large)")
                                    },
                                    if ok {
                                        ToastKind::Info
                                    } else {
                                        ToastKind::Error
                                    },
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                        mouse_drag_pane = None;
                        mouse_did_drag = false;
                    }
                    MouseEventKind::Up(MouseButton::Right) => {
                        use ratatui::layout::Position;
                        let pos = Position {
                            x: mouse.column,
                            y: mouse.row,
                        };
                        if last_draw_areas.results.is_some_and(|r| r.contains(pos)) {
                            let s = state.lock().unwrap();
                            if let Some(text) =
                                extract_results_row(mouse.column, mouse.row, &last_draw_areas, &s)
                            {
                                drop(s);
                                let ok = clipboard.set_text(&text);
                                toasts.push((
                                    if ok {
                                        "Row copied to clipboard".to_string()
                                    } else {
                                        "Row: clipboard copy failed (too large)".to_string()
                                    },
                                    if ok {
                                        ToastKind::Info
                                    } else {
                                        ToastKind::Error
                                    },
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        let mut s = state.lock().unwrap();
                        if s.show_help {
                            let max = last_draw_areas.help_max_scroll;
                            s.help_scroll = s
                                .help_scroll
                                .saturating_add(mouse_scroll_lines as u16)
                                .min(max);
                        } else {
                            s.focus = pane;
                            match pane {
                                Focus::Schema => {
                                    schema_search.focused = false;
                                    // Wheel always scrolls the viewport — works
                                    // even with an active filter. The cursor
                                    // stays where the user put it.
                                    s.scroll_schema_viewport(mouse_scroll_lines as i32);
                                }
                                Focus::Results => {
                                    for _ in 0..mouse_scroll_lines {
                                        s.scroll_results_down();
                                    }
                                }
                                Focus::Editor => {
                                    drop(s);
                                    editor.scroll_down(mouse_scroll_lines as i16);
                                }
                                Focus::Hover => {
                                    for _ in 0..mouse_scroll_lines {
                                        s.hover_cursor_move(1, 0);
                                    }
                                }
                            }
                        }
                    }
                    MouseEventKind::ScrollUp => {
                        let mut s = state.lock().unwrap();
                        if s.show_help {
                            s.help_scroll = s.help_scroll.saturating_sub(mouse_scroll_lines as u16);
                        } else {
                            s.focus = pane;
                            match pane {
                                Focus::Schema => {
                                    schema_search.focused = false;
                                    s.scroll_schema_viewport(-(mouse_scroll_lines as i32));
                                }
                                Focus::Results => {
                                    for _ in 0..mouse_scroll_lines {
                                        s.scroll_results_up();
                                    }
                                }
                                Focus::Editor => {
                                    drop(s);
                                    editor.scroll_up(mouse_scroll_lines as i16);
                                }
                                Focus::Hover => {
                                    for _ in 0..mouse_scroll_lines {
                                        s.hover_cursor_move(-1, 0);
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::Paste(text) => {
                // Bracketed-paste arrives as one event rather than N
                // key events. Insert as one atomic chunk:
                // - editor in Insert mode → splice into the buffer.
                // - any active prompt → paste into that prompt.
                // Other modes ignore the paste (vim-ish — use `p`/`P`
                // for the yank-register flow).
                let focus = state.lock().unwrap().focus;
                if focus == Focus::Editor && editor.vim_mode() == VimMode::Insert {
                    let parts: Vec<&str> = text.split('\n').collect();
                    for (i, line) in parts.iter().enumerate() {
                        editor.textarea.insert_str(*line);
                        if i + 1 < parts.len() {
                            editor.textarea.insert_newline();
                        }
                    }
                    editor.mark_content_dirty();
                } else if let Some(ref mut cmd) = command_input {
                    cmd.insert_str(&text);
                } else if let Some(ref mut rp) = rename_input {
                    rp.insert_str(&text);
                }
            }
            Event::Key(key) => {
                // Hover popups (both text and table forms) capture
                // focus and live until Esc; no auto-dismiss here.
                // Ctrl-C while a query / batch is running cancels the
                // current query and skips any remaining ones in the
                // batch. Falls through to the regular handler
                // otherwise so Ctrl-C keeps its "dismiss results"
                // binding in idle state.
                if key.modifiers == KeyModifiers::CONTROL && matches!(key.code, KeyCode::Char('c'))
                {
                    let in_flight = state.lock().unwrap().query_in_flight();
                    if in_flight {
                        state.lock().unwrap().cancel_current_query();
                        toasts.push((
                            "Query cancelled".into(),
                            ToastKind::Info,
                            std::time::Instant::now(),
                        ));
                        continue;
                    }
                }
                // Double-Esc within 500ms dismisses any visible toasts. Tracked
                // globally so it works regardless of which mode the first Esc
                // may have exited.
                if key.code == KeyCode::Esc {
                    let now = std::time::Instant::now();
                    if let Some(prev) = last_esc_at
                        && now.duration_since(prev) <= Duration::from_millis(500)
                        && !toasts.is_empty()
                    {
                        toasts.clear();
                    }
                    last_esc_at = Some(now);
                }
                let (
                    focus,
                    vim_mode,
                    show_completions,
                    show_switcher,
                    show_add,
                    show_help,
                    show_results,
                ) = {
                    let s = state.lock().unwrap();
                    (
                        s.focus,
                        s.vim_mode,
                        s.show_completions,
                        s.show_connection_switcher,
                        s.show_add_connection,
                        s.show_help,
                        s.has_results(),
                    )
                };

                // ── Leader-key chord ─────────────────────────────────────────────
                // Eligible context: no modal open, schema search box not focused,
                // and either we're outside the editor or in Vim Normal mode.
                let leader_eligible = command_input.is_none()
                    && rename_input.is_none()
                    && file_picker.is_none()
                    && delete_confirm.is_none()
                    && editor.search_prompt().is_none()
                    && !show_switcher
                    && !show_add
                    && !show_help
                    && !show_completions
                    && !schema_search.focused
                    && (focus != Focus::Editor || vim_mode == VimMode::Normal);

                // Resolve a pending leader chord with the current key.
                if let Some(t) = leader_pending_at {
                    let expired = t.elapsed() > Duration::from_millis(1500);
                    leader_pending_at = None;
                    if !expired && key.modifiers == KeyModifiers::NONE {
                        match key.code {
                            KeyCode::Char('c') => {
                                state.lock().unwrap().open_connection_switcher();
                                continue;
                            }
                            KeyCode::Char('n') => {
                                let content = {
                                    let mut s = state.lock().unwrap();
                                    s.new_tab();
                                    s.tab_content_pending.take()
                                };
                                if let Some(c) = content {
                                    editor.set_content(&c);
                                    let _ = editor.take_dirty();
                                    editor_dirty = false;
                                    last_highlight_top = usize::MAX;
                                }
                                continue;
                            }
                            KeyCode::Char('r') => {
                                let s = state.lock().unwrap();
                                let current = s
                                    .tabs
                                    .get(s.active_tab)
                                    .map(|t| t.name.clone())
                                    .unwrap_or_default();
                                drop(s);
                                rename_input = Some(TextInput::from_str(&current));
                                continue;
                            }
                            KeyCode::Char(c) if c == leader_char => {
                                file_picker = Some(FilePicker::default());
                                continue;
                            }
                            KeyCode::Char('d') => {
                                let s = state.lock().unwrap();
                                if let Some(name) = s.tabs.get(s.active_tab).map(|t| t.name.clone())
                                {
                                    drop(s);
                                    delete_confirm = Some(name);
                                }
                                continue;
                            }
                            KeyCode::Esc => continue,
                            _ => {}
                        }
                    }
                    // Unknown chord or expired — silently drop the second key so
                    // the leader doesn't accidentally insert text.
                    continue;
                }

                // Arm leader on press.
                if leader_eligible
                    && key.modifiers == KeyModifiers::NONE
                    && matches!(key.code, KeyCode::Char(c) if c == leader_char)
                {
                    leader_pending_at = Some(std::time::Instant::now());
                    continue;
                }

                // ── Quit confirmation (unsaved buffers) ──────────────────────────
                if quit_prompt.is_some() {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Char('y')) => {
                            quit_prompt = None;
                            let pending = {
                                let mut s = state.lock().unwrap();
                                s.editor_content = editor.content_arc();
                                s.editor_content_synced = true;
                                s.mark_active_dirty();
                                s.prepare_save_all_dirty()
                            };
                            let failed = commit_pending_saves(&state, pending).await;
                            if failed.is_empty() {
                                break;
                            }
                            toasts.push((
                                format!("Save failed for: {}", failed.join(", ")),
                                ToastKind::Error,
                                std::time::Instant::now(),
                            ));
                        }
                        (KeyModifiers::NONE, KeyCode::Char('n')) => {
                            break;
                        }
                        _ => {
                            // Any other key (Esc, c, …) cancels.
                            quit_prompt = None;
                        }
                    }
                    continue;
                }

                // ── Command input mode ───────────────────────────────────────────
                if let Some(ref mut cmd) = command_input {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            command_input = None;
                        }
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            let cmd_str = command_input.take().unwrap_or_default().text;
                            let trimmed = cmd_str.trim();
                            match editor::ex::run(&mut editor, trimmed) {
                                editor::ex::ExEffect::Quit { force, save } => {
                                    let local_dirty = editor_dirty;
                                    let any_dirty = {
                                        let mut s = state.lock().unwrap();
                                        s.editor_content = editor.content_arc();
                                        s.editor_content_synced = true;
                                        editor_dirty = false;
                                        if local_dirty {
                                            s.mark_active_dirty();
                                        }
                                        local_dirty || s.any_dirty()
                                    };
                                    if force {
                                        break;
                                    }
                                    if save {
                                        let pending =
                                            state.lock().unwrap().prepare_save_all_dirty();
                                        let failed = commit_pending_saves(&state, pending).await;
                                        if failed.is_empty() {
                                            break;
                                        }
                                        toasts.push((
                                            format!("Save failed for: {}", failed.join(", ")),
                                            ToastKind::Error,
                                            std::time::Instant::now(),
                                        ));
                                    } else if any_dirty {
                                        quit_prompt = Some(());
                                    } else {
                                        break;
                                    }
                                }
                                editor::ex::ExEffect::Save => {
                                    let prepared = {
                                        let mut s = state.lock().unwrap();
                                        // The heavy content pipeline is
                                        // gated off for buffers over
                                        // 2 MB, which otherwise leaves
                                        // `editor_content_synced = false`
                                        // and the save falls back to
                                        // stale `tab.content`.
                                        s.editor_content = editor.content_arc();
                                        s.editor_content_synced = true;
                                        s.prepare_save_active_tab()
                                    };
                                    match prepared {
                                        Ok(pending) => {
                                            // Run the disk write on a
                                            // blocking task so multi-MB
                                            // saves don't freeze the
                                            // render loop.
                                            let name = pending.name.clone();
                                            let idx = pending.tab_index;
                                            let commit = tokio::task::spawn_blocking(move || {
                                                pending.commit()
                                            })
                                            .await
                                            .unwrap_or_else(|e| {
                                                Err(std::io::Error::other(format!(
                                                    "spawn_blocking join error: {e}"
                                                )))
                                            });
                                            match commit {
                                                Ok(()) => {
                                                    if let Some(i) = idx {
                                                        state.lock().unwrap().mark_tab_saved(i);
                                                    }
                                                    editor_dirty = false;
                                                    toasts.push((
                                                        format!("Saved {name}"),
                                                        ToastKind::Info,
                                                        std::time::Instant::now(),
                                                    ));
                                                }
                                                Err(e) => toasts.push((
                                                    format!("Save failed: {e}"),
                                                    ToastKind::Error,
                                                    std::time::Instant::now(),
                                                )),
                                            }
                                        }
                                        Err(e) => toasts.push((
                                            format!("Save failed: {e}"),
                                            ToastKind::Error,
                                            std::time::Instant::now(),
                                        )),
                                    }
                                }
                                editor::ex::ExEffect::Substituted { count } => {
                                    state.lock().unwrap().focus = Focus::Editor;
                                    editor_dirty = true;
                                    toasts.push((
                                        format!("{count} substitution(s)"),
                                        ToastKind::Info,
                                        std::time::Instant::now(),
                                    ));
                                }
                                editor::ex::ExEffect::Ok => {
                                    state.lock().unwrap().focus = Focus::Editor;
                                }
                                editor::ex::ExEffect::Info(msg) => {
                                    toasts.push((msg, ToastKind::Info, std::time::Instant::now()));
                                }
                                editor::ex::ExEffect::Error(msg) => {
                                    toasts.push((msg, ToastKind::Error, std::time::Instant::now()));
                                }
                                editor::ex::ExEffect::Unknown(c) => {
                                    toasts.push((
                                        format!("Unknown command: :{c}"),
                                        ToastKind::Error,
                                        std::time::Instant::now(),
                                    ));
                                }
                                editor::ex::ExEffect::None => {}
                            }
                        }
                        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                            cmd.insert_char(c);
                        }
                        (KeyModifiers::NONE, code) if cmd.handle_nav(code) => {}
                        _ => {}
                    }
                    continue;
                }

                // ── Rename input mode ────────────────────────────────────────────
                if let Some(ref mut name) = rename_input {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            rename_input = None;
                        }
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            let name_str = rename_input.take().unwrap_or_default().text;
                            let mut s = state.lock().unwrap();
                            if let Err(e) = s.rename_active_tab(&name_str) {
                                toasts.push((
                                    format!("Rename failed: {e}"),
                                    ToastKind::Error,
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                            name.insert_char(c);
                        }
                        (KeyModifiers::NONE, code) if name.handle_nav(code) => {}
                        _ => {}
                    }
                    continue;
                }

                // ── Delete confirmation (leader+d) ───────────────────────────────
                if delete_confirm.is_some() {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Char('y'))
                        | (KeyModifiers::NONE, KeyCode::Enter) => {
                            delete_confirm = None;
                            let mut s = state.lock().unwrap();
                            if let Err(e) = s.delete_active_tab() {
                                toasts.push((
                                    format!("Delete failed: {e}"),
                                    ToastKind::Error,
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                        _ => {
                            // Any other key cancels (Esc, n, etc.).
                            delete_confirm = None;
                        }
                    }
                    continue;
                }

                // ── File picker (leader+space) ───────────────────────────────────
                if let Some(ref mut picker) = file_picker {
                    let names: Vec<String> = state
                        .lock()
                        .unwrap()
                        .tabs
                        .iter()
                        .map(|t| t.name.clone())
                        .collect();
                    let matched: Vec<String> =
                        picker.matches(&names).into_iter().cloned().collect();
                    let max = matched.len().saturating_sub(1);
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            file_picker = None;
                        }
                        (KeyModifiers::NONE, KeyCode::Down)
                        | (KeyModifiers::CONTROL, KeyCode::Char('j' | 'n')) => {
                            picker.cursor = (picker.cursor + 1).min(max);
                        }
                        (KeyModifiers::NONE, KeyCode::Up)
                        | (KeyModifiers::CONTROL, KeyCode::Char('k' | 'p')) => {
                            picker.cursor = picker.cursor.saturating_sub(1);
                        }
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            if let Some(name) = matched.get(picker.cursor) {
                                let mut s = state.lock().unwrap();
                                if let Some(idx) = s.tabs.iter().position(|t| &t.name == name) {
                                    if editor_dirty {
                                        s.editor_content = editor.content_arc();
                                        s.mark_active_dirty();
                                        editor_dirty = false;
                                    }
                                    s.switch_to_tab(idx);
                                }
                            }
                            file_picker = None;
                        }
                        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                            picker.query.insert_char(c);
                            picker.cursor = 0;
                        }
                        (mods, code)
                            if mods == KeyModifiers::NONE && picker.query.handle_nav(code) =>
                        {
                            picker.cursor = 0;
                        }
                        _ => {}
                    }
                    continue;
                }

                // ── Results-pane `/` search prompt ───────────────────────────────
                if let Some(ref mut prompt) = results_search_prompt {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            results_search_prompt = None;
                        }
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            let text = results_search_prompt.take().unwrap_or_default().text;
                            if !text.is_empty() {
                                let mut s = state.lock().unwrap();
                                let found = s.results_find(&text, true, false);
                                results_search_pattern = Some(text);
                                drop(s);
                                if !found {
                                    toasts.push((
                                        "Pattern not found".into(),
                                        ToastKind::Info,
                                        std::time::Instant::now(),
                                    ));
                                }
                            }
                        }
                        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                            prompt.insert_char(c);
                        }
                        (KeyModifiers::NONE, code) if prompt.handle_nav(code) => {}
                        _ => {}
                    }
                    continue;
                }

                // ── Schema search box (typing mode) ─────────────────────────────
                if schema_search.focused {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => schema_search.clear(),
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            // Keep filter active, switch to list navigation mode.
                            schema_search.focused = false;
                        }
                        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                            schema_search.push(c);
                            if let Some(q) = schema_search.query() {
                                state.lock().unwrap().lazy_load_for_schema_search(q);
                            }
                        }
                        (KeyModifiers::NONE, code) if schema_search.handle_nav(code) => {
                            if let Some(q) = schema_search.query() {
                                state.lock().unwrap().lazy_load_for_schema_search(q);
                            }
                        }
                        // ctrl+hjkl: dismiss search and move focus.
                        (KeyModifiers::CONTROL, KeyCode::Char('h')) => {
                            schema_search.clear();
                            tmux_navigate('L');
                        }
                        (KeyModifiers::CONTROL, KeyCode::Char('l' | 'k')) => {
                            schema_search.clear();
                            state.lock().unwrap().focus = Focus::Editor;
                        }
                        (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
                            schema_search.clear();
                            if show_results {
                                state.lock().unwrap().focus = Focus::Results;
                            } else {
                                tmux_navigate('D');
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // ── Schema filter navigation (filter active, box unfocused) ───────
                if schema_search.is_filtering() && focus == Focus::Schema {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => schema_search.clear(),
                        (KeyModifiers::NONE, KeyCode::Char('j') | KeyCode::Down) => {
                            schema_search.cursor_down(last_draw_areas.schema_list_count);
                        }
                        (KeyModifiers::NONE, KeyCode::Char('k') | KeyCode::Up) => {
                            schema_search.cursor_up();
                        }
                        (KeyModifiers::NONE, KeyCode::Char('/')) => {
                            schema_search.focused = true;
                        }
                        _ => {}
                    }
                    continue;
                }

                // The `/` / `?` search prompt is owned by the editor now;
                // just forward the key and let sqeel-vim handle it.
                if editor.search_prompt().is_some() {
                    editor.handle_key(key);
                    continue;
                }

                // ── Help overlay ─────────────────────────────────────────────────
                if show_help {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            state.lock().unwrap().close_help();
                        }
                        (KeyModifiers::NONE, KeyCode::Char('j') | KeyCode::Down) => {
                            let max = last_draw_areas.help_max_scroll;
                            let mut s = state.lock().unwrap();
                            s.help_scroll = s.help_scroll.saturating_add(1).min(max);
                        }
                        (KeyModifiers::NONE, KeyCode::Char('k') | KeyCode::Up) => {
                            let mut s = state.lock().unwrap();
                            s.help_scroll = s.help_scroll.saturating_sub(1);
                        }
                        _ => {}
                    }
                    continue;
                }

                // ── Add connection modal (highest priority) ──────────────────────
                if show_add {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            state.lock().unwrap().close_add_connection();
                        }
                        (KeyModifiers::NONE, KeyCode::Tab) => {
                            state.lock().unwrap().add_connection_tab();
                        }
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            let result = state.lock().unwrap().save_new_connection();
                            if let Err(e) = result {
                                state.lock().unwrap().set_error(format!("Save failed: {e}"));
                            }
                        }
                        (KeyModifiers::NONE, KeyCode::Backspace) => {
                            state.lock().unwrap().add_connection_backspace();
                        }
                        (KeyModifiers::NONE, KeyCode::Delete) => {
                            state.lock().unwrap().add_connection_delete();
                        }
                        (KeyModifiers::NONE, KeyCode::Left) => {
                            state.lock().unwrap().add_connection_left();
                        }
                        (KeyModifiers::NONE, KeyCode::Right) => {
                            state.lock().unwrap().add_connection_right();
                        }
                        (KeyModifiers::NONE, KeyCode::Home) => {
                            state.lock().unwrap().add_connection_home();
                        }
                        (KeyModifiers::NONE, KeyCode::End) => {
                            state.lock().unwrap().add_connection_end();
                        }
                        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(ch)) => {
                            state.lock().unwrap().add_connection_type_char(ch);
                        }
                        _ => {}
                    }
                    continue;
                }

                // ── Connection switcher modal ────────────────────────────────────
                if show_switcher {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            state.lock().unwrap().close_connection_switcher();
                        }
                        (KeyModifiers::NONE, KeyCode::Char('j')) => {
                            state.lock().unwrap().switcher_down();
                        }
                        (KeyModifiers::NONE, KeyCode::Char('k')) => {
                            state.lock().unwrap().switcher_up();
                        }
                        (KeyModifiers::NONE, KeyCode::Char('n')) => {
                            state.lock().unwrap().open_add_connection();
                        }
                        (KeyModifiers::NONE, KeyCode::Char('e')) => {
                            state.lock().unwrap().open_edit_connection();
                        }
                        (KeyModifiers::NONE, KeyCode::Char('d')) => {
                            let result = state.lock().unwrap().delete_selected_connection();
                            if let Err(e) = result {
                                state
                                    .lock()
                                    .unwrap()
                                    .set_error(format!("Delete failed: {e}"));
                            }
                        }
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            state.lock().unwrap().confirm_connection_switch();
                        }
                        _ => {}
                    }
                    continue;
                }

                // ── Normal key handling ──────────────────────────────────────────

                // Completion popup navigation
                if show_completions {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            state.lock().unwrap().dismiss_completions();
                            if keybinding_mode == KeybindingMode::Vim {
                                // Route through the editor so the regular
                                // insert-Esc handling (back-one + sticky col
                                // sync) runs. force_normal() bypasses both.
                                editor.handle_key(key);
                            }
                            continue;
                        }
                        (KeyModifiers::NONE, KeyCode::Up)
                        | (KeyModifiers::SHIFT, KeyCode::BackTab) => {
                            state.lock().unwrap().completion_cursor_up();
                            continue;
                        }
                        (KeyModifiers::NONE, KeyCode::Down)
                        | (KeyModifiers::NONE, KeyCode::Tab) => {
                            state.lock().unwrap().completion_cursor_down();
                            continue;
                        }
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            let chosen = state
                                .lock()
                                .unwrap()
                                .selected_completion()
                                .map(|s| s.to_owned());
                            if let Some(text) = chosen {
                                editor.accept_completion(&text);
                                state.lock().unwrap().dismiss_completions();
                                // Consume dirty flag so completions don't re-trigger immediately.
                                editor.take_dirty();
                            }
                            continue;
                        }
                        _ => {}
                    }
                }

                // Any key other than a second `g` aborts the pending `gg` chord.
                let keep_schema_g_pending = focus == Focus::Schema
                    && key.modifiers == KeyModifiers::NONE
                    && matches!(key.code, KeyCode::Char('g'));
                match (key.modifiers, key.code) {
                    // Shift+H / Shift+L: prev / next tab. Active outside the
                    // editor or when in Vim Normal mode so it doesn't shadow
                    // typing in Insert mode.
                    (KeyModifiers::SHIFT, KeyCode::Char('L')) if focus == Focus::Results => {
                        state.lock().unwrap().next_result_tab();
                    }
                    (KeyModifiers::SHIFT, KeyCode::Char('H')) if focus == Focus::Results => {
                        state.lock().unwrap().prev_result_tab();
                    }
                    (KeyModifiers::SHIFT, KeyCode::Char('L'))
                        if focus != Focus::Editor || vim_mode == VimMode::Normal =>
                    {
                        let content = {
                            let mut s = state.lock().unwrap();
                            if editor_dirty {
                                s.editor_content = editor.content_arc();
                                s.mark_active_dirty();
                                editor_dirty = false;
                            }
                            s.next_tab();
                            s.tab_content_pending.take()
                        };
                        if let Some(c) = content {
                            editor.set_content(&c);
                            let _ = editor.take_dirty();
                            editor_dirty = false;
                            last_highlight_top = usize::MAX;
                        }
                    }
                    (KeyModifiers::SHIFT, KeyCode::Char('H'))
                        if focus != Focus::Editor || vim_mode == VimMode::Normal =>
                    {
                        let content = {
                            let mut s = state.lock().unwrap();
                            if editor_dirty {
                                s.editor_content = editor.content_arc();
                                s.mark_active_dirty();
                                editor_dirty = false;
                            }
                            s.prev_tab();
                            s.tab_content_pending.take()
                        };
                        if let Some(c) = content {
                            editor.set_content(&c);
                            let _ = editor.take_dirty();
                            editor_dirty = false;
                            last_highlight_top = usize::MAX;
                        }
                    }
                    // Command mode
                    (KeyModifiers::NONE, KeyCode::Char(':'))
                        if focus != Focus::Editor || vim_mode == VimMode::Normal =>
                    {
                        command_input = Some(TextInput::default());
                    }
                    // Help: ?
                    (KeyModifiers::NONE, KeyCode::Char('?'))
                        if focus != Focus::Editor || vim_mode == VimMode::Normal =>
                    {
                        state.lock().unwrap().open_help();
                    }
                    // Schema pane navigation
                    (KeyModifiers::NONE, KeyCode::Char('j')) if focus == Focus::Schema => {
                        state.lock().unwrap().schema_cursor_down();
                    }
                    (KeyModifiers::NONE, KeyCode::Char('k')) if focus == Focus::Schema => {
                        state.lock().unwrap().schema_cursor_up();
                    }
                    (KeyModifiers::NONE, KeyCode::Char('g')) if focus == Focus::Schema => {
                        // `gg` → top. First `g` arms the chord; second `g`
                        // (landing here with pending already set) fires it.
                        if schema_g_pending {
                            state.lock().unwrap().schema_cursor_top();
                        } else {
                            schema_g_pending = true;
                        }
                    }
                    (KeyModifiers::SHIFT, KeyCode::Char('G'))
                    | (KeyModifiers::NONE, KeyCode::Char('G'))
                        if focus == Focus::Schema =>
                    {
                        state.lock().unwrap().schema_cursor_bottom();
                    }
                    (KeyModifiers::NONE, KeyCode::Enter | KeyCode::Char('l'))
                        if focus == Focus::Schema =>
                    {
                        state.lock().unwrap().schema_toggle_current();
                    }
                    // Schema search
                    (KeyModifiers::NONE, KeyCode::Char('/')) if focus == Focus::Schema => {
                        schema_search.start();
                    }
                    // Results pane: digit count prefix. `0` only counts
                    // as a digit when a count is already in progress —
                    // otherwise it's the `0` row-start binding below.
                    (KeyModifiers::NONE, KeyCode::Char(c @ '0'..='9'))
                        if focus == Focus::Results
                            && (c != '0' || results_count > 0)
                            && state.lock().unwrap().active_ddl_text().is_none() =>
                    {
                        results_count = results_count
                            .saturating_mul(10)
                            .saturating_add((c as u8 - b'0') as usize);
                    }
                    // ── Hover popup (Focus::Hover) — grid nav + yank ─────
                    // Esc first cancels an active visual selection so
                    // the second press closes the popup, mirroring the
                    // results-pane idiom. Also drops the pending hover
                    // request id so a late response doesn't re-open
                    // the popup after the user dismissed it.
                    (KeyModifiers::NONE, KeyCode::Esc) if focus == Focus::Hover => {
                        let mut s = state.lock().unwrap();
                        if s.hover_selection.is_some() {
                            s.hover_selection = None;
                        } else {
                            s.close_hover();
                            last_hover_id = None;
                        }
                    }
                    // `V` / `v` / `Ctrl-V` — visual-line / block
                    // selection inside the hover grid. Toggles off on
                    // the same key, exactly like the results pane.
                    (KeyModifiers::SHIFT, KeyCode::Char('V'))
                    | (KeyModifiers::NONE, KeyCode::Char('V'))
                        if focus == Focus::Hover =>
                    {
                        use sqeel_core::state::{ResultsSelection, ResultsSelectionMode};
                        let mut s = state.lock().unwrap();
                        let already = matches!(
                            s.hover_selection,
                            Some(ResultsSelection {
                                mode: ResultsSelectionMode::Line,
                                ..
                            })
                        );
                        if already {
                            s.hover_selection = None;
                        } else if let ResultsCursor::Cell { row, col } = s.hover_cursor {
                            s.hover_selection = Some(ResultsSelection {
                                anchor: (row, col),
                                mode: ResultsSelectionMode::Line,
                            });
                        }
                    }
                    (KeyModifiers::CONTROL, KeyCode::Char('v'))
                    | (KeyModifiers::NONE, KeyCode::Char('v'))
                        if focus == Focus::Hover =>
                    {
                        use sqeel_core::state::{ResultsSelection, ResultsSelectionMode};
                        let mut s = state.lock().unwrap();
                        let already = matches!(
                            s.hover_selection,
                            Some(ResultsSelection {
                                mode: ResultsSelectionMode::Block,
                                ..
                            })
                        );
                        if already {
                            s.hover_selection = None;
                        } else if let ResultsCursor::Cell { row, col } = s.hover_cursor {
                            s.hover_selection = Some(ResultsSelection {
                                anchor: (row, col),
                                mode: ResultsSelectionMode::Block,
                            });
                        }
                    }
                    (KeyModifiers::NONE, KeyCode::Char('j') | KeyCode::Down)
                        if focus == Focus::Hover =>
                    {
                        let mut s = state.lock().unwrap();
                        if s.hover_table.is_some() {
                            s.hover_cursor_move(1, 0);
                        } else {
                            s.hover_scroll = s.hover_scroll.saturating_add(1);
                        }
                    }
                    (KeyModifiers::NONE, KeyCode::Char('k') | KeyCode::Up)
                        if focus == Focus::Hover =>
                    {
                        let mut s = state.lock().unwrap();
                        if s.hover_table.is_some() {
                            s.hover_cursor_move(-1, 0);
                        } else {
                            s.hover_scroll = s.hover_scroll.saturating_sub(1);
                        }
                    }
                    (KeyModifiers::NONE, KeyCode::Char('l') | KeyCode::Right)
                        if focus == Focus::Hover =>
                    {
                        state.lock().unwrap().hover_cursor_move(0, 1);
                    }
                    (KeyModifiers::NONE, KeyCode::Char('h') | KeyCode::Left)
                        if focus == Focus::Hover =>
                    {
                        state.lock().unwrap().hover_cursor_move(0, -1);
                    }
                    (KeyModifiers::SHIFT, KeyCode::Char('G'))
                    | (KeyModifiers::NONE, KeyCode::Char('G'))
                        if focus == Focus::Hover =>
                    {
                        state
                            .lock()
                            .unwrap()
                            .hover_cursor_edge(sqeel_core::state::HoverEdge::LastRow);
                    }
                    (KeyModifiers::NONE, KeyCode::Char('g')) if focus == Focus::Hover => {
                        // `gg` — repurpose the results-pane chord tracker
                        // since only one pane is ever focused at a time.
                        if results_g_pending {
                            state
                                .lock()
                                .unwrap()
                                .hover_cursor_edge(sqeel_core::state::HoverEdge::FirstRow);
                            results_g_pending = false;
                        } else {
                            results_g_pending = true;
                        }
                    }
                    (KeyModifiers::NONE, KeyCode::Char('0')) if focus == Focus::Hover => {
                        state
                            .lock()
                            .unwrap()
                            .hover_cursor_edge(sqeel_core::state::HoverEdge::RowStart);
                    }
                    (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('$'))
                        if focus == Focus::Hover =>
                    {
                        state
                            .lock()
                            .unwrap()
                            .hover_cursor_edge(sqeel_core::state::HoverEdge::RowEnd);
                    }
                    (KeyModifiers::NONE, KeyCode::Char('y')) if focus == Focus::Hover => {
                        let yanked = state.lock().unwrap().hover_yank();
                        if let Some((text, label)) = yanked {
                            let ok = clipboard.set_text(&text);
                            toasts.push((
                                if ok {
                                    format!("{label} copied to clipboard")
                                } else {
                                    format!("{label}: clipboard copy failed (too large)")
                                },
                                if ok {
                                    ToastKind::Info
                                } else {
                                    ToastKind::Error
                                },
                                std::time::Instant::now(),
                            ));
                        }
                    }
                    // Results pane navigation. Arrow keys mirror hjkl.
                    (KeyModifiers::NONE, KeyCode::Char('j') | KeyCode::Down)
                        if focus == Focus::Results =>
                    {
                        let n = results_count.max(1);
                        results_count = 0;
                        let mut s = state.lock().unwrap();
                        if s.active_ddl_text().is_some() {
                            for _ in 0..n {
                                s.scroll_results_down();
                            }
                        } else {
                            for _ in 0..n {
                                s.results_cursor_down();
                            }
                        }
                    }
                    (KeyModifiers::NONE, KeyCode::Char('k') | KeyCode::Up)
                        if focus == Focus::Results =>
                    {
                        let n = results_count.max(1);
                        results_count = 0;
                        let mut s = state.lock().unwrap();
                        if s.active_ddl_text().is_some() {
                            for _ in 0..n {
                                s.scroll_results_up();
                            }
                        } else {
                            for _ in 0..n {
                                s.results_cursor_up();
                            }
                        }
                    }
                    (KeyModifiers::NONE, KeyCode::Char('l') | KeyCode::Right)
                        if focus == Focus::Results =>
                    {
                        let n = results_count.max(1);
                        results_count = 0;
                        let mut s = state.lock().unwrap();
                        if s.active_ddl_text().is_some() {
                            for _ in 0..n {
                                s.scroll_results_right();
                            }
                        } else {
                            for _ in 0..n {
                                s.results_cursor_right();
                            }
                        }
                    }
                    (KeyModifiers::NONE, KeyCode::Char('h') | KeyCode::Left)
                        if focus == Focus::Results =>
                    {
                        let n = results_count.max(1);
                        results_count = 0;
                        let mut s = state.lock().unwrap();
                        if s.active_ddl_text().is_some() {
                            for _ in 0..n {
                                s.scroll_results_left();
                            }
                        } else {
                            for _ in 0..n {
                                s.results_cursor_left();
                            }
                        }
                    }
                    // `gg` chord → first row.
                    (KeyModifiers::NONE, KeyCode::Char('g')) if focus == Focus::Results => {
                        if results_g_pending {
                            state.lock().unwrap().results_cursor_first_row();
                            results_count = 0;
                            results_g_pending = false;
                        } else {
                            results_g_pending = true;
                        }
                    }
                    // `G` → last row.
                    (KeyModifiers::SHIFT, KeyCode::Char('G'))
                    | (KeyModifiers::NONE, KeyCode::Char('G'))
                        if focus == Focus::Results =>
                    {
                        state.lock().unwrap().results_cursor_last_row();
                        results_count = 0;
                    }
                    // `0` → first column of current row. Shadowed by
                    // digit-count arm above when a count is in progress.
                    (KeyModifiers::NONE, KeyCode::Char('0')) if focus == Focus::Results => {
                        state.lock().unwrap().results_cursor_row_start();
                        results_count = 0;
                    }
                    // `$` → last column of current row.
                    (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('$'))
                        if focus == Focus::Results =>
                    {
                        state.lock().unwrap().results_cursor_row_end();
                        results_count = 0;
                    }
                    // `/` → open the results-pane search prompt.
                    (KeyModifiers::NONE, KeyCode::Char('/')) if focus == Focus::Results => {
                        results_search_prompt = Some(TextInput::default());
                    }
                    // `n` / `N` → repeat the last committed results search.
                    (KeyModifiers::NONE, KeyCode::Char('n')) if focus == Focus::Results => {
                        if let Some(pat) = results_search_pattern.clone() {
                            let mut s = state.lock().unwrap();
                            if !s.results_find(&pat, true, true) {
                                drop(s);
                                toasts.push((
                                    "Pattern not found".into(),
                                    ToastKind::Info,
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                    }
                    (KeyModifiers::SHIFT, KeyCode::Char('N'))
                    | (KeyModifiers::NONE, KeyCode::Char('N'))
                        if focus == Focus::Results =>
                    {
                        if let Some(pat) = results_search_pattern.clone() {
                            let mut s = state.lock().unwrap();
                            if !s.results_find(&pat, false, true) {
                                drop(s);
                                toasts.push((
                                    "Pattern not found".into(),
                                    ToastKind::Info,
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                    }
                    // Enter visual-line / visual-block selection in results.
                    (KeyModifiers::SHIFT, KeyCode::Char('V'))
                    | (KeyModifiers::NONE, KeyCode::Char('V'))
                        if focus == Focus::Results =>
                    {
                        let mut s = state.lock().unwrap();
                        let already_line = matches!(
                            s.active_result().and_then(|t| t.selection),
                            Some(sqeel_core::state::ResultsSelection {
                                mode: sqeel_core::state::ResultsSelectionMode::Line,
                                ..
                            })
                        );
                        if already_line {
                            s.results_clear_selection();
                        } else {
                            s.results_enter_selection(
                                sqeel_core::state::ResultsSelectionMode::Line,
                            );
                        }
                    }
                    // Block selection: `Ctrl-V` (vim) or lowercase `v`.
                    // Char-visual doesn't apply to a cell grid, so `v`
                    // is repurposed here — also gives users whose
                    // terminal swallows `Ctrl-V` a working fallback.
                    (KeyModifiers::CONTROL, KeyCode::Char('v'))
                    | (KeyModifiers::NONE, KeyCode::Char('v'))
                        if focus == Focus::Results =>
                    {
                        let mut s = state.lock().unwrap();
                        let already_block = matches!(
                            s.active_result().and_then(|t| t.selection),
                            Some(sqeel_core::state::ResultsSelection {
                                mode: sqeel_core::state::ResultsSelectionMode::Block,
                                ..
                            })
                        );
                        if already_block {
                            s.results_clear_selection();
                        } else {
                            s.results_enter_selection(
                                sqeel_core::state::ResultsSelectionMode::Block,
                            );
                        }
                    }
                    // Esc cancels an active selection before falling through
                    // to the default Esc handling.
                    (KeyModifiers::NONE, KeyCode::Esc)
                        if focus == Focus::Results
                            && state
                                .lock()
                                .unwrap()
                                .active_result()
                                .and_then(|t| t.selection)
                                .is_some() =>
                    {
                        state.lock().unwrap().results_clear_selection();
                    }
                    (KeyModifiers::NONE, KeyCode::Char('y')) if focus == Focus::Results => {
                        let now = std::time::Instant::now();
                        let has_selection = state
                            .lock()
                            .unwrap()
                            .active_result()
                            .and_then(|t| t.selection)
                            .is_some();
                        let is_yy = pending_results_y
                            .is_some_and(|t| now.duration_since(t).as_millis() < 500);
                        let yanked = if has_selection {
                            let mut s = state.lock().unwrap();
                            let y = s.results_selection_yank();
                            s.results_clear_selection();
                            y
                        } else if is_yy {
                            state.lock().unwrap().results_cursor_yank_row()
                        } else {
                            state.lock().unwrap().results_cursor_yank()
                        };
                        pending_results_y = if has_selection || is_yy {
                            None
                        } else {
                            Some(now)
                        };
                        if let Some((text, label)) = yanked {
                            let ok = clipboard.set_text(&text);
                            toasts.push((
                                if ok {
                                    format!("{label} copied to clipboard")
                                } else {
                                    format!("{label}: clipboard copy failed (too large)")
                                },
                                if ok {
                                    ToastKind::Info
                                } else {
                                    ToastKind::Error
                                },
                                now,
                            ));
                        }
                    }
                    // On error tab: Enter jumps editor cursor to the reported line:col
                    (KeyModifiers::NONE, KeyCode::Enter) if focus == Focus::Results => {
                        let jump = {
                            let s = state.lock().unwrap();
                            s.active_result().and_then(|t| match &t.kind {
                                ResultsPane::Error(msg) => parse_error_position(msg),
                                _ => None,
                            })
                        };
                        if let Some((line, col)) = jump {
                            editor.jump_to(line, col);
                            state.lock().unwrap().focus = Focus::Editor;
                        }
                    }
                    // Execute query under cursor: Ctrl+Enter
                    (KeyModifiers::CONTROL, KeyCode::Enter) => {
                        let content = editor.content();
                        let cursor_byte =
                            cursor_byte_offset(editor.textarea.lines(), editor.textarea.cursor());
                        let stmt = statement_at_byte(&content, cursor_byte)
                            .map(|(s, e)| content[s..e].trim().to_string())
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| content.trim().to_string());
                        let mut s = state.lock().unwrap();
                        s.dismiss_completions();
                        let dialect = s.active_dialect;
                        if strip_sql_comments(&stmt).trim().is_empty() {
                            // nothing to run on empty/whitespace-only content
                        } else if !dialect.is_native_statement(&stmt)
                            && let Some(err) = first_syntax_error(&stmt)
                        {
                            s.dismiss_results();
                            s.set_error(format!(
                                "Syntax error at {}:{} — {}",
                                err.line, err.col, err.message
                            ));
                        } else {
                            s.dismiss_results();
                            let tab_idx = s.push_loading_tab(stmt.clone());
                            let sent = s.send_query(stmt.clone(), tab_idx);
                            if !sent {
                                s.push_history(&stmt);
                                s.dismiss_results();
                                s.set_error(
                                    "No DB connected. Use --url / --connection or <leader>c to switch."
                                        .into(),
                                );
                            }
                        }
                    }
                    // Run all statements in the file: Ctrl+Shift+Enter
                    (m, KeyCode::Enter)
                        if m.contains(KeyModifiers::CONTROL) && m.contains(KeyModifiers::SHIFT) =>
                    {
                        let content = editor.content();
                        let stmts: Vec<String> = statement_ranges(&content)
                            .into_iter()
                            .map(|(s, e)| content[s..e].trim().to_string())
                            .filter(|s| !s.is_empty())
                            .filter(|s| !strip_sql_comments(s).trim().is_empty())
                            .collect();
                        let mut s = state.lock().unwrap();
                        s.dismiss_completions();
                        let dialect = s.active_dialect;
                        // Syntax pre-check only if none of the statements
                        // are engine-native (DESC, SHOW, PRAGMA, …) —
                        // tree-sitter-sequel rejects those but the DB runs
                        // them fine.
                        let any_native = stmts.iter().any(|s| dialect.is_native_statement(s));
                        let syntax_err = if any_native {
                            None
                        } else {
                            first_syntax_error(&content)
                        };
                        if stmts.is_empty() {
                            // nothing to run on empty/whitespace-only content
                        } else if let Some(err) = syntax_err {
                            s.dismiss_results();
                            s.set_error(format!(
                                "Syntax error at {}:{} — {}",
                                err.line, err.col, err.message
                            ));
                        } else {
                            s.dismiss_results();
                            for stmt in &stmts {
                                s.push_loading_tab(stmt.clone());
                            }
                            if !s.send_batch(stmts, 0) {
                                s.dismiss_results();
                                s.set_error(
                                    "No DB connected. Use --url / --connection or <leader>c to switch."
                                        .into(),
                                );
                            }
                        }
                    }
                    // History navigation: Ctrl+P (prev) / Ctrl+N (next)
                    (KeyModifiers::CONTROL, KeyCode::Char('p')) if focus == Focus::Editor => {
                        let recalled = state.lock().unwrap().history_prev().map(|s| s.to_owned());
                        if let Some(q) = recalled {
                            editor.set_content(&q);
                            last_highlight_top = usize::MAX;
                        }
                    }
                    (KeyModifiers::CONTROL, KeyCode::Char('n')) if focus == Focus::Editor => {
                        let recalled = state.lock().unwrap().history_next().map(|s| s.to_owned());
                        if let Some(q) = recalled {
                            editor.set_content(&q);
                        } else {
                            editor.set_content("");
                        }
                        last_highlight_top = usize::MAX;
                    }
                    // Pane focus — forward to tmux when already at the edge pane
                    (KeyModifiers::CONTROL, KeyCode::Char('h')) => {
                        if focus == Focus::Schema {
                            tmux_navigate('L');
                        } else {
                            state.lock().unwrap().focus = Focus::Schema;
                        }
                    }
                    (KeyModifiers::CONTROL, KeyCode::Char('l')) => {
                        if focus == Focus::Editor {
                            tmux_navigate('R');
                        } else {
                            state.lock().unwrap().focus = Focus::Editor;
                        }
                    }
                    (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
                        if focus == Focus::Results || !show_results {
                            tmux_navigate('D');
                        } else {
                            state.lock().unwrap().focus = Focus::Results;
                        }
                    }
                    (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
                        if focus == Focus::Editor {
                            tmux_navigate('U');
                        } else {
                            state.lock().unwrap().focus = Focus::Editor;
                        }
                    }
                    // `K` in Normal → LSP hover request. Popup opens
                    // immediately in a loading state so the user can
                    // cancel with `Esc` even before sqls answers
                    // (cold server can take a beat on first request).
                    // Response arrives via `LspEvent::Hover` and swaps
                    // content into place.
                    (KeyModifiers::SHIFT, KeyCode::Char('K'))
                    | (KeyModifiers::NONE, KeyCode::Char('K'))
                        if focus == Focus::Editor && vim_mode == VimMode::Normal =>
                    {
                        if let Some(ref client) = lsp {
                            let (row, col) = editor.textarea.cursor();
                            last_hover_id = Some(client.writer().request_hover(
                                scratch_uri.clone(),
                                row as u32,
                                col as u32,
                            ));
                            state.lock().unwrap().open_hover_loading();
                        }
                    }
                    // `/`, `?`, `n`, `N` — all handled in the vim engine.
                    _ if focus == Focus::Editor => {
                        if vim_mode == VimMode::Normal
                            && (key.modifiers == KeyModifiers::NONE
                                || key.modifiers == KeyModifiers::SHIFT)
                            && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P'))
                            && let Some(text) = clipboard.get_text()
                        {
                            editor.seed_yank(text);
                        }
                        editor.handle_key(key);
                        if let Some(text) = editor.last_yank.take() {
                            let ok = clipboard.set_text(&text);
                            toasts.push((
                                if ok {
                                    "Yanked to clipboard".to_string()
                                } else {
                                    "Yank: clipboard copy failed (too large)".to_string()
                                },
                                if ok {
                                    ToastKind::Info
                                } else {
                                    ToastKind::Error
                                },
                                std::time::Instant::now(),
                            ));
                        }
                    }
                    _ => {}
                }
                if !keep_schema_g_pending {
                    schema_g_pending = false;
                }
                // Clear pending `g` and digit count once any key that
                // wasn't part of the chord/count consumed its turn.
                // Count arms above reset to 0 when they fire; `gg`
                // clears it on the second `g`. Anything else lands here
                // with results_count == 0 already, except where the
                // user hit a non-nav key between digits — flush it.
                // Both Results and Hover use the same chord tracker;
                // include both panes here or a `g` in hover would be
                // wiped before the second keystroke can complete `gg`.
                let keep_results_g = matches!(focus, Focus::Results | Focus::Hover)
                    && key.modifiers == KeyModifiers::NONE
                    && matches!(key.code, KeyCode::Char('g'))
                    && results_g_pending;
                if !keep_results_g {
                    results_g_pending = false;
                }
                let keep_results_count = focus == Focus::Results
                    && key.modifiers == KeyModifiers::NONE
                    && matches!(key.code, KeyCode::Char('0'..='9'));
                if !keep_results_count {
                    results_count = 0;
                }
            } // Event::Key
            Event::Resize(_, _) => {
                terminal.autoresize()?;
            }
            _ => {} // FocusGained, FocusLost, Paste — ignore
        } // match event
    }
    // Graceful LSP shutdown.  `kill_on_drop(true)` is the ultimate
    // backstop for crashes / SIGKILL; this path lets a well-behaved
    // server clean up on clean exits.
    if let Some(mut client) = lsp.take() {
        client.shutdown().await;
    }
    Ok(())
}

fn tmux_navigate(direction: char) {
    if std::env::var("TMUX").is_ok() {
        let _ = std::process::Command::new("tmux")
            .args(["select-pane", &format!("-{direction}")])
            .spawn();
    }
}

fn mode_label(state: &AppState) -> Span<'static> {
    let u = ui();
    match state.vim_mode {
        VimMode::Normal => Span::styled(" NORMAL ", Style::default().fg(u.status_mode_normal)),
        VimMode::Insert => Span::styled(" INSERT ", Style::default().fg(u.status_mode_insert)),
        VimMode::Visual => Span::styled(" VISUAL ", Style::default().fg(u.status_mode_visual)),
        VimMode::VisualLine => Span::styled(" V-LINE ", Style::default().fg(u.status_mode_visual)),
        VimMode::VisualBlock => {
            Span::styled(" V-BLOCK ", Style::default().fg(u.status_mode_visual))
        }
    }
}

fn diag_label(state: &AppState) -> Option<Span<'static>> {
    let errors = state
        .lsp_diagnostics
        .iter()
        .filter(|d| d.severity == lsp_types::DiagnosticSeverity::ERROR)
        .count();
    let warnings = state
        .lsp_diagnostics
        .iter()
        .filter(|d| d.severity == lsp_types::DiagnosticSeverity::WARNING)
        .count();
    if errors > 0 {
        Some(Span::styled(
            format!(" ✖ {errors}E "),
            Style::default().fg(ui().status_diag_error),
        ))
    } else if warnings > 0 {
        Some(Span::styled(
            format!(" ⚠ {warnings}W "),
            Style::default().fg(ui().status_diag_warning),
        ))
    } else {
        None
    }
}

/// Status-bar block showing `/<pat> <i>/<n>` when an editor search is active.
/// `i` is the 1-based index of the match at-or-after the cursor; 0 means no
/// match has been navigated to yet (cursor is past the last match).
fn search_label(editor: &Editor) -> Option<Span<'static>> {
    let re = editor.textarea.search_pattern()?;
    let pat = re.as_str().to_string();
    let lines = editor.textarea.lines();
    let (cur_row, cur_col) = editor.textarea.cursor();
    let mut total = 0usize;
    let mut current = 0usize;
    for (row, line) in lines.iter().enumerate() {
        for m in re.find_iter(line) {
            total += 1;
            if current == 0 {
                let on_or_after_cursor = row > cur_row
                    || (row == cur_row && byte_to_char_col(line, m.start()) >= cur_col);
                if on_or_after_cursor {
                    current = total;
                }
            }
        }
    }
    if total == 0 {
        return Some(Span::raw(format!(" /{pat} 0/0 ")));
    }
    if current == 0 {
        current = total;
    }
    Some(Span::raw(format!(" /{pat} {current}/{total} ")))
}

fn byte_to_char_col(line: &str, byte_idx: usize) -> usize {
    line[..byte_idx.min(line.len())].chars().count()
}

/// Extract the first `L:C` (1-based line:column) location from a message like
/// `"Syntax error at 3:7 — unexpected `foo`"`. Returns `None` if no match.
fn parse_error_position(msg: &str) -> Option<(usize, usize)> {
    let bytes = msg.as_bytes();
    for i in 0..bytes.len() {
        if !bytes[i].is_ascii_digit() {
            continue;
        }
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b':' {
            continue;
        }
        let mut k = j + 1;
        let col_start = k;
        while k < bytes.len() && bytes[k].is_ascii_digit() {
            k += 1;
        }
        if k == col_start {
            continue;
        }
        let line: usize = msg[i..j].parse().ok()?;
        let col: usize = msg[col_start..k].parse().ok()?;
        return Some((line, col));
    }
    None
}

/// Convert a (row, char-col) cursor into a byte offset into `lines.join("\n")`.
fn cursor_byte_offset(lines: &[String], cursor: (usize, usize)) -> usize {
    let mut byte = 0;
    for (i, line) in lines.iter().enumerate() {
        if i < cursor.0 {
            byte += line.len() + 1; // +1 for '\n'
        } else if i == cursor.0 {
            byte += line
                .chars()
                .take(cursor.1)
                .map(|c| c.len_utf8())
                .sum::<usize>();
            break;
        }
    }
    byte
}

/// Desired terminal cursor shape after a draw. The TUI uses a thin vertical bar
/// for any text-input context (insert mode, dialogs, schema search) and a thick
/// block for editor normal mode, so cursors look consistent across the app.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ToastKind {
    Error,
    Info,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum CursorShape {
    #[default]
    Hidden,
    Bar,
    Block,
}

#[derive(Default, Clone, Copy)]
struct DrawAreas {
    schema_list_area: Rect,
    schema_list_offset: usize,
    schema_list_count: usize,
    schema_list_filtered: bool,
    editor: Rect,
    tab_bar: Rect,
    results: Option<Rect>,
    results_tab_bar: Option<Rect>,
    cursor_shape: CursorShape,
    /// Upper bound for `help_scroll`: beyond this the bottom of the
    /// help overlay is already visible. Recomputed each frame from the
    /// current terminal size so `j` / `Down` / wheel-down saturate at
    /// the last meaningful scroll offset.
    help_max_scroll: u16,
}

#[allow(clippy::too_many_arguments)]
fn draw(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    editor: &mut Editor,
    command_input: Option<&TextInput>,
    rename_input: Option<&TextInput>,
    file_picker: Option<&FilePicker>,
    delete_confirm: Option<&str>,
    quit_prompt_dirty: Option<&[String]>,
    schema_search: &SchemaSearch,
    editor_search_text: Option<&str>,
    last_editor_search: Option<&str>,
    results_search_text: Option<&str>,
    toasts: &[(String, ToastKind)],
) -> DrawAreas {
    let area = f.area();

    let lsp_warn = !state.lsp_available;

    // Always reserve 1 row for the status bar; optionally 1 more for LSP warning above it.
    let (main_area, lsp_warn_area, status_area) = if lsp_warn {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);
        (chunks[0], Some(chunks[1]), chunks[2])
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);
        (chunks[0], None, chunks[1])
    };

    let outer_raw = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(vec![
            Constraint::Min(30),
            Constraint::Length(1),
            Constraint::Percentage(85),
        ])
        .split(main_area);
    let outer: Vec<Rect> = {
        let sep = outer_raw[1];
        f.render_widget(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(ui().pane_sep).bg(ui().schema_pane_bg)),
            sep,
        );
        vec![outer_raw[0], outer_raw[2]]
    };

    let schema_focused = state.focus == Focus::Schema;
    let editor_focused = state.focus == Focus::Editor;
    let results_focused = state.focus == Focus::Results;

    // Schema panel
    let (
        schema_list_area,
        schema_list_offset,
        schema_list_count,
        schema_list_filtered,
        schema_search_cursor,
    ) = draw_schema(f, state, outer[0], schema_focused, schema_search);

    let show_results = state.has_results();
    let editor_pct = (state.editor_ratio * 100.0) as u16;
    let results_pct = 100 - editor_pct;

    let right_chunks = if show_results {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(editor_pct),
                Constraint::Percentage(results_pct),
            ])
            .split(outer[1])
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(100)])
            .split(outer[1])
    };

    // Tab bar is the top row of the editor pane, flush with no padding.
    let tab_bar = Rect {
        x: right_chunks[0].x,
        y: right_chunks[0].y,
        width: right_chunks[0].width,
        height: 1,
    };
    let results_tab_bar = if show_results {
        let results_area = right_chunks[1];
        if state.result_tabs.len() > 1 && results_area.height > 2 {
            // Tab bar now sits beneath a 1-row separator at the top of the
            // results pane — shift its y accordingly for click hit-testing.
            Some(Rect {
                x: results_area.x + 1,
                y: results_area.y + 1,
                width: results_area.width.saturating_sub(2),
                height: 1,
            })
        } else {
            None
        }
    } else {
        None
    };
    let mut areas = DrawAreas {
        schema_list_area,
        schema_list_offset,
        schema_list_count,
        schema_list_filtered,
        editor: right_chunks[0],
        tab_bar,
        results: if show_results {
            Some(right_chunks[1])
        } else {
            None
        },
        results_tab_bar,
        cursor_shape: CursorShape::Hidden,
        help_max_scroll: 0,
    };

    draw_editor(
        f,
        state,
        editor,
        right_chunks[0],
        editor_focused,
        editor_search_text,
        last_editor_search,
    );

    if show_results {
        draw_results(
            f,
            state,
            right_chunks[1],
            results_focused,
            results_search_text,
        );
    }

    // Completion popup (overlay).  Use viewport-relative coordinates so
    // the popup stays inside the editor even when the cursor lives deep
    // in a long file.
    if state.show_completions && !state.completions.is_empty() {
        let (cur_row, cur_col) = editor.textarea.cursor();
        let top_row = editor.textarea.viewport_top_row();
        let top_col = editor.textarea.viewport_top_col();
        let screen_row = cur_row.saturating_sub(top_row);
        let screen_col = cur_col.saturating_sub(top_col);
        draw_completions(f, state, right_chunks[0], screen_row, screen_col);
    }

    // Hover popup (LSP `K`). Centered borderless dialog matching the
    // command / rename / delete / file-picker styling — dialog_bg as
    // the only chrome, padded inside, `Clear` under it to punch
    // through whatever the editor drew.
    if let Some(ref table) = state.hover_table {
        draw_hover_table(f, area, state, table);
    } else if let Some(ref text) = state.hover_text {
        draw_hover_popup(f, area, state.hover_scroll, text);
    } else if state.hover_loading {
        draw_hover_loading(f, area);
    }

    // Connection switcher modal (top-level overlay)
    if state.show_connection_switcher {
        draw_connection_switcher(f, state, area);
    }

    // Add connection dialog (above switcher)
    let mut add_connection_cursor: Option<(u16, u16)> = None;
    if state.show_add_connection {
        add_connection_cursor = Some(draw_add_connection(f, state, area));
    }

    // Help overlay (topmost)
    if state.show_help {
        areas.help_max_scroll = draw_help(f, area, state.help_scroll);
    }

    // LSP warning bar (above status bar)
    if let Some(warn_area) = lsp_warn_area {
        let msg = Paragraph::new(Span::styled(
            format!(" ⚠ LSP not available ({})", state.lsp_binary),
            Style::default().fg(ui().lsp_warn_fg).bg(ui().lsp_warn_bg),
        ));
        f.render_widget(msg, warn_area);
    }

    // Status bar (always at bottom)
    draw_status_bar(f, state, editor, status_area);

    // Command palette: small centered dialog, no borders, 2-col + 1-row padding.
    let mut dialog_cursor: Option<(u16, u16)> = None;
    if let Some(cmd) = command_input {
        dialog_cursor = Some(draw_input_dialog(f, area, ": ", cmd));
    }

    // Rename prompt: same shape as command palette.
    if let Some(name) = rename_input {
        dialog_cursor = Some(draw_input_dialog(f, area, "> ", name));
    }

    // Editor `/` / `?` search: same shape as command palette. The
    // editor owns the prompt state; we read it for render via
    // `editor.search_prompt()`.
    if let Some(prompt) = editor.search_prompt() {
        let prefix = if prompt.forward { "/ " } else { "? " };
        let input = TextInput {
            text: prompt.text.clone(),
            cursor: prompt.cursor,
        };
        dialog_cursor = Some(draw_input_dialog(f, area, prefix, &input));
    }

    // Delete confirmation: centered borderless dialog.
    if let Some(name) = delete_confirm {
        draw_confirm_dialog(f, area, &format!("Delete '{name}'?  (y / n)"));
    }

    // Quit confirmation when there are unsaved buffers.
    if let Some(names) = quit_prompt_dirty {
        let list = if names.len() <= 3 {
            names.join(", ")
        } else {
            format!("{} + {} more", names[..3].join(", "), names.len() - 3)
        };
        draw_confirm_dialog(
            f,
            area,
            &format!("Save unsaved buffers [{list}]?  (y=save / n=discard / c=cancel)"),
        );
    }

    // File picker (leader+space): centered dialog with input + scrollable list.
    if let Some(picker) = file_picker {
        let names: Vec<String> = state.tabs.iter().map(|t| t.name.clone()).collect();
        let matched: Vec<&String> = picker.matches(&names);
        let active_name = state.tabs.get(state.active_tab).map(|t| t.name.as_str());
        dialog_cursor = Some(draw_file_picker(f, area, picker, &matched, active_name));
    }

    // Toast notifications (top-right corner, stacked vertically).
    // Each toast is a 3-row block: 1 row top padding, 1 row message, 1 row bottom
    // padding; message is inset by 1 column on the left and right.
    let mut y_off: u16 = 0;
    for (msg, kind) in toasts {
        let style = match kind {
            ToastKind::Error => Style::default()
                .fg(ui().toast_error_fg)
                .bg(ui().toast_error_bg),
            ToastKind::Info => Style::default()
                .fg(ui().toast_info_fg)
                .bg(ui().toast_info_bg),
        };
        let width = (msg.len() as u16 + 4).min(area.width);
        let height = 3u16.min(area.height.saturating_sub(y_off));
        if height == 0 {
            break;
        }
        let toast_area = Rect {
            x: area.width.saturating_sub(width),
            y: y_off,
            width,
            height,
        };
        f.render_widget(Clear, toast_area);
        f.render_widget(Block::default().style(style), toast_area);
        if height >= 2 {
            let msg_area = Rect {
                x: toast_area.x + 2,
                y: toast_area.y + 1,
                width: toast_area.width.saturating_sub(4),
                height: 1,
            };
            f.render_widget(Paragraph::new(msg.as_str()).style(style), msg_area);
        }
        y_off = y_off.saturating_add(height).saturating_add(1);
    }

    // Pick the active cursor target: dialogs > add-connection > schema search >
    // editor (when focused). Bar shape for any text-input context, Block for
    // editor normal mode.
    let (cursor_pos, shape) = if let Some(p) = dialog_cursor {
        (Some(p), CursorShape::Bar)
    } else if let Some(p) = add_connection_cursor {
        (Some(p), CursorShape::Bar)
    } else if let Some(p) = schema_search_cursor {
        (Some(p), CursorShape::Bar)
    } else if state.focus == Focus::Editor && !state.show_help && !state.show_connection_switcher {
        // Reconstruct the textarea rect that draw_editor uses:
        // top row is the tab bar, then a 1-col horizontal margin around the body.
        let pane = right_chunks[0];
        let textarea_rect = Rect {
            x: pane.x.saturating_add(1),
            y: pane.y.saturating_add(1),
            width: pane.width.saturating_sub(2),
            height: pane.height.saturating_sub(1),
        };
        let pos = editor.cursor_screen_pos(textarea_rect);
        let shape = if state.vim_mode == VimMode::Insert {
            CursorShape::Bar
        } else {
            CursorShape::Block
        };
        (pos, shape)
    } else {
        (None, CursorShape::Hidden)
    };
    if let Some((x, y)) = cursor_pos {
        f.set_cursor_position((x, y));
    }
    areas.cursor_shape = shape;

    areas
}

fn extract_results_left_click(
    x: u16,
    y: u16,
    areas: &DrawAreas,
    state: &AppState,
) -> Option<(String, &'static str, ResultsCursor)> {
    let results_area = areas.results?;
    use ratatui::layout::Position;
    if !results_area.contains(Position { x, y }) {
        return None;
    }
    let tab_bar_rows: u16 = if state.result_tabs.len() > 1 { 2 } else { 0 };
    // Shared query-row hit-test: row 3 below the tab bar is the query line
    // for every pane that shows it (Results/Error/Cancelled when a query is
    // attached). Clicking it copies the query verbatim.
    let query_text = state
        .active_result()
        .map(|t| t.query.clone())
        .unwrap_or_default();
    let has_query = !query_text.trim().is_empty();
    let pane_has_query_row = matches!(
        state.results(),
        sqeel_core::state::ResultsPane::Results(_)
            | sqeel_core::state::ResultsPane::Cancelled
            | sqeel_core::state::ResultsPane::Error(_)
    ) && has_query;
    if pane_has_query_row
        && y == results_area.y + tab_bar_rows + 3
        && x >= results_area.x
        && x < results_area.x + results_area.width
    {
        return Some((query_text, "Query", ResultsCursor::Query));
    }
    match state.results() {
        sqeel_core::state::ResultsPane::Results(r) => {
            let header_y = results_area.y + tab_bar_rows + 5;
            let body_y = results_area.y + tab_bar_rows + 7;
            let body_x = results_area.x + 1;
            if y < header_y || y == header_y + 1 {
                return None;
            }
            let char_offset: usize = r
                .col_widths
                .iter()
                .take(state.results_col_scroll())
                .map(|&w| w as usize + 1)
                .sum();
            let rel = (x.saturating_sub(body_x) as usize).saturating_add(char_offset);
            let mut cursor_x = 0usize;
            let mut col_idx: Option<usize> = None;
            for (i, &w) in r.col_widths.iter().enumerate() {
                let col_w = w as usize;
                if rel < cursor_x + col_w {
                    col_idx = Some(i);
                    break;
                }
                cursor_x += col_w;
                if i + 1 < r.col_widths.len() {
                    if rel == cursor_x {
                        return None;
                    }
                    cursor_x += 1;
                }
            }
            let col_idx = col_idx?;
            if y == header_y {
                let name = r.columns.get(col_idx)?.clone();
                return Some((name, "Column", ResultsCursor::Header(col_idx)));
            }
            if y < body_y {
                return None;
            }
            let row_idx = (y - body_y) as usize + state.results_scroll();
            let value = r.rows.get(row_idx)?.get(col_idx)?.trim().to_string();
            Some((
                value,
                "Value",
                ResultsCursor::Cell {
                    row: row_idx,
                    col: col_idx,
                },
            ))
        }
        sqeel_core::state::ResultsPane::Error(e) => {
            let content_y = results_area.y + tab_bar_rows;
            if y < content_y {
                return None;
            }
            let rel_y = (y - content_y) as usize;
            let query = state
                .active_result()
                .map(|t| t.query.clone())
                .unwrap_or_default();
            let (body_start, has_q) = if !query.trim().is_empty() {
                (5usize, true)
            } else {
                (3usize, false)
            };
            if has_q && rel_y == 3 {
                return Some((query.clone(), "Query", ResultsCursor::Query));
            }
            if rel_y >= body_start {
                let line_idx = rel_y - body_start + state.results_scroll();
                let line = e.lines().nth(line_idx)?.to_string();
                return Some((line, "Line", ResultsCursor::MessageLine(line_idx)));
            }
            None
        }
        sqeel_core::state::ResultsPane::Cancelled => {
            let content_y = results_area.y + tab_bar_rows;
            if y < content_y {
                return None;
            }
            let rel_y = (y - content_y) as usize;
            let query = state
                .active_result()
                .map(|t| t.query.clone())
                .unwrap_or_default();
            let has_q = !query.trim().is_empty();
            let body_start = if has_q { 5 } else { 3 };
            if has_q && rel_y == 3 {
                return Some((query, "Query", ResultsCursor::Query));
            }
            if rel_y >= body_start {
                return Some((
                    "Skipped after earlier error".to_string(),
                    "Line",
                    ResultsCursor::MessageLine(0),
                ));
            }
            None
        }
        _ => None,
    }
}

fn extract_results_row(x: u16, y: u16, areas: &DrawAreas, state: &AppState) -> Option<String> {
    let results_area = areas.results?;
    use ratatui::layout::Position;
    if !results_area.contains(Position { x, y }) {
        return None;
    }
    let r = match state.results() {
        sqeel_core::state::ResultsPane::Results(r) => r,
        _ => return None,
    };
    let tab_bar_rows: u16 = if state.result_tabs.len() > 1 { 2 } else { 0 };
    let body_y = results_area.y + tab_bar_rows + 7;
    if y < body_y {
        return None;
    }
    let row_idx = (y - body_y) as usize + state.results_scroll();
    r.rows.get(row_idx).map(|row| row.join("\t"))
}

fn draw_status_bar(f: &mut ratatui::Frame<'_>, state: &AppState, editor: &Editor, area: Rect) {
    let mode = mode_label(state);
    let mode_width = mode.content.len() as u16;

    let conn = state
        .active_connection
        .as_deref()
        .unwrap_or("no connection");
    let tab_name = state
        .tabs
        .get(state.active_tab)
        .map(|t| t.name.as_str())
        .unwrap_or("");
    let center_text = format!(" {conn} › {tab_name} ");

    let (row, col) = editor.textarea.cursor();
    let cursor_str = format!(" {}:{} ", row + 1, col + 1);
    let cursor_width = cursor_str.len() as u16;

    let diag = diag_label(state);
    let diag_width = diag.as_ref().map(|s| s.content.len() as u16).unwrap_or(0);

    let search = search_label(editor);
    let search_width = search.as_ref().map(|s| s.content.len() as u16).unwrap_or(0);

    let right_width = cursor_width + diag_width + search_width;
    let center_width = area.width.saturating_sub(mode_width + right_width);

    // Mode block (left)
    let mode_area = Rect {
        x: area.x,
        y: area.y,
        width: mode_width.min(area.width),
        height: 1,
    };
    // Center info
    let center_area = Rect {
        x: area.x + mode_width,
        y: area.y,
        width: center_width.min(area.width.saturating_sub(mode_width)),
        height: 1,
    };
    // Right side (search + diag + cursor)
    let right_x = area.x + mode_width + center_width;
    let search_area = Rect {
        x: right_x,
        y: area.y,
        width: search_width,
        height: 1,
    };
    let diag_area = Rect {
        x: right_x + search_width,
        y: area.y,
        width: diag_width,
        height: 1,
    };
    let cursor_area = Rect {
        x: right_x + search_width + diag_width,
        y: area.y,
        width: cursor_width.min(
            area.width
                .saturating_sub(mode_width + center_width + search_width + diag_width),
        ),
        height: 1,
    };

    let bar_bg = Style::default()
        .bg(ui().status_bar_bg)
        .fg(ui().status_bar_fg);

    // Mode label (colored fg, same bg as status bar)
    let mode_style = Style::default()
        .bg(mode.style.fg.unwrap_or(ui().status_mode_normal))
        .fg(ui().status_mode_fg)
        .add_modifier(Modifier::BOLD);
    f.render_widget(
        Paragraph::new(Span::styled(mode.content.to_string(), mode_style)),
        mode_area,
    );

    // Center: connection > tab
    f.render_widget(Paragraph::new(center_text).style(bar_bg), center_area);

    // Search match counter
    if let Some(s) = search {
        let style = Style::default()
            .bg(ui().status_search_bg)
            .fg(ui().status_search_fg)
            .add_modifier(Modifier::BOLD);
        f.render_widget(
            Paragraph::new(Span::styled(s.content.to_string(), style)),
            search_area,
        );
    }

    // Diagnostics
    if let Some(d) = diag {
        let diag_style = Style::default()
            .bg(d.style.fg.unwrap_or(ui().status_diag_warning))
            .fg(ui().status_mode_fg)
            .add_modifier(Modifier::BOLD);
        f.render_widget(
            Paragraph::new(Span::styled(d.content.to_string(), diag_style)),
            diag_area,
        );
    }

    // Cursor position (right-aligned, highlighted)
    let cursor_style = Style::default()
        .bg(ui().status_hint_bg)
        .fg(ui().status_hint_fg)
        .add_modifier(Modifier::BOLD);
    f.render_widget(
        Paragraph::new(Span::styled(cursor_str, cursor_style)),
        cursor_area,
    );
}

fn schema_item_line(item: &SchemaTreeItem, u: &theme::UiColors) -> Line<'static> {
    let indent = " ".repeat(1 + item.depth * 2);
    if let SchemaItemKind::Placeholder { loading } = item.kind {
        // Greyed-out hint row; for loading rows also render the shared
        // spinner frame so the user knows something is still in flight.
        let style = Style::default()
            .fg(u.schema_placeholder_fg)
            .add_modifier(Modifier::ITALIC);
        let mut spans = vec![Span::raw(indent)];
        if loading {
            spans.push(Span::styled(format!("{} ", spinner_frame()), style));
        }
        spans.push(Span::styled(item.name.clone(), style));
        return Line::from(spans);
    }
    let (icon, icon_color) = match &item.kind {
        SchemaItemKind::Database => ("󰆼", u.schema_icon_db),
        SchemaItemKind::Table => ("󰓫", u.schema_icon_table),
        SchemaItemKind::Column { is_pk: true, .. } => ("󰌆", u.schema_icon_pk),
        SchemaItemKind::Column { .. } => ("󱘚", u.schema_icon_column),
        SchemaItemKind::Placeholder { .. } => unreachable!("handled above"),
    };
    let mut spans = vec![
        Span::raw(indent),
        Span::styled(icon.to_string(), Style::default().fg(icon_color)),
        Span::raw(format!(" {}", item.name)),
    ];
    if let SchemaItemKind::Column { type_name, .. } = &item.kind
        && !type_name.is_empty()
    {
        spans.push(Span::raw(": "));
        spans.push(Span::styled(
            type_name.clone(),
            Style::default().fg(u.schema_type_fg),
        ));
    }
    Line::from(spans)
}

fn draw_schema(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    area: Rect,
    focused: bool,
    search: &SchemaSearch,
) -> (Rect, usize, usize, bool, Option<(u16, u16)>) {
    let searching = search.focused;
    let search_cursor = search.cursor;
    let title = if state.schema_loading {
        format!("Explorer {}", spinner_frame())
    } else if state.schema_nodes.is_empty() {
        "Explorer".to_string()
    } else {
        let count = state.schema_nodes.len();
        format!("Explorer ✓ ({count})")
    };

    let border_style = if focused {
        Style::default().fg(ui().schema_border_focus)
    } else {
        Style::default()
    };

    // Fill pane background (full area), then inset content by 1 on all sides.
    f.render_widget(
        Block::default().style(Style::default().bg(ui().schema_pane_bg)),
        area,
    );

    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });

    // Search box is always visible (3 rows: border+input+border), list below
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(inner);

    let query = search.query().unwrap_or("");
    let has_filter = !query.is_empty();
    // Magnifier glyph + space prefix marks this as the search input.
    let prefix = "🔍 ";
    let input_text = format!("{prefix}{query}");
    let text_cursor = search.query.as_ref().map(|q| q.cursor).unwrap_or(0);
    // The magnifier emoji is 2 cells wide; total prefix width = 3 cells.
    let prefix_cells: u16 = 3;
    let search_cursor_pos = if searching {
        Some((
            chunks[0].x + 1 + prefix_cells + text_cursor as u16,
            chunks[0].y + 1,
        ))
    } else {
        None
    };
    let search_block = Block::default()
        .title(title.clone())
        .title_style(Style::default().add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(if searching {
            Style::default().fg(ui().schema_border_focus)
        } else if has_filter {
            Style::default().fg(ui().schema_border_filter)
        } else {
            border_style
        });
    f.render_widget(Paragraph::new(input_text).block(search_block), chunks[0]);

    // Inset 1 char left+right to align with search box inner content
    let list_area = Rect {
        x: chunks[1].x + 1,
        y: chunks[1].y,
        width: chunks[1].width.saturating_sub(2),
        height: chunks[1].height,
    };

    let items: Vec<&SchemaTreeItem> = if has_filter {
        schema::filter_items(state.all_schema_items(), query)
    } else {
        state.visible_schema_items().iter().collect()
    };

    let item_count = items.len();

    if items.is_empty() {
        f.render_widget(
            Paragraph::new(if has_filter {
                "No matches"
            } else if state.active_connection.is_some() {
                "Loading..."
            } else {
                "No connection"
            }),
            list_area,
        );
        return (list_area, 0, 0, has_filter, search_cursor_pos);
    }

    let u = ui();
    let list_items: Vec<ListItem> = items
        .iter()
        .map(|item| ListItem::new(schema_item_line(item, u)))
        .collect();

    // When search box is actively focused, don't highlight the list.
    // In filter-nav mode (filter active, box not focused) use search_cursor.
    // Normal mode uses state.schema_cursor.
    let cursor = if has_filter {
        search_cursor
    } else {
        state.schema_cursor
    };
    let (highlight_style, selected) = if searching {
        (Style::default(), None)
    } else if focused {
        (Style::default().bg(ui().schema_sel_active_bg), Some(cursor))
    } else {
        (
            Style::default().bg(ui().schema_sel_inactive_bg),
            Some(cursor),
        )
    };

    // Publish viewport height so cursor-nav helpers on AppState can keep the
    // selection visible without needing the draw metrics plumbed through.
    state
        .schema_viewport_rows
        .store(list_area.height, std::sync::atomic::Ordering::Relaxed);

    let height = list_area.height as usize;
    let max_offset = item_count.saturating_sub(height.max(1));
    let offset = state.schema_scroll_offset.min(max_offset);
    // Only mark the row as "selected" when it's actually inside the viewport;
    // otherwise ratatui's List would fight our offset and snap back to the
    // cursor every frame.
    let selected_visible = selected.and_then(|c| {
        if height > 0 && c >= offset && c < offset + height {
            Some(c)
        } else {
            None
        }
    });

    let list = List::new(list_items).highlight_style(highlight_style);
    let mut list_state = ListState::default()
        .with_offset(offset)
        .with_selected(selected_visible);
    f.render_stateful_widget(list, list_area, &mut list_state);
    (
        list_area,
        list_state.offset(),
        item_count,
        has_filter,
        search_cursor_pos,
    )
}

fn draw_editor(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    editor: &mut Editor,
    area: Rect,
    focused: bool,
    editor_search: Option<&str>,
    last_editor_search: Option<&str>,
) {
    // Fill pane background
    f.render_widget(
        Block::default().style(Style::default().bg(ui().editor_pane_bg)),
        area,
    );

    // Tab bar sits flush at the top (full-width, no padding); the remaining
    // content below is inset by 1 on all sides.
    let tab_bar_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    let body_outer = Rect {
        x: area.x,
        y: area.y.saturating_add(1),
        width: area.width,
        height: area.height.saturating_sub(1),
    };
    let inner = body_outer.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 0,
    });

    // Show first diagnostic message if any
    let diag_line = state
        .lsp_diagnostics
        .first()
        .map(|d| format!(" {}:{} {}", d.line + 1, d.col + 1, d.message));

    // Split inner: textarea + optional diag (1)
    let mut constraints = vec![Constraint::Min(1)];
    if diag_line.is_some() {
        constraints.push(Constraint::Length(1));
    }
    let body_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);
    // Build a `chunks`-like slice: [tab_bar, textarea, ...extras] so the rest of
    // this function (which references chunks[0..]) keeps working unchanged.
    let mut chunks: Vec<Rect> = vec![tab_bar_area];
    chunks.extend(body_chunks.iter().copied());

    f.render_widget(
        Paragraph::new(build_tab_title(state)).style(Style::default().bg(ui().editor_tab_bar_bg)),
        chunks[0],
    );

    // Pre-render a full-width strip at the cursor line so trailing empty cells
    // also get the highlight background (tui-textarea only styles character cells).
    let cursor_line_bg = if focused {
        ui().editor_cursor_line_active
    } else {
        ui().editor_cursor_line_inactive
    };
    let textarea_area = chunks[1];
    let cursor_screen_row = editor.cursor_screen_row(textarea_area.height);
    if cursor_screen_row < textarea_area.height {
        let strip = Rect {
            x: textarea_area.x,
            y: textarea_area.y + cursor_screen_row,
            width: textarea_area.width,
            height: 1,
        };
        f.render_widget(
            Block::default().style(Style::default().bg(cursor_line_bg)),
            strip,
        );
    }

    editor
        .textarea
        .set_line_number_style(Style::default().fg(ui().editor_line_num));
    editor
        .textarea
        .set_cursor_line_style(Style::default().bg(cursor_line_bg));
    // Real terminal cursor handles all cursor rendering — hide the textarea's
    // cell-based cursor by blending it into the cursor-line background.
    editor
        .textarea
        .set_cursor_style(Style::default().bg(cursor_line_bg));
    // Search pattern is dedicated to the user's `/` query (Visual mode clears
    // it so selection color isn't overridden by Search rank).
    if state.vim_mode == VimMode::Visual || state.vim_mode == VimMode::VisualLine {
        let _ = editor.textarea.set_search_pattern("");
    } else if let Some(query) = editor_search.or(last_editor_search) {
        let _ = editor.textarea.set_search_pattern(query);
        editor.textarea.set_search_style(
            Style::default()
                .bg(ui().editor_search_bg)
                .fg(ui().editor_search_fg),
        );
    } else {
        let _ = editor.textarea.set_search_pattern("");
    }

    // Publish the editor rect's text height so scroll helpers can clamp
    // the cursor without recomputing layout.
    editor.set_viewport_height(chunks[1].height);
    f.render_widget(&editor.textarea, chunks[1]);

    // Gutter diagnostic signs: paint a severity marker in the leftmost
    // gutter cell for any row with an LSP diagnostic. Highest severity
    // wins per row (error > warning).
    let gutter_signs: Vec<GutterSign> = state
        .lsp_diagnostics
        .iter()
        .filter_map(|d| match d.severity {
            lsp_types::DiagnosticSeverity::ERROR => Some(GutterSign {
                row: d.line as usize,
                ch: '●',
                fg: ui().status_diag_error,
                priority: 2,
            }),
            lsp_types::DiagnosticSeverity::WARNING => Some(GutterSign {
                row: d.line as usize,
                ch: '⚠',
                fg: ui().status_diag_warning,
                priority: 1,
            }),
            _ => None,
        })
        .collect();
    paint_gutter_signs(f, &editor.textarea, chunks[1], &gutter_signs);

    // All three visual modes paint their highlight as a post-render
    // overlay so the cursor can sit at its natural column (matches vim)
    // and tree-sitter styling stays intact underneath.
    if let Some((start, end)) = editor.char_highlight() {
        paint_char_overlay(f, &editor.textarea, chunks[1], start, end);
    }
    if let Some((top, bot)) = editor.line_highlight() {
        paint_line_overlay(f, &editor.textarea, chunks[1], top, bot);
    }

    // Visual-block selection is painted as a buffer-level overlay so it
    // lands on top of any tree-sitter styling. Trying to do this via
    // tui-textarea's per-row syntax spans doesn't work: `emit_with_syntax`
    // picks the *first* span that contains the cursor position, so a
    // second (block) span over an already-styled keyword is ignored.
    if let Some((top, bot, left, right)) = editor.block_highlight() {
        paint_block_overlay(f, &editor.textarea, chunks[1], top, bot, left, right);
    }

    if let Some(msg) = diag_line {
        f.render_widget(
            Paragraph::new(msg).style(Style::default().fg(ui().editor_error_fg)),
            chunks[2],
        );
    }
}

fn build_tab_title(state: &AppState) -> Line<'_> {
    let mut spans: Vec<Span> = vec![];
    for (i, tab) in state.tabs.iter().enumerate() {
        let active = i == state.active_tab;
        let style = if active {
            Style::default()
                .fg(ui().tab_active_fg)
                .bg(ui().tab_active_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(ui().tab_inactive_fg)
        };
        spans.push(Span::styled(format!(" {} ", tab.name), style));
        if i + 1 < state.tabs.len() {
            spans.push(Span::styled("│", Style::default().fg(ui().tab_sep_fg)));
        }
    }
    Line::from(spans)
}

fn draw_results(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    area: Rect,
    focused: bool,
    search_text: Option<&str>,
) {
    // Fill pane background (full area), then inset content by 1 on all sides.
    f.render_widget(
        Block::default().style(Style::default().bg(ui().results_pane_bg)),
        area,
    );
    let area = area.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 0,
    });
    // When the `/` prompt is live, carve one row off the bottom and
    // render the search input there; the rest of the pane lays out as
    // usual in the shrunk area.
    let (area, prompt_area) = if search_text.is_some() && area.height > 1 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    };

    // Split off a separator + 1-row tab bar at the top when there are multiple
    // result tabs. The separator sits above the tab strip.
    let sep_style = Style::default().fg(ui().results_sep);
    let (tab_bar_area, content_area) = if state.result_tabs.len() > 1 && area.height > 2 {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .split(area);
        let hr: String = "─".repeat(area.width as usize);
        f.render_widget(Paragraph::new(hr).style(sep_style), chunks[0]);
        (Some(chunks[1]), chunks[2])
    } else {
        (None, area)
    };

    if let Some(tab_area) = tab_bar_area {
        f.render_widget(results_tab_bar(state), tab_area);
    }

    match state.results() {
        ResultsPane::Results(r) => {
            let title_style = if focused {
                Style::default().fg(ui().results_title_active)
            } else {
                Style::default().fg(ui().results_title_inactive)
            };

            let query_text = state
                .active_result()
                .map(|t| t.query.clone())
                .unwrap_or_default();

            // `SHOW CREATE TABLE/VIEW/...` returns a single row whose last
            // column holds the DDL. Render that as a syntax-highlighted block
            // instead of a 1x2 table, which is unreadable.
            if is_show_create(&query_text)
                && r.rows.len() == 1
                && r.columns.len() >= 2
                && let Some(ddl) = r.rows[0].last()
            {
                let sep_style = Style::default().fg(ui().results_sep);
                let title = if state.result_tabs.len() > 1 {
                    format!(
                        " Results ({}/{} • DDL)",
                        state.active_result_tab + 1,
                        state.result_tabs.len()
                    )
                } else {
                    " Results (DDL)".to_string()
                };
                let query_line = highlight_query_line(&query_text, state.active_dialect);
                let body_lines = highlight_sql_lines(ddl, state.active_dialect);
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),
                        Constraint::Length(1),
                        Constraint::Length(1),
                        Constraint::Length(1),
                        Constraint::Length(1),
                        Constraint::Min(0),
                    ])
                    .split(content_area);
                let hr: String = "─".repeat(content_area.width as usize);
                f.render_widget(Paragraph::new(hr.clone()).style(sep_style), chunks[0]);
                f.render_widget(Paragraph::new(title).style(title_style), chunks[1]);
                f.render_widget(Paragraph::new(hr.clone()).style(sep_style), chunks[2]);
                f.render_widget(Paragraph::new(query_line), chunks[3]);
                f.render_widget(Paragraph::new(hr).style(sep_style), chunks[4]);
                let body_area = chunks[5];
                state
                    .results_body_rows
                    .store(body_area.height, std::sync::atomic::Ordering::Relaxed);
                state
                    .results_body_width
                    .store(body_area.width, std::sync::atomic::Ordering::Relaxed);
                let v_scroll = state.results_scroll().min(body_lines.len()) as u16;
                let h_scroll = state.results_col_scroll() as u16;
                f.render_widget(
                    Paragraph::new(body_lines).scroll((v_scroll, h_scroll)),
                    body_area,
                );
                return;
            }

            let title = if state.result_tabs.len() > 1 {
                format!(
                    " Results ({}/{} • {} rows)",
                    state.active_result_tab + 1,
                    state.result_tabs.len(),
                    r.rows.len()
                )
            } else {
                format!(" Results ({} rows)", r.rows.len())
            };
            let col_start = state.results_col_scroll();
            let sep_style = Style::default().fg(ui().results_sep);
            let header_style = Style::default()
                .fg(ui().results_header_active)
                .add_modifier(Modifier::BOLD);

            // Char offset into the full-width row string, derived from col_scroll.
            // Each rendered column is padded to col_widths[i], separated by `│`.
            let char_offset: u16 = r
                .col_widths
                .iter()
                .take(col_start)
                .map(|&w| w as u32 + 1)
                .sum::<u32>() as u16;

            let cursor = state.active_result().map(|t| t.cursor);
            let col_bg = results_cursor_bg(focused);
            let cursor_bg = results_cursor_bg_strong(focused);
            // Highlighted column (Header or Cell cursor) — whole column gets muted bg.
            let active_col: Option<usize> = match cursor {
                Some(ResultsCursor::Header(c)) | Some(ResultsCursor::Cell { col: c, .. }) => {
                    Some(c)
                }
                _ => None,
            };
            let cursor_row: Option<usize> = match cursor {
                Some(ResultsCursor::Cell { row, .. }) => Some(row),
                _ => None,
            };

            let selection_bounds = state.results_selection_bounds();
            let header_cursor_col = if matches!(cursor, Some(ResultsCursor::Header(_))) {
                active_col
            } else {
                None
            };
            let (header_line, body_lines) = render_grid_lines(
                &r.columns,
                &r.rows,
                &r.col_widths,
                cursor_row,
                active_col,
                header_cursor_col,
                selection_bounds,
                state.results_scroll(),
                header_style,
                sep_style,
                Style::default().bg(cursor_bg),
                Style::default().bg(col_bg),
            );

            let mut query_line = highlight_query_line(&query_text, state.active_dialect);
            if cursor == Some(ResultsCursor::Query) {
                let qbg = results_cursor_bg(focused);
                query_line = Line::from(
                    query_line
                        .spans
                        .into_iter()
                        .map(|s| {
                            let st = s.style.bg(qbg);
                            Span::styled(s.content, st)
                        })
                        .collect::<Vec<_>>(),
                );
            }

            // Split content_area: hr (1) + title (1) + hr (1) + query (1) + hr (1) + header (1) + hr (1) + body (rest).
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Min(0),
                ])
                .split(content_area);

            let hr: String = "─".repeat(content_area.width as usize);
            f.render_widget(Paragraph::new(hr.clone()).style(sep_style), chunks[0]);
            f.render_widget(Paragraph::new(title).style(title_style), chunks[1]);
            f.render_widget(Paragraph::new(hr.clone()).style(sep_style), chunks[2]);
            f.render_widget(Paragraph::new(query_line), chunks[3]);
            f.render_widget(Paragraph::new(hr.clone()).style(sep_style), chunks[4]);
            f.render_widget(
                Paragraph::new(header_line).scroll((0, char_offset)),
                chunks[5],
            );
            f.render_widget(Paragraph::new(hr).style(sep_style), chunks[6]);
            let body_area = chunks[7];
            use std::sync::atomic::Ordering;
            state.results_body_x.store(body_area.x, Ordering::Relaxed);
            state.results_body_y.store(body_area.y, Ordering::Relaxed);
            state
                .results_body_rows
                .store(body_area.height, Ordering::Relaxed);
            state
                .results_body_width
                .store(body_area.width, Ordering::Relaxed);
            f.render_widget(
                Paragraph::new(body_lines).scroll((0, char_offset)),
                body_area,
            );
        }
        ResultsPane::Error(e) => {
            let title_text = render_pos_title(state, "Result");
            let cursor = state.active_result().map(|t| t.cursor);
            let cursor_bg = results_cursor_bg(focused);
            let body: Vec<Line<'static>> = e
                .lines()
                .enumerate()
                .map(|(i, el)| {
                    let mut st = Style::default().fg(ui().results_error);
                    if cursor == Some(ResultsCursor::MessageLine(i)) {
                        st = st.bg(cursor_bg);
                    }
                    Line::from(Span::styled(format!(" {}", el), st))
                })
                .collect();
            let has_query = state
                .active_result()
                .map(|t| !t.query.trim().is_empty())
                .unwrap_or(false);
            render_framed_pane(
                f,
                content_area,
                &title_text,
                Style::default().fg(ui().results_error),
                state,
                body,
                has_query,
            );
        }
        ResultsPane::Loading => {
            let frame = spinner_frame();
            let title_text = render_pos_title(state, "Result");
            let body = vec![Line::from(Span::styled(
                format!(" {} Running query…", frame),
                Style::default().fg(ui().results_loading),
            ))];
            render_framed_pane(
                f,
                content_area,
                &title_text,
                Style::default().fg(ui().results_loading),
                state,
                body,
                false,
            );
        }
        ResultsPane::Cancelled => {
            let title_text = render_pos_title(state, "Result");
            let cursor = state.active_result().map(|t| t.cursor);
            let mut st = Style::default().fg(ui().results_cancelled);
            if matches!(cursor, Some(ResultsCursor::MessageLine(_))) {
                st = st.bg(results_cursor_bg(focused));
            }
            let body = vec![Line::from(Span::styled(
                " Skipped (previous query failed)",
                st,
            ))];
            let has_query = state
                .active_result()
                .map(|t| !t.query.trim().is_empty())
                .unwrap_or(false);
            render_framed_pane(
                f,
                content_area,
                &title_text,
                Style::default().fg(ui().results_cancelled),
                state,
                body,
                has_query,
            );
        }
        ResultsPane::Empty => unreachable!(),
    }

    // Paint the `/` prompt row last so it sits on top of whatever the
    // content renderer drew above.
    if let (Some(rect), Some(text)) = (prompt_area, search_text) {
        let style = Style::default().fg(ui().status_mode_normal);
        f.render_widget(Paragraph::new(format!("/{text}")).style(style), rect);
    }
}

/// Muted background for the currently-highlighted column in the results pane —
/// mirrors the editor's `cursor_line_bg` so focus feels consistent.
fn results_cursor_bg(focused: bool) -> Color {
    if focused {
        ui().results_col_active_bg
    } else {
        ui().results_col_inactive_bg
    }
}

/// Build the header + body `Line`s for a tabular grid. Used by both
/// the results pane and the LSP hover popup so their styling stays in
/// lock-step (cursor cell, column-wide bg, selection rectangle).
///
/// - `cursor_row` / `active_col` drive the cursor-cell + column-wide
///   backgrounds on body rows. Pass `None` for each if the cursor isn't
///   on the grid.
/// - `header_cursor_col` flips the header cell at that column to the
///   strong cursor bg (used when the results cursor is on the header
///   row; hover grids don't have a header cursor so pass `None`).
/// - `selection_bounds` is `(top, bot, left, right)` when a visual
///   selection is active. Rows inside it take the muted column bg.
/// - `body_skip` drops N leading body rows (row-scroll offset).
#[allow(clippy::too_many_arguments)]
fn render_grid_lines(
    columns: &[String],
    rows: &[Vec<String>],
    col_widths: &[u16],
    cursor_row: Option<usize>,
    active_col: Option<usize>,
    header_cursor_col: Option<usize>,
    selection_bounds: Option<(usize, usize, usize, usize)>,
    body_skip: usize,
    header_style: Style,
    sep_style: Style,
    cursor_style: Style,
    selection_style: Style,
) -> (Line<'static>, Vec<Line<'static>>) {
    let col_count = columns.len();
    let mut header_spans: Vec<Span<'static>> = Vec::with_capacity(col_count * 2);
    for (i, c) in columns.iter().enumerate() {
        let w = col_widths.get(i).copied().unwrap_or(0) as usize;
        let inner = w.saturating_sub(1);
        let mut st = header_style;
        if header_cursor_col == Some(i) {
            st = st.patch(cursor_style);
        }
        header_spans.push(Span::styled(format!(" {:<inner$}", c, inner = inner), st));
        if i + 1 < col_count {
            header_spans.push(Span::styled("│".to_string(), sep_style));
        }
    }
    let header_line = Line::from(header_spans);

    let body_lines: Vec<Line<'static>> = rows
        .iter()
        .enumerate()
        .skip(body_skip)
        .map(|(row_idx, row)| {
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(col_count * 2);
            for i in 0..col_count {
                let w = col_widths.get(i).copied().unwrap_or(0) as usize;
                let inner = w.saturating_sub(1);
                let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
                let text = format!(" {:<inner$}", cell, inner = inner);
                let is_cursor = cursor_row == Some(row_idx) && active_col == Some(i);
                let is_selected = selection_bounds
                    .is_some_and(|(t, b, l, rr)| row_idx >= t && row_idx <= b && i >= l && i <= rr);
                let style = if is_cursor {
                    Some(cursor_style)
                } else if is_selected {
                    Some(selection_style)
                } else {
                    None
                };
                if let Some(st) = style {
                    spans.push(Span::styled(text, st));
                } else {
                    spans.push(Span::raw(text));
                }
                if i + 1 < col_count {
                    spans.push(Span::styled("│".to_string(), sep_style));
                }
            }
            Line::from(spans)
        })
        .collect();

    (header_line, body_lines)
}

/// Slightly brighter bg used for the single cell (or header) the cursor
/// actually points at, sitting on top of the column-wide muted bg.
fn results_cursor_bg_strong(focused: bool) -> Color {
    if focused {
        ui().results_cursor_active_bg
    } else {
        ui().results_cursor_inactive_bg
    }
}

fn render_pos_title(state: &AppState, label: &str) -> String {
    if state.result_tabs.len() > 1 {
        format!(
            " {label} ({}/{})",
            state.active_result_tab + 1,
            state.result_tabs.len(),
        )
    } else {
        format!(" {label}")
    }
}

/// Draw the hr/title/hr/query/hr chrome shared by Error, Loading, and
/// Cancelled panes, then the caller-supplied body below. When `show_query`
/// is false the query row + its trailing separator are omitted.
fn render_framed_pane(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    title: &str,
    title_style: Style,
    state: &AppState,
    body: Vec<Line<'static>>,
    show_query: bool,
) {
    let sep_style = Style::default().fg(ui().results_sep);
    let hr: String = "─".repeat(area.width as usize);
    let query_text = state
        .active_result()
        .map(|t| t.query.clone())
        .unwrap_or_default();

    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(1), // hr
        Constraint::Length(1), // title
        Constraint::Length(1), // hr
    ];
    if show_query {
        constraints.push(Constraint::Length(1)); // query
        constraints.push(Constraint::Length(1)); // hr
    }
    constraints.push(Constraint::Min(0));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    f.render_widget(Paragraph::new(hr.clone()).style(sep_style), chunks[0]);
    f.render_widget(
        Paragraph::new(title.to_string()).style(title_style),
        chunks[1],
    );
    f.render_widget(Paragraph::new(hr.clone()).style(sep_style), chunks[2]);
    let body_chunk = if show_query {
        let mut query_line = highlight_query_line(&query_text, state.active_dialect);
        let cursor = state.active_result().map(|t| t.cursor);
        if state.focus == Focus::Results && cursor == Some(ResultsCursor::Query) {
            let qbg = results_cursor_bg(state.focus == Focus::Results);
            query_line = Line::from(
                query_line
                    .spans
                    .into_iter()
                    .map(|s| {
                        let st = s.style.bg(qbg);
                        Span::styled(s.content, st)
                    })
                    .collect::<Vec<_>>(),
            );
        }
        f.render_widget(Paragraph::new(query_line), chunks[3]);
        f.render_widget(Paragraph::new(hr).style(sep_style), chunks[4]);
        chunks[5]
    } else {
        chunks[3]
    };
    state
        .results_body_rows
        .store(body_chunk.height, std::sync::atomic::Ordering::Relaxed);
    state
        .results_body_width
        .store(body_chunk.width, std::sync::atomic::Ordering::Relaxed);
    let scroll_y = state.active_result().map(|t| t.scroll as u16).unwrap_or(0);
    f.render_widget(
        Paragraph::new(body)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .scroll((scroll_y, 0)),
        body_chunk,
    );
}

/// Render a 1-row tab bar above the results pane: numbered tabs with the active
/// one highlighted in cyan.
fn results_tab_bar(state: &AppState) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(state.result_tabs.len() * 2);
    for (i, tab) in state.result_tabs.iter().enumerate() {
        let is_err = matches!(tab.kind, ResultsPane::Error(_));
        let is_loading = matches!(tab.kind, ResultsPane::Loading);
        let is_cancelled = matches!(tab.kind, ResultsPane::Cancelled);
        let label = format!(" {} ", i + 1);
        let u = ui();
        let style = if i == state.active_result_tab {
            Style::default()
                .fg(u.tab_active_fg)
                .bg(if is_err {
                    u.tab_err_bg
                } else if is_loading {
                    u.tab_loading_bg
                } else if is_cancelled {
                    u.tab_cancel_bg
                } else {
                    u.tab_active_bg
                })
                .add_modifier(Modifier::BOLD)
        } else if is_err {
            Style::default().fg(u.tab_err_fg)
        } else if is_loading {
            Style::default().fg(u.tab_loading_fg)
        } else if is_cancelled {
            Style::default().fg(u.tab_cancel_fg)
        } else {
            Style::default().fg(u.results_header_active)
        };
        spans.push(Span::styled(label, style));
        if i + 1 < state.result_tabs.len() {
            spans.push(Span::styled("│", Style::default().fg(u.tab_sep_fg)));
        }
    }
    Line::from(spans)
}

/// Build a syntax-highlighted single-line Line for the results-pane query row.
/// Newlines in the source are collapsed to spaces. Byte offsets from the
/// highlighter refer to the original (multiline) source — we remap them onto
/// the flattened string so spans stay aligned.
/// Render `source` as syntax-highlighted lines. Spans crossing line breaks
/// are split per row. Shared tree-sitter parser kept in TLS (same pattern as
/// `highlight_query_line`).
fn highlight_sql_lines(source: &str, dialect: Dialect) -> Vec<Line<'static>> {
    use std::cell::RefCell;
    thread_local! {
        static HL: RefCell<Option<Highlighter>> = const { RefCell::new(None) };
    }

    let spans = HL.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none()
            && let Ok(h) = Highlighter::new()
        {
            *slot = Some(h);
        }
        slot.as_mut()
            .map(|h| h.highlight(source, dialect))
            .unwrap_or_default()
    });

    let bytes = source.as_bytes();
    let plain = Style::default().fg(ui().sql_plain);

    // Byte range of each line (without the trailing newline).
    let mut line_ranges: Vec<(usize, usize)> = Vec::new();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            line_ranges.push((start, i));
            start = i + 1;
        }
    }
    line_ranges.push((start, bytes.len()));

    line_ranges
        .iter()
        .map(|&(ls, le)| {
            let mut out: Vec<Span<'static>> = Vec::new();
            let mut cursor = ls;
            for s in &spans {
                let sb = s.start_byte.max(ls);
                let eb = s.end_byte.min(le);
                if sb >= eb {
                    continue;
                }
                if sb > cursor
                    && let Ok(raw) = std::str::from_utf8(&bytes[cursor..sb])
                {
                    out.push(Span::styled(raw.to_string(), plain));
                }
                if let Ok(raw) = std::str::from_utf8(&bytes[sb..eb]) {
                    let style = token_kind_style(s.kind).unwrap_or(plain);
                    out.push(Span::styled(raw.to_string(), style));
                }
                cursor = eb;
            }
            if cursor < le
                && let Ok(raw) = std::str::from_utf8(&bytes[cursor..le])
            {
                out.push(Span::styled(raw.to_string(), plain));
            }
            if out.is_empty() {
                Line::from(Span::raw(""))
            } else {
                Line::from(out)
            }
        })
        .collect()
}

fn highlight_query_line(query: &str, dialect: Dialect) -> Line<'static> {
    use std::cell::RefCell;
    thread_local! {
        static HL: RefCell<Option<Highlighter>> = const { RefCell::new(None) };
    }

    if query.is_empty() {
        return Line::from(vec![Span::raw(" ")]);
    }

    let spans = HL.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none()
            && let Ok(h) = Highlighter::new()
        {
            *slot = Some(h);
        }
        slot.as_mut()
            .map(|h| h.highlight(query, dialect))
            .unwrap_or_default()
    });

    let bytes = query.as_bytes();
    let mut out: Vec<Span<'static>> = vec![Span::raw(" ")];
    let plain = Style::default().fg(ui().sql_plain);
    let mut cursor = 0usize;
    let flatten = |b: &[u8]| -> String {
        std::str::from_utf8(b)
            .unwrap_or("")
            .replace(['\n', '\r'], " ")
    };

    for s in &spans {
        if s.start_byte >= bytes.len() || s.end_byte > bytes.len() || s.start_byte >= s.end_byte {
            continue;
        }
        if s.start_byte > cursor {
            out.push(Span::styled(flatten(&bytes[cursor..s.start_byte]), plain));
        }
        let slice = flatten(&bytes[s.start_byte..s.end_byte]);
        let style = token_kind_style(s.kind).unwrap_or(plain);
        out.push(Span::styled(slice, style));
        cursor = s.end_byte;
    }
    if cursor < bytes.len() {
        out.push(Span::styled(flatten(&bytes[cursor..]), plain));
    }
    Line::from(out)
}

/// Commit each [`PendingSave`] on a blocking task so multi-MB writes
/// don't stall the render loop, then clear the matching tab's dirty
/// flag on success. Returns the names of saves that failed.
async fn commit_pending_saves(
    state: &Arc<Mutex<AppState>>,
    pending: Vec<sqeel_core::state::PendingSave>,
) -> Vec<String> {
    let mut failed = Vec::new();
    for p in pending {
        let tab_index = p.tab_index;
        let name = p.name.clone();
        let commit = tokio::task::spawn_blocking(move || p.commit())
            .await
            .unwrap_or_else(|e| {
                Err(std::io::Error::other(format!(
                    "spawn_blocking join error: {e}"
                )))
            });
        match commit {
            Ok(()) => {
                if let Some(i) = tab_index {
                    state.lock().unwrap().mark_tab_saved(i);
                }
            }
            Err(_) => failed.push(name),
        }
    }
    failed
}

/// Combine the LSP diagnostics vector with tree-sitter-derived parse
/// errors into one list for the inline-underline overlay. Parse errors
/// are lifted to `ERROR` severity so they render with the same loud
/// styling as an LSP error — they're "why did my SQL not run" markers
/// either way.
fn merged_diagnostics(
    lsp: &[sqeel_core::lsp::Diagnostic],
    parse_errors: &[sqeel_core::highlight::ParseError],
) -> Vec<sqeel_core::lsp::Diagnostic> {
    let mut out: Vec<sqeel_core::lsp::Diagnostic> = lsp.to_vec();
    out.extend(parse_errors.iter().map(|e| sqeel_core::lsp::Diagnostic {
        line: e.start_row as u32,
        col: e.start_col as u32,
        end_line: e.end_row as u32,
        end_col: e.end_col as u32,
        message: e.message.clone(),
        severity: lsp_types::DiagnosticSeverity::ERROR,
    }));
    out
}

/// Decide whether the highlight worker needs a fresh submission.
///
/// Fires on:
/// - `content_changed` — user edited the buffer.
/// - `viewport_scrolled` — viewport moved far enough that the current
///   parse window no longer covers what's on screen.
/// - A dialect flip — the DB handshake is async, so the first parse at
///   startup runs under `Dialect::Generic`. Once the connection resolves
///   and sets the concrete dialect we need to re-parse so dialect-specific
///   keyword promotion (DESC / SHOW / PRAGMA / …) kicks in.
fn should_resubmit_highlight(
    content_changed: bool,
    viewport_scrolled: bool,
    current_dialect: Dialect,
    last_dialect: Dialect,
) -> bool {
    content_changed || viewport_scrolled || current_dialect != last_dialect
}

fn token_kind_style(kind: TokenKind) -> Option<Style> {
    let u = ui();
    match kind {
        TokenKind::Keyword => Some(
            Style::default()
                .fg(u.sql_keyword)
                .add_modifier(Modifier::BOLD),
        ),
        TokenKind::String => Some(Style::default().fg(u.sql_string)),
        TokenKind::Comment => Some(
            Style::default()
                .fg(u.sql_comment)
                .add_modifier(Modifier::ITALIC),
        ),
        TokenKind::Number => Some(Style::default().fg(u.sql_number)),
        TokenKind::Operator => Some(Style::default().fg(u.sql_operator)),
        TokenKind::Identifier | TokenKind::Plain => None,
    }
}

/// Splice a window of tree-sitter spans into the textarea's existing
/// per-row syntax span table.  Spans in the result are slice-local —
/// rows are rebased by `result.start_row` before being written.
///
/// Avoids the 700k-row `vec![Vec::new(); row_count]` allocation the old
/// `syntax_spans_by_row` paid on the main thread every time a highlight
/// result arrived: we `take` the existing outer `Vec`, mutate only the
/// rows inside the window, and put it back.
fn apply_window_spans(
    textarea: &mut tui_textarea::TextArea<'_>,
    result: &HighlightResult,
    buffer_rows: usize,
    cursor_row: usize,
    diagnostics: &[sqeel_core::lsp::Diagnostic],
) {
    let mut by_row = textarea.take_syntax_spans();
    if by_row.len() < buffer_rows {
        by_row.resize_with(buffer_rows, Vec::new);
    }
    let window_start = result.start_row;
    let window_end = (window_start + result.row_count).min(buffer_rows);
    for row_spans in by_row.iter_mut().take(window_end).skip(window_start) {
        row_spans.clear();
    }
    // Per-row comment bodies derived from tree-sitter's Comment spans,
    // so we only treat `--` / `/*` as a comment when the parser agrees
    // (no false positives inside string literals, and block comments
    // end at `*/` not at EOL).
    let mut comment_ranges_by_row: Vec<Vec<CommentBody>> = vec![Vec::new(); buffer_rows];
    let textarea_lines = textarea.lines();
    for s in &result.spans {
        let Some(style) = token_kind_style(s.kind) else {
            continue;
        };
        let sr = s.start_row + window_start;
        let er = s.end_row + window_start;
        if sr >= buffer_rows {
            continue;
        }
        if sr == er {
            if s.end_col > s.start_col {
                by_row[sr].push((s.start_col, s.end_col, style));
                if s.kind == TokenKind::Comment {
                    comment_ranges_by_row[sr].push(comment_body_from_span(
                        &textarea_lines[sr],
                        s.start_col,
                        s.end_col,
                    ));
                }
            }
        } else {
            by_row[sr].push((s.start_col, usize::MAX, style));
            for row_spans in by_row.iter_mut().take(er.min(buffer_rows)).skip(sr + 1) {
                row_spans.push((0, usize::MAX, style));
            }
            if er < buffer_rows && s.end_col > 0 {
                by_row[er].push((0, s.end_col, style));
            }
            if s.kind == TokenKind::Comment {
                let first_end = textarea_lines[sr].len();
                comment_ranges_by_row[sr].push(comment_body_from_span(
                    &textarea_lines[sr],
                    s.start_col,
                    first_end,
                ));
                for row in (sr + 1)..er.min(buffer_rows) {
                    comment_ranges_by_row[row].push(CommentBody {
                        start: 0,
                        end: textarea_lines[row].len(),
                    });
                }
                if er < buffer_rows && s.end_col > 0 {
                    comment_ranges_by_row[er].push(CommentBody {
                        start: 0,
                        end: s.end_col.min(textarea_lines[er].len()),
                    });
                }
            }
        }
    }
    // Overlay TODO-family comment markers: parse each row's text, splice
    // marker spans in so they override any covering tree-sitter comment
    // span. `line_spans` picks the first matching span for a given byte,
    // so we have to split overlapping spans around the marker rather than
    // just pushing on top.
    //
    // Active colour inheritance: a comment line without its own marker
    // inherits the most recent marker colour from the contiguous comment
    // block above it. A non-comment line resets the inheritance. Seed the
    // state by scanning backwards from `window_start` until we hit either
    // a marker or a non-comment line (capped so huge files don't pay).
    let mut active_color = seed_active_color(textarea, window_start);
    for (row, row_spans) in by_row
        .iter_mut()
        .enumerate()
        .take(window_end)
        .skip(window_start)
    {
        let line = &textarea_lines[row];
        let on_cursor_line = row == cursor_row;
        let comments = &comment_ranges_by_row[row];
        active_color =
            apply_marker_overlay(row_spans, line, comments, active_color, on_cursor_line);
    }
    // LSP diagnostic underlines. Applied last so the underline stacks
    // on top of the keyword / marker overlays; we preserve the existing
    // span's fg and just layer on an error-coloured underline.
    for d in diagnostics {
        apply_diagnostic_underline(&mut by_row, d, textarea_lines, buffer_rows);
    }
    // Sort each touched row so `line_spans` sees them in start-byte order.
    for row_spans in by_row.iter_mut().take(window_end).skip(window_start) {
        row_spans.sort_by_key(|&(s, _, _)| s);
    }
    textarea.set_syntax_spans(by_row);
}

/// A located TODO-family marker: the byte span of the marker word and
/// the color associated with it.
struct Marker {
    word_start: usize,
    word_end: usize,
    color: Color,
}

/// Byte range of a single comment's *body* on a given line — the span
/// between the comment's start delimiter (e.g. `--`, `/*`) and its end.
/// Sourced from tree-sitter comment spans inside the highlight window;
/// the backward seed scan that runs outside that window uses the
/// [`comment_body_from_line`] string fallback.
#[derive(Clone, Copy)]
struct CommentBody {
    start: usize,
    end: usize,
}

/// Build a `CommentBody` from a tree-sitter comment span's byte range
/// on `line`. Skips the `--` or `/*` delimiter (2 bytes) if the span
/// starts with one; otherwise (continuation row of a multi-line block
/// comment) uses the span start as-is.
fn comment_body_from_span(line: &str, span_start: usize, span_end: usize) -> CommentBody {
    let bytes = line.as_bytes();
    let delim = if span_start + 1 < bytes.len() {
        let (a, b) = (bytes[span_start], bytes[span_start + 1]);
        if (a == b'-' && b == b'-') || (a == b'/' && b == b'*') {
            2
        } else {
            0
        }
    } else {
        0
    };
    CommentBody {
        start: (span_start + delim).min(line.len()),
        end: span_end.min(line.len()),
    }
}

/// String-scan fallback for rows outside the tree-sitter window (the
/// backward seed scan). Known to false-positive inside string literals
/// — OK for the 500-row seed cap where a small mis-read self-corrects
/// at the first real marker.
fn comment_body_from_line(line: &str) -> Option<CommentBody> {
    let hit = [line.find("--"), line.find("/*")]
        .into_iter()
        .flatten()
        .min()?;
    Some(CommentBody {
        start: hit + 2,
        end: line.len(),
    })
}

/// Find every TODO / FIXME / FIX / NOTE / WARN / INFO marker inside the
/// byte range `[body.start, body.end)` of `line`. Results are sorted
/// by position.
fn scan_markers(line: &str, body: CommentBody) -> Vec<Marker> {
    let u = ui();
    let words: [(&str, Color); 6] = [
        ("TODO", u.sql_marker_todo),
        ("FIXME", u.sql_marker_fixme),
        ("FIX", u.sql_marker_fixme),
        ("NOTE", u.sql_marker_note),
        ("INFO", u.sql_marker_note),
        ("WARN", u.sql_marker_warn),
    ];
    let end = body.end.min(line.len());
    if body.start >= end {
        return Vec::new();
    }
    let bytes = &line.as_bytes()[body.start..end];
    let mut out = Vec::new();
    for (word, color) in words {
        let wbytes = word.as_bytes();
        let mut i = 0usize;
        while i + wbytes.len() <= bytes.len() {
            if &bytes[i..i + wbytes.len()] == wbytes {
                let left_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
                let right_byte = bytes.get(i + wbytes.len()).copied();
                let right_ok = right_byte
                    .map(|b| !b.is_ascii_alphanumeric())
                    .unwrap_or(true);
                if left_ok && right_ok {
                    out.push(Marker {
                        word_start: body.start + i,
                        word_end: body.start + i + wbytes.len(),
                        color,
                    });
                    i += wbytes.len();
                    continue;
                }
            }
            i += 1;
        }
    }
    out.sort_by_key(|m| m.word_start);
    out
}

/// Emit marker / tail spans for one line onto `row_spans`, honouring
/// `active_color` inherited from the previous comment line. Returns the
/// active colour for the *next* line — which is:
/// - `None` if this line is not a `--` comment (block break),
/// - the last marker colour seen on this line, or
/// - the inherited colour passed in (no new marker).
fn apply_marker_overlay(
    row_spans: &mut Vec<(usize, usize, Style)>,
    line: &str,
    comments: &[CommentBody],
    active_color: Option<Color>,
    on_cursor_line: bool,
) -> Option<Color> {
    if comments.is_empty() {
        return None;
    }
    let u = ui();
    let tail_style = |fg| Style::default().fg(fg).add_modifier(Modifier::ITALIC);
    // On the cursor row we blend the marker WORD into its badge by
    // matching fg = bg, so the edit line stays visually calm while the
    // badge colour is still visible.
    let label_style = |bg| {
        let fg = if on_cursor_line { bg } else { u.sql_marker_fg };
        Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD)
    };

    let mut current = active_color;
    for body in comments {
        let markers = scan_markers(line, *body);
        let body_end = body.end.min(line.len());
        if body.start >= body_end {
            continue;
        }

        if markers.is_empty() {
            // Continuation body — inherit the active colour across it.
            if let Some(c) = current {
                overlay_span(row_spans, body.start, body_end, tail_style(c));
            }
            continue;
        }

        let mut cursor = body.start;
        for m in &markers {
            // Include the one char before the marker in the label, but
            // never swallow the comment delimiter (clamped to body.start).
            let label_start = m.word_start.saturating_sub(1).max(body.start);
            if let Some(c) = current
                && cursor < label_start
            {
                overlay_span(row_spans, cursor, label_start, tail_style(c));
            }
            overlay_span(row_spans, label_start, m.word_end, label_style(m.color));
            // Trailing char — a `:` always blends into the badge
            // (fg = bg); the cursor-line highlight handles editing
            // visibility so no mode-specific flip.
            let trail_end = if m.word_end < body_end {
                let next = line.as_bytes()[m.word_end];
                let trail_fg = if next == b':' {
                    m.color
                } else {
                    u.sql_marker_fg
                };
                let style = Style::default()
                    .fg(trail_fg)
                    .bg(m.color)
                    .add_modifier(Modifier::BOLD);
                overlay_span(row_spans, m.word_end, m.word_end + 1, style);
                m.word_end + 1
            } else {
                m.word_end
            };
            cursor = trail_end;
            current = Some(m.color);
        }
        // Tail after the last marker on this comment: the current colour
        // carries to the end of the comment body.
        if let Some(c) = current
            && cursor < body_end
        {
            overlay_span(row_spans, cursor, body_end, tail_style(c));
        }
    }
    current
}

/// Walk backward from `window_start` to seed the inherited marker colour
/// for the first row of the window. Stops at the nearest non-comment
/// line (reset) or the nearest comment line that carries its own marker
/// (inherit that colour). Capped so huge buffers pay at most a bounded
/// cost per highlight refresh.
fn seed_active_color(textarea: &tui_textarea::TextArea<'_>, window_start: usize) -> Option<Color> {
    const SEED_SCAN_CAP: usize = 500;
    if window_start == 0 {
        return None;
    }
    let lines = textarea.lines();
    let start = window_start.saturating_sub(SEED_SCAN_CAP);
    // Walk down from the cap toward `window_start - 1`, updating the
    // active colour the same way the forward pass does. This gives the
    // correct seed even when the cap is hit mid-block.
    let mut active: Option<Color> = None;
    for line in &lines[start..window_start] {
        if let Some(body) = comment_body_from_line(line) {
            let markers = scan_markers(line, body);
            if let Some(last) = markers.last() {
                active = Some(last.color);
            }
            // else: inherit active unchanged.
        } else {
            active = None;
        }
    }
    active
}

#[cfg(test)]
fn find_comment_markers(line: &str) -> Vec<(usize, usize, Style)> {
    let mut row: Vec<(usize, usize, Style)> = Vec::new();
    let comments: Vec<CommentBody> = comment_body_from_line(line).into_iter().collect();
    apply_marker_overlay(&mut row, line, &comments, None, false);
    row.sort_by_key(|&(s, _, _)| s);
    row
}

/// Layer an LSP diagnostic's error / warning underline onto `by_row`
/// at the diagnostic's range. Existing spans in the range are split
/// and their fg preserved — we only add the `UNDERLINED` modifier and
/// paint the underline colour with the diagnostic severity colour, so
/// keyword / marker colouring inside the range still renders.
fn apply_diagnostic_underline(
    by_row: &mut [Vec<(usize, usize, Style)>],
    d: &sqeel_core::lsp::Diagnostic,
    lines: &[String],
    buffer_rows: usize,
) {
    let u = ui();
    let color = match d.severity {
        lsp_types::DiagnosticSeverity::ERROR => u.status_diag_error,
        lsp_types::DiagnosticSeverity::WARNING => u.status_diag_warning,
        _ => return,
    };
    let start_row = d.line as usize;
    let end_row = d.end_line as usize;
    if start_row >= buffer_rows {
        return;
    }
    let stop = end_row.min(buffer_rows.saturating_sub(1));
    for (row, row_spans) in by_row.iter_mut().enumerate().take(stop + 1).skip(start_row) {
        let line_len = lines.get(row).map(|l| l.len()).unwrap_or(0);
        let start_col = if row == start_row { d.col as usize } else { 0 };
        let mut end_col = if row == end_row {
            d.end_col as usize
        } else {
            line_len
        };
        // Zero-width ranges (LSP sometimes emits those) need to
        // highlight *something* — fall back to `start_col..line_end`,
        // clamped to at least one cell.
        if end_col <= start_col {
            end_col = line_len.max(start_col + 1);
        }
        end_col = end_col.min(line_len.max(start_col + 1));
        if start_col >= end_col {
            continue;
        }
        merge_underline(row_spans, start_col, end_col, color);
    }
}

/// Split `row` at `[start, end)` boundaries, adding `UNDERLINED`
/// modifier + `underline_color = color` to the overlap region of
/// each existing span. Uncovered bytes in `[start, end)` get a bare
/// underline span using `color` as both fg and underline colour.
fn merge_underline(row: &mut Vec<(usize, usize, Style)>, start: usize, end: usize, color: Color) {
    let mut out: Vec<(usize, usize, Style)> = Vec::with_capacity(row.len() + 4);
    let mut overlap_ranges: Vec<(usize, usize)> = Vec::new();
    for &(s, e, sty) in row.iter() {
        if e <= start || s >= end {
            out.push((s, e, sty));
            continue;
        }
        if s < start {
            out.push((s, start, sty));
        }
        let olap_s = s.max(start);
        let olap_e = e.min(end);
        // Replace the syntax fg with the diagnostic colour inside the
        // range so the underline reads loud against the editor bg even
        // in terminals without colored-underline support. The range is
        // small (usually one token) so losing syntax colour there is a
        // fair trade for unambiguous error visibility.
        let merged = sty
            .fg(color)
            .add_modifier(Modifier::UNDERLINED)
            .underline_color(color);
        out.push((olap_s, olap_e, merged));
        overlap_ranges.push((olap_s, olap_e));
        if e > end {
            out.push((end, e, sty));
        }
    }
    // Fill gaps in [start, end) uncovered by any existing span.
    overlap_ranges.sort_by_key(|&(s, _)| s);
    let bare = Style::default()
        .fg(color)
        .add_modifier(Modifier::UNDERLINED)
        .underline_color(color);
    let mut cursor = start;
    for (s, e) in overlap_ranges {
        if s > cursor {
            out.push((cursor, s, bare));
        }
        cursor = cursor.max(e);
    }
    if cursor < end {
        out.push((cursor, end, bare));
    }
    out.sort_by_key(|&(s, _, _)| s);
    *row = out;
}

/// Insert a marker span `[ms, me)` with `style` into `row`, trimming /
/// splitting any existing span that overlaps so the marker isn't masked
/// by an outer tree-sitter comment span.
fn overlay_span(row: &mut Vec<(usize, usize, Style)>, ms: usize, me: usize, style: Style) {
    let mut trimmed: Vec<(usize, usize, Style)> = Vec::with_capacity(row.len() + 2);
    for &(s, e, sty) in row.iter() {
        if e <= ms || s >= me {
            trimmed.push((s, e, sty));
        } else if s < ms && e > me {
            trimmed.push((s, ms, sty));
            trimmed.push((me, e, sty));
        } else if s < ms {
            trimmed.push((s, ms, sty));
        } else if e > me {
            trimmed.push((me, e, sty));
        }
        // else: span fully inside marker — drop it.
    }
    trimmed.push((ms, me, style));
    *row = trimmed;
}

/// Convert a `(row, col)` character position into a byte offset in the
/// joined source (`\n` between lines). Used to feed cursor position into
/// `completion_ctx::parse_context`, which operates on a single string.
fn row_col_to_byte(lines: &[String], row: usize, col: usize) -> usize {
    let mut offset = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if i == row {
            for (char_count, (b, _)) in line.char_indices().enumerate() {
                if char_count == col {
                    return offset + b;
                }
            }
            return offset + line.len();
        }
        offset += line.len() + 1; // `\n`
    }
    offset
}

/// Returns the word (alphanumeric + `_`) ending at `col` on `line`.
fn word_prefix_at(lines: &[String], row: usize, col: usize) -> String {
    let Some(line) = lines.get(row) else {
        return String::new();
    };
    let before = &line[..col.min(line.len())];
    before
        .chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

/// Render a tabular hover payload as a centred, borderless, focus-
/// stealing dialog. Chrome matches the command palette — 2-col / 1-row
/// padding on `dialog_bg`, no border rule.
fn draw_hover_table(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &AppState,
    table: &sqeel_core::state::QueryResult,
) {
    if area.width < 20 || area.height < 5 {
        return;
    }
    let u = ui();
    let bg = Style::default().fg(u.dialog_fg).bg(u.dialog_bg);

    // Natural width: sum of col_widths + separator per column gap.
    let natural_w: u32 = table
        .col_widths
        .iter()
        .map(|&w| w as u32 + 1)
        .sum::<u32>()
        .saturating_sub(1);
    let popup_w = (natural_w as u16)
        .saturating_add(4) // 2-col pad each side
        .clamp(40, area.width.saturating_sub(4).min(100));
    // Body max = terminal height - vertical padding (2) - borders
    // space (none) - header/hr (2). Popup height = header + hr + body
    // + 1 top + 1 bottom pad.
    let max_body = area.height.saturating_sub(4);
    let body_h = (table.rows.len() as u16).min(max_body.max(1));
    let popup_h = (body_h + 4).min(area.height.saturating_sub(2));

    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w,
        height: popup_h,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);

    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };
    // No title row — the popup is chromeless apart from `dialog_bg`.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    let sep_style = Style::default().fg(u.results_sep).bg(u.dialog_bg);
    let header_style = bg.add_modifier(Modifier::BOLD).fg(u.results_header_active);
    // Highlights for the hover popup: results-pane bg colors read as
    // identical to `dialog_bg` (both are dim neutral shades), so
    // rolling `REVERSED` in inverts bg/fg of whatever cell the cursor
    // or selection covers. Unconditionally visible regardless of the
    // active theme.
    let cursor_style = bg.add_modifier(Modifier::REVERSED | Modifier::BOLD);
    let selection_style = Style::default()
        .fg(u.editor_search_fg)
        .bg(u.editor_search_bg);

    let cursor_row = match state.hover_cursor {
        ResultsCursor::Cell { row, .. } => Some(row),
        _ => None,
    };
    let active_col = match state.hover_cursor {
        ResultsCursor::Cell { col, .. } => Some(col),
        _ => None,
    };
    let selection_bounds: Option<(usize, usize, usize, usize)> =
        state.hover_selection.and_then(|sel| {
            let (ar, ac) = sel.anchor;
            let (cr, cc) = match state.hover_cursor {
                ResultsCursor::Cell { row, col } => (row, col),
                _ => return None,
            };
            let top = ar.min(cr);
            let bot = ar.max(cr);
            let (left, right) = match sel.mode {
                sqeel_core::state::ResultsSelectionMode::Line => {
                    (0, table.columns.len().saturating_sub(1))
                }
                sqeel_core::state::ResultsSelectionMode::Block => (ac.min(cc), ac.max(cc)),
            };
            Some((top, bot, left, right))
        });

    let (header_line, body_lines) = render_grid_lines(
        &table.columns,
        &table.rows,
        &table.col_widths,
        cursor_row,
        active_col,
        None,
        selection_bounds,
        state.hover_scroll,
        header_style,
        sep_style,
        cursor_style,
        selection_style,
    );

    let char_offset: u16 = table
        .col_widths
        .iter()
        .take(state.hover_col_scroll)
        .map(|&w| w as u32 + 1)
        .sum::<u32>() as u16;

    f.render_widget(
        Paragraph::new(header_line)
            .style(bg)
            .scroll((0, char_offset)),
        chunks[0],
    );
    let hr: String = "─".repeat(inner.width as usize);
    f.render_widget(Paragraph::new(hr).style(sep_style), chunks[1]);
    let body_rect = chunks[2];
    // Publish the body rect so nav helpers can clamp the scroll
    // offsets and the mouse-click handler can translate terminal-
    // space coordinates into grid (row, col). Without this `l` past
    // the viewport leaves the cursor off-screen, and clicks inside
    // the popup can't hit their cell.
    use std::sync::atomic::Ordering;
    state.hover_body_x.store(body_rect.x, Ordering::Relaxed);
    state.hover_body_y.store(body_rect.y, Ordering::Relaxed);
    state
        .hover_body_height
        .store(body_rect.height, Ordering::Relaxed);
    state
        .hover_body_width
        .store(body_rect.width, Ordering::Relaxed);
    f.render_widget(
        Paragraph::new(body_lines)
            .style(bg)
            .scroll((0, char_offset)),
        body_rect,
    );
}

/// Loading placeholder rendered while the LSP hover response is in
/// flight. Uses the same borderless `dialog_bg` chrome so the
/// popup's footprint is consistent once content arrives.
fn draw_hover_loading(f: &mut ratatui::Frame<'_>, area: Rect) {
    if area.width < 20 || area.height < 5 {
        return;
    }
    let u = ui();
    let bg = Style::default().fg(u.dialog_fg).bg(u.dialog_bg);
    // Size from actual content. The hourglass glyph is wide in most
    // fonts, so use `UnicodeWidthStr` to measure display cells rather
    // than chars; char-count under-sized the popup and clipped the
    // trailing "(Esc to cancel)" hint.
    use unicode_width::UnicodeWidthStr;
    let label = format!("{} Loading hover…", spinner_frame());
    let hint = "  (Esc to cancel)";
    let content_w = (label.width() + hint.width()) as u16;
    let popup_w = (content_w + 4).min(area.width.saturating_sub(4));
    let popup_h = 3u16.min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w,
        height: popup_h,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);
    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: 1,
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(label, bg.add_modifier(Modifier::BOLD)),
            Span::styled(hint, bg),
        ]))
        .style(bg),
        inner,
    );
}

/// Plain-text (or lightly-markdown) hover payload. Same borderless
/// dialog_bg chrome as the table form; supports j/k scroll via the
/// passed-in `scroll` offset.
fn draw_hover_popup(f: &mut ratatui::Frame<'_>, area: Rect, scroll: usize, text: &str) {
    if area.width < 10 || area.height < 5 {
        return;
    }
    let u = ui();
    let bg = Style::default().fg(u.dialog_fg).bg(u.dialog_bg);
    let lines = format_hover_lines(text);
    let longest = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>() as u16
        })
        .max()
        .unwrap_or(0);
    let popup_w = (longest + 4).clamp(30, area.width.saturating_sub(4).min(80));
    let content_h = (lines.len() as u16).min(area.height.saturating_sub(4));
    // popup = content + 2 rows of vertical padding.
    let popup_h = (content_h + 2).min(area.height.saturating_sub(2));

    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w,
        height: popup_h,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);
    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };
    // Chromeless — no title row, content takes the whole inner area.
    f.render_widget(
        Paragraph::new(lines)
            .style(bg)
            .scroll((scroll as u16, 0))
            .wrap(ratatui::widgets::Wrap { trim: false }),
        inner,
    );
}

/// Render an LSP hover payload (markdown or plain text) as styled
/// ratatui lines via pulldown-cmark. Handles headers, fenced code
/// blocks, inline code, bold, italic, and blockquotes; everything else
/// flattens to plain spans so the popup stays readable for arbitrary
/// server output.
fn format_hover_lines(text: &str) -> Vec<Line<'static>> {
    use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
    let u = ui();
    let header1 = Style::default()
        .fg(u.dialog_border)
        .add_modifier(Modifier::BOLD);
    let header2 = Style::default()
        .fg(u.sql_keyword)
        .add_modifier(Modifier::BOLD);
    let code_style = Style::default().fg(u.sql_ident);
    let bold_style = Style::default().add_modifier(Modifier::BOLD);
    let italic_style = Style::default().add_modifier(Modifier::ITALIC);
    let quote_style = Style::default()
        .fg(u.sql_comment)
        .add_modifier(Modifier::ITALIC);

    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(text, opts);

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    // Active style overlay stack. Headings / blockquotes push a base
    // style that applies to every text span emitted until the matching
    // End event pops it; `bold` / `emph` / `code` layer on top.
    let mut base_stack: Vec<Style> = Vec::new();
    let mut span_stack: Vec<Style> = Vec::new();
    let mut in_code_block = false;

    let flush_line = |current: &mut Vec<Span<'static>>, out: &mut Vec<Line<'static>>| {
        out.push(Line::from(std::mem::take(current)));
    };
    let active_style = |base_stack: &[Style], span_stack: &[Style]| -> Style {
        // Right-most wins so inner emphasis overrides outer heading
        // base. Patch each layer so partial styles (fg only, modifier
        // only) compose instead of overwriting.
        let mut acc = Style::default();
        for s in base_stack {
            acc = acc.patch(*s);
        }
        for s in span_stack {
            acc = acc.patch(*s);
        }
        acc
    };
    let push_text = |text: &str,
                     style: Style,
                     current: &mut Vec<Span<'static>>,
                     out: &mut Vec<Line<'static>>| {
        // Newlines inside a text event (rare) still need to split the
        // current line so wrapping works.
        let mut parts = text.split('\n').peekable();
        while let Some(chunk) = parts.next() {
            if !chunk.is_empty() {
                current.push(Span::styled(chunk.to_string(), style));
            }
            if parts.peek().is_some() {
                flush_line(current, out);
            }
        }
    };

    for event in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                base_stack.push(match level {
                    HeadingLevel::H1 => header1,
                    _ => header2,
                });
            }
            Event::End(TagEnd::Heading(_)) => {
                base_stack.pop();
                flush_line(&mut current, &mut out);
            }
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                flush_line(&mut current, &mut out);
                out.push(Line::from(""));
            }
            Event::Start(Tag::BlockQuote(_)) => base_stack.push(quote_style),
            Event::End(TagEnd::BlockQuote) => {
                base_stack.pop();
                flush_line(&mut current, &mut out);
            }
            Event::Start(Tag::CodeBlock(_)) => {
                in_code_block = true;
                base_stack.push(code_style);
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                base_stack.pop();
            }
            Event::Start(Tag::Emphasis) => span_stack.push(italic_style),
            Event::End(TagEnd::Emphasis) => {
                span_stack.pop();
            }
            Event::Start(Tag::Strong) => span_stack.push(bold_style),
            Event::End(TagEnd::Strong) => {
                span_stack.pop();
            }
            Event::Text(t) => {
                let st = active_style(&base_stack, &span_stack);
                if in_code_block {
                    // Code blocks emit a single Text with embedded
                    // newlines; split them so each source line becomes
                    // its own visual row.
                    for (i, chunk) in t.split('\n').enumerate() {
                        if i > 0 {
                            flush_line(&mut current, &mut out);
                        }
                        if !chunk.is_empty() {
                            current.push(Span::styled(chunk.to_string(), st));
                        }
                    }
                } else {
                    push_text(&t, st, &mut current, &mut out);
                }
            }
            Event::Code(t) => {
                current.push(Span::styled(t.into_string(), code_style));
            }
            Event::SoftBreak => current.push(Span::raw(" ")),
            Event::HardBreak => flush_line(&mut current, &mut out),
            Event::Rule => {
                flush_line(&mut current, &mut out);
                out.push(Line::from(Span::styled(
                    "─".repeat(40),
                    Style::default().fg(u.results_sep),
                )));
            }
            _ => {}
        }
    }
    if !current.is_empty() {
        flush_line(&mut current, &mut out);
    }
    // Strip trailing blank lines so the popup sizes tightly.
    while out
        .last()
        .is_some_and(|l| l.spans.iter().all(|s| s.content.is_empty()))
    {
        out.pop();
    }
    if out.is_empty() {
        out.push(Line::from(""));
    }
    out
}

fn draw_completions(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    editor_area: Rect,
    cur_row: usize,
    cur_col: usize,
) {
    let cursor = state.completion_cursor;
    let items: Vec<ListItem> = state
        .completions
        .iter()
        .map(|s| ListItem::new(s.as_str()))
        .collect();

    let longest = state
        .completions
        .iter()
        .map(|s| s.chars().count() as u16)
        .max()
        .unwrap_or(0);
    let popup_w = (longest + 2)
        .clamp(20, 60)
        .min(editor_area.width.saturating_sub(2));
    let popup_h = (items.len() as u16 + 2).min(12);

    // inner editor area starts 1 cell in from the block border
    let inner_x = editor_area.x + 1;
    let inner_y = editor_area.y + 1;
    let inner_w = editor_area.width.saturating_sub(2);
    let inner_h = editor_area.height.saturating_sub(2);

    // cursor position in screen coords (row 0 = first visible line)
    let cx = inner_x.saturating_add(cur_col as u16);
    let cy = inner_y.saturating_add(cur_row as u16);

    // place popup one row below cursor; flip up if it would overflow bottom
    let popup_y = if cy + 2 + popup_h <= inner_y + inner_h {
        cy + 2
    } else {
        cy.saturating_sub(popup_h)
    };
    // clamp x so popup stays inside the editor
    let popup_x = cx.min((inner_x + inner_w).saturating_sub(popup_w));

    let popup = Rect {
        x: popup_x,
        y: popup_y,
        width: popup_w,
        height: popup_h.min(inner_h),
    };

    // Borderless — match command palette / hover / help styling. A bg
    // fill + 1-col horizontal padding inside replaces the old border
    // frame so the overlay reads as one unified chrome.
    let bg = Style::default().fg(ui().dialog_fg).bg(ui().dialog_bg);
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);
    let list = List::new(items)
        .style(bg)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let inner = Rect {
        x: popup.x + 1,
        y: popup.y,
        width: popup.width.saturating_sub(2),
        height: popup.height,
    };
    let mut list_state = ListState::default().with_selected(Some(cursor));
    f.render_stateful_widget(list, inner, &mut list_state);
}

/// Small borderless centered dialog: 2-col horizontal padding, 1-row vertical
/// padding, single line of input. Used by the command palette and rename.
/// Borderless centered single-line input dialog. The caller supplies the
/// prompt prefix (e.g. `> `, `: `) so cursor placement stays exact regardless
/// of glyph width. Returns the terminal-space cursor position so the caller
/// can place the real cursor.
fn draw_input_dialog(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    prefix: &str,
    input: &TextInput,
) -> (u16, u16) {
    let bg = Style::default().fg(ui().dialog_fg).bg(ui().dialog_bg);
    let content = format!("{prefix}{}", input.text);
    let inner_w = (content.chars().count() as u16 + 1).max(20);
    let width = (inner_w + 4).min(area.width.saturating_sub(4));
    let height = 3u16.min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);
    let line = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: 1,
    };
    f.render_widget(Paragraph::new(content).style(bg), line);
    (
        line.x + prefix.chars().count() as u16 + input.cursor as u16,
        line.y,
    )
}

/// Borderless centered confirmation dialog with a single message line.
fn draw_confirm_dialog(f: &mut ratatui::Frame<'_>, area: Rect, message: &str) {
    let bg = Style::default()
        .fg(ui().dialog_error_fg)
        .bg(ui().dialog_error_bg);
    let inner_w = (message.chars().count() as u16).max(20);
    let width = (inner_w + 4).min(area.width.saturating_sub(4));
    let height = 3u16.min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);
    let line = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: 1,
    };
    f.render_widget(Paragraph::new(message.to_string()).style(bg), line);
}

/// File picker dialog: borderless, padded, with input row + matching tab names.
fn draw_file_picker(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    picker: &FilePicker,
    matched: &[&String],
    active_name: Option<&str>,
) -> (u16, u16) {
    let bg = Style::default().fg(ui().dialog_fg).bg(ui().dialog_bg);
    let width = 60u16.min(area.width.saturating_sub(4));
    let max_rows = 12u16;
    let list_rows = (matched.len() as u16).min(max_rows).max(1);
    // 1 row top pad + 1 row input + 1 row separator + N rows + 1 row bottom pad
    let height = (list_rows + 4).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);

    let inner_x = popup.x + 2;
    let inner_w = popup.width.saturating_sub(4);

    // Input row
    let input_area = Rect {
        x: inner_x,
        y: popup.y + 1,
        width: inner_w,
        height: 1,
    };
    f.render_widget(
        Paragraph::new(format!("> {}", picker.query.text)).style(bg),
        input_area,
    );
    let cursor_pos = (input_area.x + 2 + picker.query.cursor as u16, input_area.y);

    // Results list
    let list_y = popup.y + 3;
    let cursor = picker.cursor.min(matched.len().saturating_sub(1));
    for (i, name) in matched.iter().take(list_rows as usize).enumerate() {
        let row = Rect {
            x: inner_x,
            y: list_y + i as u16,
            width: inner_w,
            height: 1,
        };
        let is_cursor = i == cursor;
        let is_active = active_name == Some(name.as_str());
        let mut style = bg;
        if is_cursor {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let marker = if is_active { "* " } else { "  " };
        f.render_widget(Paragraph::new(format!("{marker}{name}")).style(style), row);
    }
    if matched.is_empty() {
        let row = Rect {
            x: inner_x,
            y: list_y,
            width: inner_w,
            height: 1,
        };
        f.render_widget(
            Paragraph::new("(no matches)").style(bg.add_modifier(Modifier::DIM)),
            row,
        );
    }
    cursor_pos
}

fn draw_connection_switcher(f: &mut ratatui::Frame<'_>, state: &AppState, area: Rect) {
    let bg = Style::default().fg(ui().dialog_fg).bg(ui().dialog_bg);
    let conns = &state.available_connections;
    let cursor = state.connection_switcher_cursor;
    let active_name = state.active_connection.as_deref();

    let width = 60u16.min(area.width.saturating_sub(4));
    let max_rows = 12u16;
    let list_rows = (conns.len() as u16).min(max_rows).max(1);
    // 1 row top pad + 1 row header + 1 row separator + N rows + 1 row bottom pad
    let height = (list_rows + 4).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);

    let inner_x = popup.x + 2;
    let inner_w = popup.width.saturating_sub(4);

    // Header row
    let header = Rect {
        x: inner_x,
        y: popup.y + 1,
        width: inner_w,
        height: 1,
    };
    f.render_widget(
        Paragraph::new("Connections  (Enter connect · n new · e edit · d delete)")
            .style(bg.add_modifier(Modifier::DIM)),
        header,
    );

    // List
    let list_y = popup.y + 3;
    if conns.is_empty() {
        let row = Rect {
            x: inner_x,
            y: list_y,
            width: inner_w,
            height: 1,
        };
        f.render_widget(
            Paragraph::new("(no connections configured)").style(bg.add_modifier(Modifier::DIM)),
            row,
        );
        return;
    }
    let cur = cursor.min(conns.len().saturating_sub(1));
    for (i, c) in conns.iter().take(list_rows as usize).enumerate() {
        let row = Rect {
            x: inner_x,
            y: list_y + i as u16,
            width: inner_w,
            height: 1,
        };
        let is_cursor = i == cur;
        let is_active = active_name == Some(c.name.as_str());
        let mut style = bg;
        if is_cursor {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let marker = if is_active { "* " } else { "  " };
        f.render_widget(
            Paragraph::new(format!("{marker}{} — {}", c.name, c.url)).style(style),
            row,
        );
    }
}

fn draw_help(f: &mut ratatui::Frame<'_>, area: Rect, scroll: u16) -> u16 {
    const SECTIONS: &[(&str, &[(&str, &str)])] = &[
        (
            "Global",
            &[
                ("?", "Open this help (normal mode)"),
                ("Ctrl+Enter", "Run statement under cursor"),
                ("Ctrl+Shift+Enter", "Run all statements in file"),
                (":q", "Quit"),
                ("Esc Esc", "Dismiss all toasts"),
            ],
        ),
        (
            "Leader (default Space — config: editor.leader_key)",
            &[
                ("<leader> c", "Connection switcher"),
                ("<leader> n", "New scratch tab"),
                ("<leader> r", "Rename current tab"),
                ("<leader> d", "Delete current tab (confirm)"),
                ("<leader> <leader>", "Fuzzy file picker"),
            ],
        ),
        (
            "Pane Focus",
            &[
                ("Ctrl+H / click", "Focus schema"),
                ("Ctrl+L / click", "Focus editor"),
                ("Ctrl+J / click", "Focus results"),
                ("Ctrl+K / click", "Focus editor"),
            ],
        ),
        (
            "Explorer Pane",
            &[
                ("j / k", "Navigate up / down"),
                ("Enter / l", "Expand / collapse node"),
            ],
        ),
        (
            "Results Pane",
            &[
                ("j / k", "Down / up (count prefix)"),
                ("h / l", "Left / right (count prefix)"),
                ("gg / G", "First / last row"),
                ("0 / $", "First / last column"),
                ("/", "Search cells"),
                ("n / N", "Next / previous match"),
                ("H / L", "Prev / next result tab"),
                ("V", "Visual-line select rows"),
                ("v / Ctrl+V", "Visual-block select rectangle"),
                ("y", "Yank selection / row"),
                ("Esc", "Clear selection"),
                ("Left click", "Copy column value"),
                ("Right click", "Copy full row"),
                ("Left click (error)", "Copy query or error text"),
            ],
        ),
        (
            "Tabs",
            &[
                ("<leader>n", "New scratch tab"),
                ("Shift+L", "Next tab"),
                ("Shift+H", "Prev tab"),
                ("<leader>r", "Rename current tab"),
                ("<leader>d", "Delete current tab"),
                ("<leader><leader>", "Fuzzy switch tab"),
                ("Click tab name", "Switch to tab"),
            ],
        ),
        (
            "Editor — Vim",
            &[
                ("i", "Insert mode"),
                ("Esc", "Normal mode"),
                ("v", "Visual mode"),
                ("K", "LSP hover under cursor"),
                ("Ctrl+P / Ctrl+N", "History prev / next"),
            ],
        ),
        (
            "Connection Switcher",
            &[
                ("j / k", "Navigate"),
                ("Enter", "Connect"),
                ("n", "New connection"),
                ("e", "Edit connection"),
                ("d", "Delete connection"),
                ("Esc", "Close"),
            ],
        ),
        (
            "Add / Edit Connection",
            &[
                ("Tab", "Switch Name / URL field"),
                ("Enter", "Save"),
                ("Esc", "Cancel"),
            ],
        ),
    ];

    let u = ui();
    let bg = Style::default().fg(u.dialog_fg).bg(u.dialog_bg);
    // Borderless dialog styling: same 2-col / 1-row padding idiom as
    // the command palette and hover popups. Two extra rows reserved
    // for the title + trailing pad, so content height = total_rows.
    let width = 62.min(area.width.saturating_sub(4));
    let total_rows: u16 = SECTIONS
        .iter()
        .map(|(_, items)| items.len() as u16 + 2)
        .sum::<u16>()
        + 1;
    let height = (total_rows + 4).min(area.height.saturating_sub(4));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(Clear, popup);
    f.render_widget(Block::default().style(bg), popup);

    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    let title = Paragraph::new(Line::from(Span::styled(
        " Help",
        bg.add_modifier(Modifier::BOLD),
    )))
    .style(bg);
    f.render_widget(title, chunks[0]);

    let max_scroll = total_rows.saturating_sub(chunks[1].height);
    let scroll = scroll.min(max_scroll);

    let mut lines: Vec<ratatui::text::Line<'static>> = vec![];
    for (section, items) in SECTIONS {
        lines.push(ratatui::text::Line::from(Span::styled(
            format!(" {section}"),
            bg.add_modifier(Modifier::BOLD),
        )));
        for (key, desc) in *items {
            let pad = 20usize.saturating_sub(key.len());
            lines.push(ratatui::text::Line::from(vec![
                Span::styled(
                    format!("  {key}"),
                    Style::default().fg(u.completion_key).bg(u.dialog_bg),
                ),
                Span::styled(" ".repeat(pad), bg),
                Span::styled((*desc).to_string(), bg),
            ]));
        }
        lines.push(ratatui::text::Line::raw(""));
    }

    f.render_widget(
        Paragraph::new(lines).style(bg).scroll((scroll, 0)),
        chunks[1],
    );
    max_scroll
}

fn draw_add_connection(f: &mut ratatui::Frame<'_>, state: &AppState, area: Rect) -> (u16, u16) {
    let width = 64.min(area.width.saturating_sub(4));
    let height = 7;
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(Clear, popup);

    let block = Block::default()
        .title(if state.edit_connection_original_name.is_some() {
            " Edit Connection (Tab switch, Enter save, Esc cancel) "
        } else {
            " Add Connection (Tab switch, Enter save, Esc cancel) "
        })
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ui().confirm_border));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let name_style = if state.add_connection_field == AddConnectionField::Name {
        Style::default()
            .fg(ui().completion_key)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let url_style = if state.add_connection_field == AddConnectionField::Url {
        Style::default()
            .fg(ui().completion_key)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    let name_label = "Name > ";
    let url_label = "URL  > ";
    f.render_widget(
        Paragraph::new(format!("{name_label}{}", state.add_connection_name)).style(name_style),
        rows[0],
    );
    f.render_widget(
        Paragraph::new(format!("{url_label}{}", state.add_connection_url)).style(url_style),
        rows[1],
    );
    f.render_widget(
        Paragraph::new("Only letters, digits, - and _ allowed in name")
            .style(Style::default().fg(ui().editor_line_num)),
        rows[2],
    );
    match state.add_connection_field {
        AddConnectionField::Name => (
            rows[0].x + name_label.chars().count() as u16 + state.add_connection_name_cursor as u16,
            rows[0].y,
        ),
        AddConnectionField::Url => (
            rows[1].x + url_label.chars().count() as u16 + state.add_connection_url_cursor as u16,
            rows[1].y,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{CommentBody, comment_body_from_line, format_hover_lines};
    use ratatui::style::Modifier;
    use sqeel_core::{
        AppState,
        state::{Focus, QueryResult},
    };

    #[test]
    fn layout_ratio_default() {
        let state = AppState::new();
        let s = state.lock().unwrap();
        assert_eq!(s.editor_ratio, 1.0);
    }

    #[test]
    fn layout_ratio_with_results() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_results(QueryResult {
            columns: vec!["col".into()],
            rows: vec![vec!["val".into()]],
            col_widths: vec![],
        });
        assert_eq!(s.editor_ratio, 0.5);
    }

    #[test]
    fn focus_transitions() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.focus = Focus::Schema;
        assert_eq!(s.focus, Focus::Schema);
        s.focus = Focus::Results;
        assert_eq!(s.focus, Focus::Results);
        s.focus = Focus::Editor;
        assert_eq!(s.focus, Focus::Editor);
    }

    #[test]
    fn completions_set_and_dismiss() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_completions(vec!["SELECT".into(), "FROM".into()]);
        assert!(s.show_completions);
        assert_eq!(s.completions.len(), 2);
        s.dismiss_completions();
        assert!(!s.show_completions);
    }

    #[test]
    fn diagnostics_stored() {
        use lsp_types::DiagnosticSeverity;
        use sqeel_core::lsp::Diagnostic;
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_diagnostics(vec![Diagnostic {
            line: 0,
            col: 5,
            end_line: 0,
            end_col: 10,
            message: "unexpected token".into(),
            severity: DiagnosticSeverity::ERROR,
        }]);
        assert!(s.has_errors());
    }

    #[test]
    fn connection_switcher_open_close() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        assert!(!s.show_connection_switcher);
        s.open_connection_switcher();
        assert!(s.show_connection_switcher);
        s.close_connection_switcher();
        assert!(!s.show_connection_switcher);
    }

    #[test]
    fn connection_switcher_navigation() {
        use sqeel_core::config::ConnectionConfig;
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_available_connections(vec![
            ConnectionConfig {
                name: "local".into(),
                url: "mysql://localhost/mydb".into(),
            },
            ConnectionConfig {
                name: "staging".into(),
                url: "mysql://staging/mydb".into(),
            },
        ]);
        s.open_connection_switcher();
        assert_eq!(s.connection_switcher_cursor, 0);
        s.switcher_down();
        assert_eq!(s.connection_switcher_cursor, 1);
        // Cannot go past last
        s.switcher_down();
        assert_eq!(s.connection_switcher_cursor, 1);
        s.switcher_up();
        assert_eq!(s.connection_switcher_cursor, 0);
    }

    #[test]
    fn connection_switcher_confirm_sets_pending() {
        use sqeel_core::config::ConnectionConfig;
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_available_connections(vec![ConnectionConfig {
            name: "local".into(),
            url: "mysql://localhost/mydb".into(),
        }]);
        s.open_connection_switcher();
        let url = s.confirm_connection_switch();
        assert_eq!(url, Some("mysql://localhost/mydb".into()));
        assert_eq!(s.pending_reconnect, Some("mysql://localhost/mydb".into()));
        assert!(!s.show_connection_switcher);
    }

    #[test]
    fn comment_markers_detected_inside_comment() {
        let line = "SELECT 1; -- TODO: backfill nulls";
        let ranges: Vec<(usize, usize)> = super::find_comment_markers(line)
            .into_iter()
            .map(|(s, e, _)| (s, e))
            .collect();
        // Label body (space + TODO) = 12..17, trailing `:` = 17..18,
        // tail (rest of line) = 18..33.
        assert_eq!(ranges, vec![(12, 17), (17, 18), (18, 33)]);
    }

    #[test]
    fn comment_markers_without_colon_still_get_label_and_trailing_space() {
        let line = "-- TODO backfill";
        let ranges: Vec<(usize, usize)> = super::find_comment_markers(line)
            .into_iter()
            .map(|(s, e, _)| (s, e))
            .collect();
        // " TODO" label at 2..7, trailing space 7..8, tail 8..16.
        assert_eq!(ranges, vec![(2, 7), (7, 8), (8, 16)]);
    }

    #[test]
    fn comment_markers_skip_lines_without_dashdash() {
        assert!(super::find_comment_markers("TODO: not in a comment").is_empty());
    }

    #[test]
    fn comment_markers_respect_word_boundary_both_sides() {
        assert!(super::find_comment_markers("-- XTODO: nope").is_empty());
        assert!(super::find_comment_markers("-- TODOS nope").is_empty());
    }

    #[test]
    fn comment_markers_multiple_on_one_line() {
        let line = "-- FIX: bar WARN: baz";
        let ranges: Vec<(usize, usize)> = super::find_comment_markers(line)
            .into_iter()
            .map(|(s, e, _)| (s, e))
            .collect();
        assert!(ranges.contains(&(2, 6)));
        assert!(ranges.contains(&(6, 7)));
        assert!(ranges.contains(&(11, 16)));
        assert!(ranges.contains(&(16, 17)));
    }

    #[test]
    fn comment_marker_colon_always_blends_into_badge() {
        let line = "-- TODO: x";
        let spans = super::find_comment_markers(line);
        let colon = spans.iter().find(|(s, e, _)| *s == 7 && *e == 8).unwrap();
        let u = super::theme::ui();
        assert_eq!(colon.2.fg, Some(u.sql_marker_todo));
        assert_eq!(colon.2.bg, Some(u.sql_marker_todo));
    }

    #[test]
    fn marker_word_invisible_on_cursor_line() {
        use ratatui::style::Style;
        let mut row: Vec<(usize, usize, Style)> = Vec::new();
        let line = "-- TODO: x";
        let comments: Vec<CommentBody> = comment_body_from_line(line).into_iter().collect();
        super::apply_marker_overlay(&mut row, line, &comments, None, true);
        let u = super::theme::ui();
        let label = row.iter().find(|(s, e, _)| *s == 2 && *e == 7).unwrap();
        assert_eq!(label.2.fg, Some(u.sql_marker_todo));
        assert_eq!(label.2.bg, Some(u.sql_marker_todo));
    }

    #[test]
    fn marker_word_visible_off_cursor_line() {
        use ratatui::style::Style;
        let mut row: Vec<(usize, usize, Style)> = Vec::new();
        let line = "-- TODO: x";
        let comments: Vec<CommentBody> = comment_body_from_line(line).into_iter().collect();
        super::apply_marker_overlay(&mut row, line, &comments, None, false);
        let u = super::theme::ui();
        let label = row.iter().find(|(s, e, _)| *s == 2 && *e == 7).unwrap();
        assert_eq!(label.2.fg, Some(u.sql_marker_fg));
        assert_eq!(label.2.bg, Some(u.sql_marker_todo));
    }

    #[test]
    fn continuation_comment_inherits_active_color() {
        use ratatui::style::Style;
        let mut row: Vec<(usize, usize, Style)> = Vec::new();
        let line = "-- this is a warning";
        let comments: Vec<CommentBody> = comment_body_from_line(line).into_iter().collect();
        let u = super::theme::ui();
        let new =
            super::apply_marker_overlay(&mut row, line, &comments, Some(u.sql_marker_warn), false);
        assert_eq!(new, Some(u.sql_marker_warn));
        let tinted = row
            .iter()
            .find(|(_, _, st)| st.fg == Some(u.sql_marker_warn))
            .unwrap();
        // Tint starts *after* the `--` sigil.
        assert_eq!(tinted.0, 2);
        assert_eq!(tinted.1, "-- this is a warning".len());
    }

    #[test]
    fn non_comment_line_resets_inherited_color() {
        use ratatui::style::Style;
        let mut row: Vec<(usize, usize, Style)> = Vec::new();
        let line = "SELECT 1;";
        let comments: Vec<CommentBody> = comment_body_from_line(line).into_iter().collect();
        let u = super::theme::ui();
        let new =
            super::apply_marker_overlay(&mut row, line, &comments, Some(u.sql_marker_warn), false);
        assert_eq!(new, None);
        assert!(row.is_empty());
    }

    #[test]
    fn comment_body_from_span_skips_line_delim() {
        let line = "SELECT 1; -- FIX: x";
        let body = super::comment_body_from_span(line, 10, line.len());
        // `--` at 10..12 skipped; body covers everything after it.
        assert_eq!(body.start, 12);
        assert_eq!(body.end, line.len());
    }

    #[test]
    fn comment_body_from_span_skips_block_delim() {
        let line = "/* FIX: x */ more";
        // Block comment span is only `/* FIX: x */` → end before ` more`.
        let body = super::comment_body_from_span(line, 0, 12);
        assert_eq!(body.start, 2);
        assert_eq!(body.end, 12);
    }

    #[test]
    fn comment_body_from_span_continuation_row_has_no_delim() {
        // Middle row of a multi-line /* … */ block comment — the span
        // starts at col 0 on a line that begins with comment content
        // (no delimiter), so we must not skip 2 bytes.
        let line = "continuation";
        let body = super::comment_body_from_span(line, 0, line.len());
        assert_eq!(body.start, 0);
        assert_eq!(body.end, line.len());
    }

    #[test]
    fn marker_in_string_literal_not_highlighted_without_comment_body() {
        use ratatui::style::Style;
        // Caller passes an empty comments slice — emulates tree-sitter
        // reporting no comment on this row, e.g. because the `--` is
        // inside a string literal.
        let mut row: Vec<(usize, usize, Style)> = Vec::new();
        let new =
            super::apply_marker_overlay(&mut row, "SELECT '-- FIXME: x' FROM t", &[], None, false);
        assert_eq!(new, None);
        assert!(row.is_empty());
    }

    #[test]
    fn marker_tail_stops_at_block_comment_end() {
        use ratatui::style::Style;
        let line = "/* FIX: hi */ SELECT 1";
        let body = super::comment_body_from_span(line, 0, 12);
        let mut row: Vec<(usize, usize, Style)> = Vec::new();
        super::apply_marker_overlay(&mut row, line, &[body], None, false);
        // Every emitted span must end at or before the `*/` (byte 12).
        for (s, e, _) in &row {
            assert!(*e <= 12, "span {s}..{e} bled past `*/`");
        }
    }

    #[test]
    fn text_input_insert_str_inserts_at_caret_and_advances() {
        let mut t = super::TextInput::from_str("ab");
        t.insert_str("XYZ");
        assert_eq!(t.text, "abXYZ");
        assert_eq!(t.cursor, 5);
        t.left();
        t.left();
        t.insert_str("--");
        assert_eq!(t.text, "abX--YZ");
        assert_eq!(t.cursor, 5);
    }

    #[test]
    fn text_input_insert_str_handles_multibyte() {
        let mut t = super::TextInput::from_str("á");
        t.insert_str("ñ");
        assert_eq!(t.text, "áñ");
        assert_eq!(t.cursor, 2);
    }

    #[test]
    fn should_resubmit_triggers_on_dialect_flip() {
        use sqeel_core::highlight::Dialect;
        // Steady state: no content change, no scroll, no dialect change.
        assert!(!super::should_resubmit_highlight(
            false,
            false,
            Dialect::Generic,
            Dialect::Generic
        ));
        // Dialect changes (e.g. async DB handshake completes) → force
        // re-parse even when content is idle.
        assert!(super::should_resubmit_highlight(
            false,
            false,
            Dialect::MySql,
            Dialect::Generic
        ));
        // Content change fires regardless of dialect match.
        assert!(super::should_resubmit_highlight(
            true,
            false,
            Dialect::Generic,
            Dialect::Generic
        ));
        // Viewport scroll fires regardless of dialect match.
        assert!(super::should_resubmit_highlight(
            false,
            true,
            Dialect::Generic,
            Dialect::Generic
        ));
    }

    #[test]
    fn diagnostic_underline_marks_range_with_severity_color() {
        use ratatui::style::{Color, Modifier, Style};
        use sqeel_core::lsp::Diagnostic;
        let _ = super::theme::load();

        let blue = Color::Rgb(10, 20, 30);
        let mut row: Vec<(usize, usize, Style)> = vec![(0, 10, Style::default().fg(blue))];
        let by_row = std::slice::from_mut(&mut row);
        let diag = Diagnostic {
            line: 0,
            col: 2,
            end_line: 0,
            end_col: 7,
            message: "nope".into(),
            severity: lsp_types::DiagnosticSeverity::ERROR,
        };
        let lines = vec!["SELECT * x;".to_string()];
        super::apply_diagnostic_underline(by_row, &diag, &lines, 1);

        let u = super::theme::ui();
        let overlap = row
            .iter()
            .find(|&&(s, e, _)| s == 2 && e == 7)
            .expect("overlap span missing");
        // fg flips to error colour so the range reads loud even in
        // terminals without colored-underline support.
        assert_eq!(overlap.2.fg, Some(u.status_diag_error));
        assert!(
            overlap.2.add_modifier.contains(Modifier::UNDERLINED),
            "overlap missing UNDERLINED modifier"
        );
        // Bytes outside the range keep their original fg.
        let left = row
            .iter()
            .find(|&&(s, e, _)| s == 0 && e == 2)
            .expect("left segment missing");
        assert_eq!(left.2.fg, Some(blue));
        let right = row
            .iter()
            .find(|&&(s, e, _)| s == 7 && e == 10)
            .expect("right segment missing");
        assert_eq!(right.2.fg, Some(blue));
    }

    #[test]
    fn diagnostic_underline_paints_gap_when_no_existing_spans() {
        use ratatui::style::{Modifier, Style};
        use sqeel_core::lsp::Diagnostic;
        let _ = super::theme::load();

        let mut row: Vec<(usize, usize, Style)> = Vec::new();
        let by_row = std::slice::from_mut(&mut row);
        let diag = Diagnostic {
            line: 0,
            col: 3,
            end_line: 0,
            end_col: 8,
            message: "nope".into(),
            severity: lsp_types::DiagnosticSeverity::ERROR,
        };
        let lines = vec!["some random text".to_string()];
        super::apply_diagnostic_underline(by_row, &diag, &lines, 1);

        let u = super::theme::ui();
        let span = row
            .iter()
            .find(|&&(s, e, _)| s == 3 && e == 8)
            .expect("bare diagnostic span missing");
        assert_eq!(span.2.fg, Some(u.status_diag_error));
        assert!(span.2.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn diagnostic_underline_zero_width_range_falls_back() {
        use ratatui::style::Style;
        use sqeel_core::lsp::Diagnostic;
        let _ = super::theme::load();

        let mut row: Vec<(usize, usize, Style)> = Vec::new();
        let by_row = std::slice::from_mut(&mut row);
        let diag = Diagnostic {
            line: 0,
            col: 5,
            end_line: 0,
            end_col: 5,
            message: "nope".into(),
            severity: lsp_types::DiagnosticSeverity::ERROR,
        };
        let lines = vec!["hello world".to_string()];
        super::apply_diagnostic_underline(by_row, &diag, &lines, 1);
        assert!(!row.is_empty(), "zero-width diag produced no spans");
    }

    #[test]
    fn overlay_splits_outer_span_around_marker() {
        use ratatui::style::Style;
        let base = Style::default();
        let marker = Style::default();
        let mut row = vec![(0usize, 30usize, base)];
        super::overlay_span(&mut row, 10, 15, marker);
        row.sort_by_key(|&(s, _, _)| s);
        let ranges: Vec<(usize, usize)> = row.iter().map(|&(s, e, _)| (s, e)).collect();
        assert_eq!(ranges, vec![(0, 10), (10, 15), (15, 30)]);
    }

    #[test]
    fn apply_window_spans_with_alter_tail_repro() {
        use super::HighlightResult;
        use super::theme;
        use sqeel_core::highlight::{Dialect, Highlighter};

        let header = "select * from ppc_third.searches_182 order by id desc;\n\
                   select * from ppc_third.searches_181 order by id desc;\n\
                   select count(*), status from ppc_third.searches_182 group by status;\n\
                   \n\
                   -- TODO: \n\
                   -- test\n\
                   \n\
                   -- TODO test\n\
                   \n\
                   -- TODO: this is a test\n\
                   -- FIXME: this is a test\n\
                   -- this is a test\n\
                   -- FIX:\n\
                   \n\
                   -- NOTE: another note\n\
                   -- WARN: woah...\n\
                   -- this is a warning\n\
                   -- INFO:  this is \n\
                   \n\
                   select * from users;\n\
                   \n\
                   DESC users;\n\
                   \n\
                   DESC users;\n\
                   \n";
        let alter = "-- ALTER TABLE ppc_third.`searches_182` ADD COLUMN `error` TEXT NULL AFTER `status`;\n";
        let mut src = header.to_string();
        for _ in 0..40 {
            src.push_str(alter);
        }
        let _ = theme::load();

        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight(&src, Dialect::MySql);
        let lines: Vec<String> = src.lines().map(|l| l.to_string()).collect();
        let row_count = lines.len();
        let result = HighlightResult {
            spans,
            start_row: 0,
            row_count,
            parse_errors: Vec::new(),
        };

        let mut ta = tui_textarea::TextArea::new(lines);
        super::apply_window_spans(&mut ta, &result, row_count, 0, &[]);
        let by_row = ta.take_syntax_spans();

        let keyword_style =
            super::token_kind_style(sqeel_core::highlight::TokenKind::Keyword).unwrap();
        for row in [21usize, 23] {
            let spans = &by_row[row];
            let has_kw_at_zero = spans
                .iter()
                .any(|&(s, e, st)| s == 0 && e >= 4 && st == keyword_style);
            assert!(
                has_kw_at_zero,
                "row {row} missing Keyword span; row spans = {spans:?}"
            );
        }
    }

    #[test]
    fn apply_window_spans_keeps_both_desc_keyword_spans() {
        use super::HighlightResult;
        use super::theme;
        use sqeel_core::highlight::{Dialect, Highlighter};

        let src = "select * from ppc_third.searches_182 order by id desc;\n\
                   select * from ppc_third.searches_181 order by id desc;\n\
                   select count(*), status from ppc_third.searches_182 group by status;\n\
                   \n\
                   -- TODO: \n\
                   -- test\n\
                   \n\
                   -- TODO test\n\
                   \n\
                   -- TODO: this is a test\n\
                   -- FIXME: this is a test\n\
                   -- this is a test\n\
                   -- FIX:\n\
                   \n\
                   -- NOTE: another note\n\
                   -- WARN: woah...\n\
                   -- this is a warning\n\
                   -- INFO:  this is \n\
                   \n\
                   select * from users;\n\
                   \n\
                   DESC users;\n\
                   \n\
                   DESC users;\n";
        let _ = theme::load();

        let mut h = Highlighter::new().unwrap();
        let spans = h.highlight(src, Dialect::MySql);
        let lines: Vec<String> = src.lines().map(|l| l.to_string()).collect();
        let row_count = lines.len();
        let result = HighlightResult {
            spans,
            start_row: 0,
            row_count,
            parse_errors: Vec::new(),
        };

        let mut ta = tui_textarea::TextArea::new(lines);
        super::apply_window_spans(&mut ta, &result, row_count, 0, &[]);
        let by_row = ta.take_syntax_spans();

        // Row 21 and row 23 each hold `DESC users;`. Both should have
        // at least one span starting at col 0 with Keyword styling.
        let keyword_style =
            super::token_kind_style(sqeel_core::highlight::TokenKind::Keyword).unwrap();
        for row in [21usize, 23] {
            let spans = &by_row[row];
            let has_kw_at_zero = spans
                .iter()
                .any(|&(s, e, st)| s == 0 && e >= 4 && st == keyword_style);
            assert!(
                has_kw_at_zero,
                "row {row} missing Keyword span at col 0..4; row spans = {spans:?}"
            );
        }
    }

    #[test]
    fn hover_formatter_strips_h1_header_markers() {
        let lines = format_hover_lines("# schema.users\nbody");
        assert_eq!(lines[0].spans[0].content, "schema.users");
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(lines[1].spans[0].content, "body");
    }

    #[test]
    fn hover_formatter_strips_code_fence_markers() {
        let text = "before\n```sql\nSELECT 1\n```\nafter";
        let lines = format_hover_lines(text);
        let joined: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<&str>>()
                    .join("")
            })
            .collect();
        // Fence markers themselves must not appear in rendered lines.
        assert!(joined.iter().all(|l| !l.contains("```")));
        assert!(joined.iter().any(|l| l == "SELECT 1"));
        assert!(joined.first().is_some_and(|l| l == "before"));
        assert!(joined.last().is_some_and(|l| l == "after"));
    }

    #[test]
    fn hover_formatter_splits_inline_code() {
        let lines = format_hover_lines("text `code` more");
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.to_string())
            .collect();
        assert!(joined.contains("code"));
        // Inline code span carries the code style's fg — it must be
        // a distinct span, not concatenated with the surrounding text.
        let has_code_span = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content == "code" && s.style.fg.is_some());
        assert!(has_code_span);
    }

    #[test]
    fn hover_formatter_emits_bold_span() {
        let lines = format_hover_lines("a **bold** b");
        let bold_span = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content == "bold")
            .expect("bold span present");
        assert!(bold_span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn hover_formatter_preserves_unicode() {
        let lines = format_hover_lines("tablé");
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.to_string())
            .collect();
        assert!(joined.contains("tablé"));
    }
}
