use crate::completion_ctx::CompletionCtx;
use crate::config::ConnectionConfig;
use crate::ddl::DdlEffect;
use crate::highlight::{Dialect, HighlightSpan};
use crate::lsp::Diagnostic;
use crate::persistence;
use crate::schema::{
    SchemaNode, SchemaTreeItem, collect_expanded_paths, expand_path, find_cursor_by_path,
    flatten_all, flatten_tree, merge_expansion, path_to_string, restore_expanded_paths,
    toggle_node,
};
use lsp_types::DiagnosticSeverity;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

impl AppState {
    /// True iff a single query or batch is currently running against
    /// the DB. Used to gate the Ctrl-C cancel keybinding — outside a
    /// running query Ctrl-C falls through to other handlers.
    pub fn query_in_flight(&self) -> bool {
        self.batch_in_progress
            || self
                .result_tabs
                .iter()
                .any(|t| matches!(t.kind, ResultsPane::Loading))
    }

    /// Signal the executor to cancel the currently running query (or
    /// the rest of the running batch). Called by the TUI on Ctrl-C /
    /// `:cancel`. Safe to call when no query is in flight — the flag
    /// is cleared by the executor at the start of the next request.
    pub fn cancel_current_query(&self) {
        self.cancel_control.cancel();
    }
}

/// Per-query cancellation handle. Set once at startup + shared
/// between the TUI and the query executor. The TUI flips the flag on
/// user cancel (Ctrl-C / `:cancel`) and wakes every waiter; the
/// executor re-sets between requests so a cancel doesn't leak into
/// the next query.
#[derive(Default)]
pub struct CancelControl {
    flag: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

impl CancelControl {
    /// Flip the flag on + wake every current waiter. Safe to call
    /// while no waiters are listening; the flag stays set until the
    /// next [`reset`].
    pub fn cancel(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Clear the flag. The executor calls this at the start of each
    /// request so a stale cancel (pressed while idle) doesn't
    /// immediately abort the next query.
    pub fn reset(&self) {
        self.flag.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Future that resolves when [`cancel`] is called. Picks up a
    /// cancel that was signalled before the await started, so there's
    /// no lost-wake race when the executor enters the select.
    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            self.notify.notified().await;
            if self.is_cancelled() {
                return;
            }
        }
    }
}

/// A tab-content load that [`AppState::switch_to_tab`] deferred
/// because the tab's cached content wasn't in memory. The TUI drives
/// the actual `persistence::load_query` on a blocking task so slow
/// filesystems don't freeze the render loop, then ships the loaded
/// content back via [`AppState::apply_loaded_tab_content`].
#[derive(Debug, Clone)]
pub struct PendingTabLoad {
    pub tab_index: usize,
    pub name: String,
}

/// A staged write produced by [`AppState::prepare_save_active_tab`] /
/// [`AppState::prepare_save_all_dirty`]. The disk-write step is held
/// off so the caller can run [`commit`](PendingSave::commit) on a
/// blocking task and keep the event loop responsive during large
/// writes; once the commit succeeds the caller should invoke
/// [`AppState::mark_tab_saved`] with `tab_index` to clear the dirty
/// flag.
#[derive(Debug, Clone)]
pub struct PendingSave {
    pub name: String,
    pub content: String,
    pub tab_index: Option<usize>,
}

impl PendingSave {
    /// Write the staged content to disk. Pure I/O — safe to run on
    /// `tokio::task::spawn_blocking` so the TUI loop doesn't stall on
    /// multi-megabyte buffers over slow filesystems.
    pub fn commit(&self) -> std::io::Result<()> {
        persistence::save_query(&self.name, &self.content)
            .map_err(|e| std::io::Error::other(e.to_string()))
    }
}

/// One open editor tab. Content is lazily loaded and evicted after 5 min of inactivity.
#[derive(Debug, Clone)]
pub struct TabEntry {
    pub name: String,
    pub content: Option<String>,
    pub last_accessed: Option<Instant>,
    /// Last-known editor cursor `(row, col)` (0-based). Restored on tab switch
    /// and persisted in session.toml across restarts.
    pub cursor: Option<(usize, usize)>,
    /// True when the in-memory content differs from the on-disk file.
    pub dirty: bool,
}

impl TabEntry {
    fn new(name: String) -> Self {
        Self {
            name,
            content: None,
            last_accessed: None,
            cursor: None,
            dirty: false,
        }
    }
    fn open(name: String, content: String) -> Self {
        Self {
            name,
            content: Some(content),
            last_accessed: Some(Instant::now()),
            cursor: None,
            dirty: false,
        }
    }
}

// Re-exported from sqeel-vim so app code can keep `use sqeel_core::state::{KeybindingMode, VimMode}`.
pub use sqeel_vim::{KeybindingMode, VimMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AddConnectionField {
    #[default]
    Name,
    Url,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoverEdge {
    FirstRow,
    LastRow,
    RowStart,
    RowEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Focus {
    #[default]
    Editor,
    Schema,
    Results,
    /// LSP hover popup with a tabular payload. While focused the user
    /// can navigate cells, select, and yank — same idiom as the
    /// results pane. Esc returns to the previous focus.
    Hover,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    #[serde(skip)]
    pub col_widths: Vec<u16>,
}

impl QueryResult {
    pub fn compute_col_widths(&mut self) {
        self.col_widths = self
            .columns
            .iter()
            .enumerate()
            .map(|(i, col)| {
                let max_data = self
                    .rows
                    .iter()
                    .map(|row| row.get(i).map(|s| s.len()).unwrap_or(0))
                    .max()
                    .unwrap_or(0);
                (col.len().max(max_data) + 2) as u16
            })
            .collect();
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ResultsPane {
    #[default]
    Empty,
    Loading,
    Results(QueryResult),
    Error(String),
    Cancelled,
    /// Statement that doesn't return a result set — INSERT/UPDATE/
    /// DELETE/CREATE/DROP/etc. `verb` is the leading SQL keyword
    /// (uppercase) so the renderer can distinguish DML ("3 rows
    /// affected") from DDL ("OK"). `rows_affected` comes straight
    /// from sqlx; DDL typically reports 0 here, which we suppress
    /// in the render layer.
    NonQuery {
        verb: String,
        rows_affected: u64,
    },
}

/// One entry in the results pane's tab bar — the query that produced it and
/// either a result set, an error, or a loading placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultsTab {
    pub query: String,
    pub kind: ResultsPane,
    /// Per-tab vertical scroll position; preserved across tab switches.
    pub scroll: usize,
    /// Per-tab horizontal column scroll position.
    pub col_scroll: usize,
    /// On-disk filename under `~/.local/share/sqeel/results/<conn>/` once a
    /// successful result is persisted. `None` for error/loading/cancelled tabs
    /// or until the result is saved.
    pub saved_filename: Option<String>,
    /// Per-tab cursor position inside the results pane. Reset to `Query` on
    /// creation; survives scroll + tab switches so returning users land where
    /// they left off.
    pub cursor: ResultsCursor,
    /// Active visual selection anchor + mode in the body. `None` means no
    /// selection. Entered with `V` (line) or `Ctrl-V` (block) when the
    /// cursor is on a Header / Cell. Exits on `Esc`, `v`, `V`, `Ctrl-V`,
    /// or after a yank.
    pub selection: Option<ResultsSelection>,
}

/// A rectangular / line-wise selection in the results body. Both
/// `anchor` and `cursor` live in `(row, col)` coordinates; `row` is the
/// body row index (0 = first data row, no header). `mode` picks whether
/// the column range spans both corners (Block) or the full row (Line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResultsSelection {
    pub anchor: (usize, usize),
    pub mode: ResultsSelectionMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultsSelectionMode {
    /// `V` — full row, from first to last column.
    Line,
    /// `Ctrl-V` — rectangle between anchor and cursor.
    Block,
}

/// What the results-pane cursor currently highlights. Shared across all three
/// variants (Results/Error/Cancelled) so `y` can yank whatever is under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResultsCursor {
    #[default]
    Query,
    /// Column name in the header row (Results pane only).
    Header(usize),
    /// Data cell in the body (Results pane only).
    Cell { row: usize, col: usize },
    /// Numbered line of the message body (Error / Cancelled panes).
    MessageLine(usize),
}

/// Bump `col_scroll` so column `col` lies inside a viewport of width `width`
/// cells, given `col_widths` (plus the 1-cell `│` separator between columns).
/// Leaves `col_scroll` unchanged if already visible. Shared between results
/// pane and hover popup so both clamp identically.
pub(crate) fn scroll_cols_into_view_slice(
    col_widths: &[u16],
    col_scroll: &mut usize,
    col: usize,
    width: u16,
) {
    if col < *col_scroll {
        *col_scroll = col;
        return;
    }
    if width == 0 || col_widths.is_empty() {
        return;
    }
    // Shrink col_scroll until the cursor column's right edge fits in the
    // viewport. Each column contributes its width plus a 1-cell separator
    // (except after the final column, but over-counting by 1 is safe).
    loop {
        let used: u32 = col_widths
            .iter()
            .skip(*col_scroll)
            .take(col + 1 - *col_scroll)
            .map(|&w| w as u32 + 1)
            .sum();
        if used <= width as u32 || *col_scroll >= col {
            break;
        }
        *col_scroll += 1;
    }
}

/// A query request sent over the executor channel — single statement or batch.
#[derive(Debug, Clone)]
pub enum QueryRequest {
    /// Single query with the result tab index to update when done.
    Single(String, usize),
    /// Batch of queries with the starting result tab index to update sequentially.
    Batch(Vec<String>, usize),
}

/// Lazy-load request for the schema sidebar. Sent to the background loader
/// when the user expands a node we haven't fetched yet.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SchemaLoadRequest {
    /// Fetch the database list and merge it into the tree.
    Databases,
    /// Fetch the table list for `db` and merge it into that db's table vec.
    Tables { db: String },
    /// Fetch column metadata for `db.table`.
    Columns { db: String, table: String },
}

#[derive(Default)]
pub struct AppState {
    pub editor_content: Arc<String>,
    /// True once the TUI has pushed the live editor buffer into
    /// `editor_content`. Before this, `editor_content` holds the default empty
    /// string and must not be mistaken for user-authored content.
    pub editor_content_synced: bool,
    pub tabs: Vec<TabEntry>,
    pub active_tab: usize,
    /// Set when the active tab changes; TUI drains this to reload the editor.
    pub tab_content_pending: Option<String>,
    /// Set alongside `tab_content_pending` when switching tabs; TUI drains this
    /// to restore the editor cursor. `(row, col)` 0-based.
    pub tab_cursor_pending: Option<(usize, usize)>,
    pub keybinding_mode: KeybindingMode,
    pub vim_mode: VimMode,
    pub focus: Focus,
    pub result_tabs: Vec<ResultsTab>,
    pub active_result_tab: usize,
    /// Whether a run-all batch should stop on the first query error.
    pub stop_on_error: bool,
    /// Set while a run-all batch is in progress.
    pub batch_in_progress: bool,
    pub editor_ratio: f32,
    pub lsp_diagnostics: Vec<Diagnostic>,
    pub highlight_spans: Vec<HighlightSpan>,
    pub completions: Vec<String>,
    pub show_completions: bool,
    pub completion_cursor: usize,
    /// LSP hover payload for the `K` binding. `Some` while the popup
    /// is visible; cleared by Esc or any cursor-moving keystroke so the
    /// overlay doesn't drift away from the symbol it described.
    pub hover_text: Option<String>,
    /// Tabular payload parsed out of a markdown-table hover response.
    /// When `Some` the popup renders as a navigable grid (reuses the
    /// results-pane rendering + cursor idiom); otherwise the text form
    /// above is shown as styled markdown.
    pub hover_table: Option<QueryResult>,
    /// Cursor inside `hover_table`. Always a `Cell` when active — the
    /// popup never surfaces a `Query` / `Header` / `MessageLine`
    /// position, which keeps nav code simple.
    pub hover_cursor: ResultsCursor,
    /// Optional visual selection inside the hover grid. Mirrors the
    /// results-pane `ResultsSelection` so `V` / `v` / `Ctrl-V` / `y`
    /// behave identically.
    pub hover_selection: Option<ResultsSelection>,
    /// Row scroll offset for the hover grid (rows above this aren't
    /// rendered). Kept in sync with cursor row by `ensure_hover_visible`.
    pub hover_scroll: usize,
    /// Column scroll offset for the hover grid — same semantics as
    /// `results_col_scroll` on a result tab.
    pub hover_col_scroll: usize,
    /// Focus that was active when the hover grid opened. Esc restores
    /// it so the user lands back on whatever pane they were driving.
    pub hover_prev_focus: Option<Focus>,
    /// True while the hover popup is awaiting the LSP response. Lets
    /// the render path show a spinner instead of blank chrome when
    /// the server takes a beat (common on first request / cold sqls).
    pub hover_loading: bool,
    /// Schema lookup the K binding queued — `(db, table)`. Loop
    /// resolves it once the lazy column load reports back, then
    /// installs the cached table view so the popup transitions from
    /// loading → grid without an LSP round-trip.
    pub hover_pending_table: Option<(String, String)>,
    /// Height of the hover popup's body area (rows currently visible
    /// between the header separator and the popup's bottom padding).
    /// Published by the render path each frame so nav helpers can
    /// clamp the row scroll offset when the cursor leaves the window.
    pub hover_body_height: AtomicU16,
    /// Width of the hover popup's body area in terminal cells. Feeds
    /// the column-scroll clamp so `l` past the right edge advances
    /// `hover_col_scroll` instead of parking the cursor off-screen.
    pub hover_body_width: AtomicU16,
    /// Terminal-space (x, y) of the hover body's top-left cell.
    /// Published by the render path so the mouse click handler can
    /// translate a click inside the popup into a (row, col) on the
    /// underlying grid.
    pub hover_body_x: AtomicU16,
    pub hover_body_y: AtomicU16,
    pub active_connection: Option<String>,
    /// SQL dialect of the current connection. Drives per-dialect
    /// keyword highlighting; `Generic` before any connection opens.
    pub active_dialect: Dialect,
    /// Populated by [`switch_to_tab`] when the target tab's content
    /// isn't cached. The TUI runs the disk read off the render loop,
    /// then calls [`apply_loaded_tab_content`] so the editor swaps in
    /// the real content once the load finishes.
    pub pending_tab_load: Option<PendingTabLoad>,
    /// When a connection resolves we write a sqls config file from its
    /// URL and park the path here. The TUI main loop takes it, restarts
    /// the LSP with `--config=<path>`, so sqls can emit schema-aware
    /// diagnostics instead of running blind.
    pub pending_sqls_config: Option<std::path::PathBuf>,
    pub status_message: Option<String>,
    pub schema_nodes: Vec<SchemaNode>,
    pub schema_cursor: usize,
    pub schema_loading: bool,
    /// True while the async DB handshake is in flight. Distinct from
    /// `schema_loading` (which covers schema-node fetching after the
    /// connect succeeds) so the sidebar can render "Connecting…" vs
    /// "Loading…" separately.
    pub schema_connecting: bool,
    /// Last connection error message — set when `connect_and_spawn`
    /// fails so the sidebar can show "Connection failed" instead of a
    /// stuck "Loading…" placeholder. Sidebar shows the short form;
    /// the full message stays here for the details popup. Cleared on
    /// a successful connect or when the user switches connections.
    pub schema_connect_error: Option<String>,
    /// URL of the last failed connection. Stashed alongside
    /// `schema_connect_error` so `retry_connection` can re-issue
    /// the handshake without round-tripping through the switcher.
    pub schema_connect_url: Option<String>,
    /// Toggled on when the user presses Enter (or clicks) the
    /// "Connection failed" placeholder. The render layer paints a
    /// modal with the full `schema_connect_error` text; Esc closes.
    pub show_connect_error_popup: bool,
    /// Set by the executor when a query finishes; cleared by the run loop after redraw.
    pub results_dirty: bool,
    schema_items_cache: Vec<SchemaTreeItem>,
    all_schema_items_cache: Vec<SchemaTreeItem>,
    /// Sorted, deduplicated identifier names for completion. Rebuilt on schema changes
    /// so hot-path completion submissions only clone an `Arc`.
    schema_identifier_cache: Arc<Vec<String>>,
    /// Set by schema mutators that want to defer the O(N log N) cache rebuild.
    /// Consumers call `rebuild_schema_cache_if_dirty` once per tick.
    pub schema_cache_dirty: bool,
    /// True while a spawn_blocking rebuild is in flight. Guards the
    /// main loop from spawning duplicate rebuilds while one is running
    /// against a now-stale snapshot.
    pub schema_rebuild_in_flight: bool,
    pub query_history: Vec<String>,
    pub history_cursor: Option<usize>,
    // Connection switcher
    pub available_connections: Vec<ConnectionConfig>,
    pub show_connection_switcher: bool,
    pub connection_switcher_cursor: usize,
    pub pending_reconnect: Option<String>,
    /// Two-step delete arm: when `Some(name)`, the next `d`/Enter on
    /// the same selection commits the delete. Movement, Esc, or
    /// switching modals clears it. Drives the "Delete `{name}`? …"
    /// confirmation in the switcher.
    pub connection_delete_armed: Option<String>,
    // Add/edit connection dialog
    pub show_add_connection: bool,
    pub add_connection_name: String,
    pub add_connection_url: String,
    pub add_connection_field: AddConnectionField,
    /// Caret position (char index) within the active add-connection field.
    pub add_connection_name_cursor: usize,
    pub add_connection_url_cursor: usize,
    /// Original name when editing an existing connection (None when adding new).
    pub edit_connection_original_name: Option<String>,
    // Help overlay
    pub show_help: bool,
    pub help_scroll: u16,
    // Debug mode — enabled via --debug CLI flag
    pub debug_mode: bool,
    pub lsp_available: bool,
    pub lsp_binary: String,
    // Live query channel — set by the binary when connected
    pub query_tx: Option<tokio::sync::mpsc::Sender<QueryRequest>>,
    /// Shared cancel signal for the active single / batch query.
    /// The TUI flips it on user cancel; the executor aborts the
    /// current sqlx future via `tokio::select!` and skips any
    /// remaining queries in the batch.
    pub cancel_control: std::sync::Arc<CancelControl>,
    /// Lazy schema-load channel — set by the binary when connected. Toggling a
    /// sidebar node into an expanded state posts a request here so the loader
    /// fetches tables/columns on demand instead of eagerly up front.
    pub schema_load_tx: Option<tokio::sync::mpsc::UnboundedSender<SchemaLoadRequest>>,
    /// In-flight lazy schema loads. Bumped on send, decremented by the loader
    /// when each request finishes. Drives the sidebar spinner.
    pub schema_pending_loads: usize,
    /// Currently-in-flight load requests. Entries are added on send and
    /// removed when the loader calls `finish_schema_load(&req)`. Used to
    /// dedup bursts of requests (e.g. typing `db.` fires every keystroke)
    /// without blocking future refreshes once a load completes.
    pub schema_loads_inflight: HashSet<SchemaLoadRequest>,
    /// When the top-level database list was last loaded this session. `None`
    /// means not yet loaded. Used alongside `schema_ttl` to drive periodic
    /// refreshes of the sidebar root.
    pub databases_loaded_at: Option<Instant>,
    /// Cached schema TTL. `Duration::ZERO` disables automatic refreshes.
    pub schema_ttl: std::time::Duration,
    /// Schema sidebar search query — persisted to session so it survives app restart.
    pub schema_search_query: Option<String>,
    /// Last-rendered viewport size of the results body, written by the TUI on
    /// each draw so cursor-nav helpers can scroll the viewport to follow the
    /// cursor when it moves off-screen. Atomics so the draw path (which only
    /// has a shared ref) can update them without taking a mutable lock.
    pub results_body_rows: AtomicU16,
    pub results_body_width: AtomicU16,
    /// Terminal-space top-left of the results body area. Published on
    /// every draw so the mouse handler can translate clicks into
    /// grid (row, col) without re-computing layout.
    pub results_body_x: AtomicU16,
    pub results_body_y: AtomicU16,
    /// Visible row count of the schema list viewport, written by the TUI on
    /// every draw so cursor-nav helpers can scroll the viewport without needing
    /// the height plumbed through.
    pub schema_viewport_rows: AtomicU16,
    /// Top item index currently shown in the schema list. Decoupled from the
    /// cursor so the mouse wheel can scroll without dragging the selection.
    pub schema_scroll_offset: usize,
}

impl AppState {
    pub fn new() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            editor_ratio: 1.0,
            stop_on_error: true,
            ..Default::default()
        }))
    }

    pub fn apply_editor_config(&mut self, cfg: &crate::config::EditorConfig) {
        self.stop_on_error = cfg.stop_on_error;
        self.schema_ttl = std::time::Duration::from_secs(cfg.schema_ttl_secs);
    }

    fn rebuild_schema_cache(&mut self) {
        self.schema_items_cache = flatten_tree(&self.schema_nodes);
        self.all_schema_items_cache = flatten_all(&self.schema_nodes);
        let mut ids = Vec::new();
        let mut stack: Vec<&SchemaNode> = self.schema_nodes.iter().collect();
        while let Some(node) = stack.pop() {
            ids.push(node.name().to_owned());
            match node {
                SchemaNode::Database { tables, .. } => stack.extend(tables.iter()),
                SchemaNode::Table { columns, .. } => stack.extend(columns.iter()),
                SchemaNode::Column { .. } => {}
            }
        }
        ids.sort();
        ids.dedup();
        self.schema_identifier_cache = Arc::new(ids);
        self.schema_cache_dirty = false;
    }

    /// Mark caches stale without doing the work. The next
    /// `rebuild_schema_cache_if_dirty` call (typically once per TUI tick) coalesces
    /// many mutations into a single rebuild.
    fn mark_schema_cache_dirty(&mut self) {
        self.schema_cache_dirty = true;
    }

    pub fn rebuild_schema_cache_if_dirty(&mut self) {
        if self.schema_cache_dirty {
            self.rebuild_schema_cache();
        }
    }

    /// If a rebuild is needed and none is already in flight, mark the
    /// in-flight flag + clear the dirty flag (we're snapshotting NOW;
    /// any further mutations will re-dirty), and hand back a clone of
    /// `schema_nodes` for the caller to flatten off the render loop.
    /// Returns `None` otherwise so the caller skips spawning.
    pub fn schema_snapshot_for_rebuild(&mut self) -> Option<Vec<SchemaNode>> {
        if !self.schema_cache_dirty || self.schema_rebuild_in_flight {
            return None;
        }
        self.schema_rebuild_in_flight = true;
        self.schema_cache_dirty = false;
        Some(self.schema_nodes.clone())
    }

    /// Install the caches computed off-main-loop by
    /// [`schema_snapshot_for_rebuild`]'s consumer. Clears the
    /// in-flight flag so a subsequent mutation can kick another rebuild.
    pub fn apply_schema_cache_rebuild(
        &mut self,
        items: Vec<SchemaTreeItem>,
        all: Vec<SchemaTreeItem>,
        ids: Vec<String>,
    ) {
        self.schema_items_cache = items;
        self.all_schema_items_cache = all;
        self.schema_identifier_cache = Arc::new(ids);
        self.schema_rebuild_in_flight = false;
    }

    /// Active result tab's pane (or `Empty` if no tabs).
    pub fn results(&self) -> &ResultsPane {
        static EMPTY: ResultsPane = ResultsPane::Empty;
        self.result_tabs
            .get(self.active_result_tab)
            .map(|t| &t.kind)
            .unwrap_or(&EMPTY)
    }

    pub fn active_result(&self) -> Option<&ResultsTab> {
        self.result_tabs.get(self.active_result_tab)
    }

    pub fn active_result_mut(&mut self) -> Option<&mut ResultsTab> {
        self.result_tabs.get_mut(self.active_result_tab)
    }

    pub fn has_results(&self) -> bool {
        !self.result_tabs.is_empty()
    }

    pub fn results_scroll(&self) -> usize {
        self.active_result().map(|t| t.scroll).unwrap_or(0)
    }

    /// Returns the DDL payload when the active tab is a `SHOW CREATE ...`
    /// result (single row, ≥2 cols — the last column holds the DDL text).
    /// Used by the renderer and scroll logic to treat it as a text block
    /// rather than a 1-row table.
    pub fn active_ddl_text(&self) -> Option<&str> {
        let tab = self.active_result()?;
        if !crate::highlight::is_show_create(&tab.query) {
            return None;
        }
        match &tab.kind {
            ResultsPane::Results(r) if r.rows.len() == 1 && r.columns.len() >= 2 => {
                r.rows[0].last().map(|s| s.as_str())
            }
            _ => None,
        }
    }

    pub fn results_col_scroll(&self) -> usize {
        self.active_result().map(|t| t.col_scroll).unwrap_or(0)
    }

    /// Append a new result tab.
    pub fn push_result_tab(&mut self, query: String, kind: ResultsPane) {
        let tab = ResultsTab {
            query,
            kind,
            scroll: 0,
            col_scroll: 0,
            saved_filename: None,
            cursor: ResultsCursor::default(),
            selection: None,
        };
        self.result_tabs.push(tab);
        self.active_result_tab = self.result_tabs.len() - 1;
        self.editor_ratio = 0.5;
    }

    /// Push a loading placeholder tab immediately after syntax check passes.
    /// Returns the index so the executor can replace it when done.
    pub fn push_loading_tab(&mut self, query: String) -> usize {
        let tab = ResultsTab {
            query,
            kind: ResultsPane::Loading,
            scroll: 0,
            col_scroll: 0,
            saved_filename: None,
            cursor: ResultsCursor::default(),
            selection: None,
        };
        self.result_tabs.push(tab);
        let idx = self.result_tabs.len() - 1;
        self.active_result_tab = idx;
        self.editor_ratio = 0.5;
        idx
    }

    /// Replace a loading tab at `idx` with the final result or error.
    pub fn finish_result_tab(&mut self, idx: usize, kind: ResultsPane) {
        if let Some(tab) = self.result_tabs.get_mut(idx) {
            tab.kind = kind;
            tab.cursor = ResultsCursor::Query;
        }
    }

    /// Begin a run-all batch: returns the index where the first batch tab will land.
    pub fn start_batch(&mut self) -> usize {
        self.batch_in_progress = true;
        self.result_tabs.len()
    }

    /// End the current batch and focus the first batch tab.
    pub fn end_batch(&mut self, batch_start: usize) {
        self.batch_in_progress = false;
        if batch_start < self.result_tabs.len() {
            self.active_result_tab = batch_start;
        } else if !self.result_tabs.is_empty() {
            self.active_result_tab = self.result_tabs.len() - 1;
        }
        self.editor_ratio = 0.5;
    }

    pub fn next_result_tab(&mut self) {
        if self.result_tabs.is_empty() {
            return;
        }
        self.active_result_tab = (self.active_result_tab + 1) % self.result_tabs.len();
        self.clamp_results_cursor();
    }

    pub fn prev_result_tab(&mut self) {
        if self.result_tabs.is_empty() {
            return;
        }
        self.active_result_tab = if self.active_result_tab == 0 {
            self.result_tabs.len() - 1
        } else {
            self.active_result_tab - 1
        };
        self.clamp_results_cursor();
    }

    pub fn close_active_result_tab(&mut self) {
        if self.result_tabs.is_empty() {
            return;
        }
        self.result_tabs.remove(self.active_result_tab);
        if self.result_tabs.is_empty() {
            self.active_result_tab = 0;
            self.editor_ratio = 1.0;
        } else if self.active_result_tab >= self.result_tabs.len() {
            self.active_result_tab = self.result_tabs.len() - 1;
        }
    }

    /// Replace single-query result. Wraps `push_result_tab` for the test API.
    pub fn set_results(&mut self, mut result: QueryResult) {
        result.compute_col_widths();
        self.push_result_tab(String::new(), ResultsPane::Results(result));
    }

    pub fn set_error(&mut self, msg: String) {
        self.push_result_tab(String::new(), ResultsPane::Error(msg));
    }

    pub fn dismiss_results(&mut self) {
        self.result_tabs.clear();
        self.active_result_tab = 0;
        self.editor_ratio = 1.0;
    }

    pub fn scroll_results_down(&mut self) {
        let max = if let Some(ddl) = self.active_ddl_text() {
            ddl.lines().count().saturating_sub(1)
        } else {
            match self.active_result().map(|t| &t.kind) {
                Some(ResultsPane::Results(r)) => r.rows.len().saturating_sub(1),
                _ => 0,
            }
        };
        if let Some(t) = self.active_result_mut()
            && t.scroll < max
        {
            t.scroll += 1;
        }
    }

    pub fn scroll_results_up(&mut self) {
        if let Some(t) = self.active_result_mut() {
            t.scroll = t.scroll.saturating_sub(1);
        }
    }

    pub fn scroll_results_right(&mut self) {
        let max = if let Some(ddl) = self.active_ddl_text() {
            ddl.lines()
                .map(|l| l.chars().count())
                .max()
                .unwrap_or(0)
                .saturating_sub(1)
        } else {
            match self.active_result().map(|t| &t.kind) {
                Some(ResultsPane::Results(r)) => r.columns.len().saturating_sub(1),
                _ => 0,
            }
        };
        if let Some(t) = self.active_result_mut()
            && t.col_scroll < max
        {
            t.col_scroll += 1;
        }
    }

    pub fn scroll_results_left(&mut self) {
        if let Some(t) = self.active_result_mut() {
            t.col_scroll = t.col_scroll.saturating_sub(1);
        }
    }

    /// Ensure the cursor is a legal position for the active tab (e.g. after
    /// `finish_result_tab` swaps Loading → Error, or the pane kind changes).
    pub fn clamp_results_cursor(&mut self) {
        let Some(tab) = self.result_tabs.get_mut(self.active_result_tab) else {
            return;
        };
        tab.cursor = match (&tab.kind, tab.cursor) {
            (ResultsPane::Results(r), ResultsCursor::Header(c)) => {
                if r.columns.is_empty() {
                    ResultsCursor::Query
                } else {
                    ResultsCursor::Header(c.min(r.columns.len() - 1))
                }
            }
            (ResultsPane::Results(r), ResultsCursor::Cell { row, col }) => {
                if r.rows.is_empty() || r.columns.is_empty() {
                    ResultsCursor::Query
                } else {
                    ResultsCursor::Cell {
                        row: row.min(r.rows.len() - 1),
                        col: col.min(r.columns.len() - 1),
                    }
                }
            }
            (ResultsPane::Error(e), ResultsCursor::MessageLine(i)) => {
                let n = e.lines().count();
                if n == 0 {
                    ResultsCursor::Query
                } else {
                    ResultsCursor::MessageLine(i.min(n - 1))
                }
            }
            (ResultsPane::Cancelled, ResultsCursor::MessageLine(_)) => {
                ResultsCursor::MessageLine(0)
            }
            (ResultsPane::Results(_), ResultsCursor::MessageLine(_))
            | (ResultsPane::Error(_), ResultsCursor::Header(_))
            | (ResultsPane::Error(_), ResultsCursor::Cell { .. })
            | (ResultsPane::Cancelled, ResultsCursor::Header(_))
            | (ResultsPane::Cancelled, ResultsCursor::Cell { .. })
            | (ResultsPane::Empty | ResultsPane::Loading, _) => ResultsCursor::Query,
            (_, c) => c,
        };
        let rows = self.results_body_rows.load(Ordering::Relaxed);
        let width = self.results_body_width.load(Ordering::Relaxed);
        if let Some(tab) = self.result_tabs.get_mut(self.active_result_tab) {
            Self::ensure_cursor_visible(tab, rows, width);
        }
    }

    /// Scroll the body so the cursor is inside the rendered viewport. `rows`
    /// is the visible row count, `width` is the body width in cells. Falls
    /// back to safe defaults when the TUI hasn't rendered yet.
    fn ensure_cursor_visible(tab: &mut ResultsTab, rows: u16, width: u16) {
        let rows = if rows == 0 { 10 } else { rows as usize };
        match tab.cursor {
            ResultsCursor::Cell { row, col } => {
                if row < tab.scroll {
                    tab.scroll = row;
                } else if row >= tab.scroll + rows {
                    tab.scroll = row + 1 - rows;
                }
                if let ResultsPane::Results(r) = &tab.kind {
                    scroll_cols_into_view_slice(&r.col_widths, &mut tab.col_scroll, col, width);
                }
            }
            ResultsCursor::Header(col) => {
                if let ResultsPane::Results(r) = &tab.kind {
                    scroll_cols_into_view_slice(&r.col_widths, &mut tab.col_scroll, col, width);
                }
            }
            ResultsCursor::MessageLine(line) => {
                if line < tab.scroll {
                    tab.scroll = line;
                } else if line >= tab.scroll + rows {
                    tab.scroll = line + 1 - rows;
                }
            }
            _ => {}
        }
    }

    fn with_active_tab<F: FnOnce(&mut ResultsTab)>(&mut self, f: F) {
        let rows = self.results_body_rows.load(Ordering::Relaxed);
        let width = self.results_body_width.load(Ordering::Relaxed);
        if let Some(t) = self.result_tabs.get_mut(self.active_result_tab) {
            f(t);
            Self::ensure_cursor_visible(t, rows, width);
        }
    }

    pub fn results_cursor_down(&mut self) {
        self.with_active_tab(|t| {
            t.cursor = match (&t.kind, t.cursor) {
                (ResultsPane::Results(r), ResultsCursor::Query) if !r.columns.is_empty() => {
                    ResultsCursor::Header(0)
                }
                (ResultsPane::Results(r), ResultsCursor::Header(c)) if !r.rows.is_empty() => {
                    ResultsCursor::Cell { row: 0, col: c }
                }
                (ResultsPane::Results(r), ResultsCursor::Cell { row, col })
                    if row + 1 < r.rows.len() =>
                {
                    ResultsCursor::Cell { row: row + 1, col }
                }
                (ResultsPane::Error(e), ResultsCursor::Query) if e.lines().next().is_some() => {
                    ResultsCursor::MessageLine(0)
                }
                (ResultsPane::Error(e), ResultsCursor::MessageLine(i))
                    if i + 1 < e.lines().count() =>
                {
                    ResultsCursor::MessageLine(i + 1)
                }
                (ResultsPane::Cancelled, ResultsCursor::Query) => ResultsCursor::MessageLine(0),
                (_, c) => c,
            };
        });
    }

    pub fn results_cursor_up(&mut self) {
        self.with_active_tab(|t| {
            t.cursor = match (&t.kind, t.cursor) {
                (ResultsPane::Results(_), ResultsCursor::Header(_)) => ResultsCursor::Query,
                (ResultsPane::Results(_), ResultsCursor::Cell { row: 0, col }) => {
                    ResultsCursor::Header(col)
                }
                (ResultsPane::Results(_), ResultsCursor::Cell { row, col }) => {
                    ResultsCursor::Cell { row: row - 1, col }
                }
                (ResultsPane::Error(_), ResultsCursor::MessageLine(0))
                | (ResultsPane::Cancelled, ResultsCursor::MessageLine(_)) => ResultsCursor::Query,
                (ResultsPane::Error(_), ResultsCursor::MessageLine(i)) => {
                    ResultsCursor::MessageLine(i - 1)
                }
                (_, c) => c,
            };
        });
    }

    pub fn results_cursor_left(&mut self) {
        self.with_active_tab(|t| {
            t.cursor = match (&t.kind, t.cursor) {
                (ResultsPane::Results(_), ResultsCursor::Header(c)) if c > 0 => {
                    ResultsCursor::Header(c - 1)
                }
                (ResultsPane::Results(_), ResultsCursor::Cell { row, col }) if col > 0 => {
                    ResultsCursor::Cell { row, col: col - 1 }
                }
                (_, c) => c,
            };
        });
    }

    pub fn results_cursor_right(&mut self) {
        self.with_active_tab(|t| {
            t.cursor = match (&t.kind, t.cursor) {
                (ResultsPane::Results(r), ResultsCursor::Header(c)) if c + 1 < r.columns.len() => {
                    ResultsCursor::Header(c + 1)
                }
                (ResultsPane::Results(r), ResultsCursor::Cell { row, col })
                    if col + 1 < r.columns.len() =>
                {
                    ResultsCursor::Cell { row, col: col + 1 }
                }
                (_, c) => c,
            };
        });
    }

    /// `gg` — jump to the first body row, keeping the current column.
    pub fn results_cursor_first_row(&mut self) {
        self.with_active_tab(|t| {
            t.cursor = match (&t.kind, t.cursor) {
                (ResultsPane::Results(r), ResultsCursor::Cell { col, .. })
                    if !r.rows.is_empty() =>
                {
                    ResultsCursor::Cell { row: 0, col }
                }
                (ResultsPane::Results(r), ResultsCursor::Header(c)) if !r.rows.is_empty() => {
                    ResultsCursor::Cell { row: 0, col: c }
                }
                (ResultsPane::Error(_), _) | (ResultsPane::Cancelled, _) => {
                    ResultsCursor::MessageLine(0)
                }
                (_, c) => c,
            };
        });
    }

    /// `G` — jump to the last body row, keeping the current column.
    pub fn results_cursor_last_row(&mut self) {
        self.with_active_tab(|t| {
            t.cursor = match (&t.kind, t.cursor) {
                (ResultsPane::Results(r), ResultsCursor::Cell { col, .. })
                    if !r.rows.is_empty() =>
                {
                    ResultsCursor::Cell {
                        row: r.rows.len() - 1,
                        col,
                    }
                }
                (ResultsPane::Results(r), ResultsCursor::Header(c)) if !r.rows.is_empty() => {
                    ResultsCursor::Cell {
                        row: r.rows.len() - 1,
                        col: c,
                    }
                }
                (ResultsPane::Error(e), _) => {
                    let n = e.lines().count();
                    if n > 0 {
                        ResultsCursor::MessageLine(n - 1)
                    } else {
                        t.cursor
                    }
                }
                (_, c) => c,
            };
        });
    }

    /// `0` — jump to the first column of the current row.
    pub fn results_cursor_row_start(&mut self) {
        self.with_active_tab(|t| {
            t.cursor = match (&t.kind, t.cursor) {
                (ResultsPane::Results(_), ResultsCursor::Header(_)) => ResultsCursor::Header(0),
                (ResultsPane::Results(_), ResultsCursor::Cell { row, .. }) => {
                    ResultsCursor::Cell { row, col: 0 }
                }
                (_, c) => c,
            };
        });
    }

    /// Parse the first GFM table found in a markdown hover payload.
    /// Uses pulldown-cmark so emphasis, inline code, and link text
    /// inside cells get flattened to their underlying text values
    /// rather than leaking markdown delimiters into the grid.
    pub fn parse_hover_table(text: &str) -> Option<QueryResult> {
        use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_TABLES);
        let mut parser = Parser::new_ext(text, opts);

        let mut header: Vec<String> = Vec::new();
        let mut rows: Vec<Vec<String>> = Vec::new();

        // Walk events until we hit a Table start. Everything outside a
        // table is ignored — callers use the full markdown path for
        // non-tabular hover content.
        loop {
            match parser.next()? {
                Event::Start(Tag::Table(_)) => break,
                _ => continue,
            }
        }

        let mut in_head = false;
        let mut current_row: Vec<String> = Vec::new();
        let mut current_cell = String::new();
        let mut in_cell = false;

        for event in parser.by_ref() {
            match event {
                Event::Start(Tag::TableHead) => {
                    in_head = true;
                    current_row.clear();
                }
                Event::End(TagEnd::TableHead) => {
                    // pulldown-cmark emits TableCells directly inside
                    // TableHead (no wrapping TableRow), so we flush the
                    // accumulated row here.
                    header = std::mem::take(&mut current_row);
                    in_head = false;
                }
                Event::Start(Tag::TableRow) => current_row.clear(),
                Event::End(TagEnd::TableRow) => {
                    rows.push(std::mem::take(&mut current_row));
                }
                Event::Start(Tag::TableCell) => {
                    current_cell.clear();
                    in_cell = true;
                }
                Event::End(TagEnd::TableCell) => {
                    // sqls emits empty cells as a literal "``" (empty
                    // inline-code span), which pulldown-cmark passes
                    // through as Text rather than a Code event. Strip
                    // a surrounding pair of backticks so visually-
                    // empty cells don't render as "``" in the grid.
                    let mut cell = std::mem::take(&mut current_cell).trim().to_string();
                    while cell.starts_with('`') && cell.ends_with('`') && cell.len() >= 2 {
                        cell = cell[1..cell.len() - 1].trim().to_string();
                    }
                    current_row.push(cell);
                    in_cell = false;
                }
                Event::Text(t) if in_cell => current_cell.push_str(&t),
                Event::Code(t) if in_cell => current_cell.push_str(&t),
                Event::SoftBreak | Event::HardBreak if in_cell => current_cell.push(' '),
                Event::End(TagEnd::Table) => break,
                _ => {}
            }
        }
        // `in_head` is retained for potential future use (e.g. different
        // cell trim rules); silence the unused-write warning until then.
        let _ = in_head;

        if header.is_empty() || rows.is_empty() {
            return None;
        }
        for row in &mut rows {
            row.resize(header.len(), String::new());
        }
        let mut col_widths: Vec<u16> = header
            .iter()
            .map(|c| (c.chars().count() as u16).saturating_add(2))
            .collect();
        for row in &rows {
            for (i, cell) in row.iter().enumerate() {
                let w = (cell.chars().count() as u16).saturating_add(2);
                if w > col_widths[i] {
                    col_widths[i] = w;
                }
            }
        }
        Some(QueryResult {
            columns: header,
            rows,
            col_widths,
        })
    }

    /// Locate `name` in the schema tree and return `(db, columns_loaded)`
    /// for the first match. Used by the K hover path to decide whether
    /// to render from cache, queue a column load, or fall back to LSP.
    /// Case-insensitive.
    pub fn find_table(&self, name: &str) -> Option<(String, bool)> {
        let lower = name.to_lowercase();
        for node in &self.schema_nodes {
            let SchemaNode::Database {
                name: db, tables, ..
            } = node
            else {
                continue;
            };
            for t in tables {
                let SchemaNode::Table {
                    name: tname,
                    columns_loaded_at,
                    ..
                } = t
                else {
                    continue;
                };
                if tname.to_lowercase() == lower {
                    return Some((db.clone(), columns_loaded_at.is_some()));
                }
            }
        }
        None
    }

    /// Synthesise a hover table from the schema cache when the word
    /// under the cursor matches a table we've already fetched columns
    /// for. Returns `None` when the name doesn't resolve or the
    /// table's columns haven't been loaded yet — callers fall back
    /// to a real LSP hover in those cases. Case-insensitive.
    pub fn hover_table_from_cache(&self, name: &str) -> Option<QueryResult> {
        let lower = name.to_lowercase();
        for node in &self.schema_nodes {
            let SchemaNode::Database { tables, .. } = node else {
                continue;
            };
            for t in tables {
                let SchemaNode::Table {
                    name: tname,
                    columns,
                    columns_loaded_at,
                    ..
                } = t
                else {
                    continue;
                };
                if tname.to_lowercase() != lower {
                    continue;
                }
                // Columns must have been fetched already — otherwise
                // we'd render an empty grid. Fall through to LSP in
                // that case.
                columns_loaded_at.as_ref()?;
                if columns.is_empty() {
                    return None;
                }
                let header = vec![
                    "Column".into(),
                    "Type".into(),
                    "PK".into(),
                    "Nullable".into(),
                ];
                let rows: Vec<Vec<String>> = columns
                    .iter()
                    .filter_map(|c| {
                        if let SchemaNode::Column {
                            name,
                            type_name,
                            nullable,
                            is_pk,
                        } = c
                        {
                            Some(vec![
                                name.clone(),
                                type_name.clone(),
                                if *is_pk { "✓" } else { "" }.into(),
                                if *nullable { "✓" } else { "" }.into(),
                            ])
                        } else {
                            None
                        }
                    })
                    .collect();
                if rows.is_empty() {
                    return None;
                }
                let mut col_widths: Vec<u16> = header
                    .iter()
                    .map(|c: &String| (c.chars().count() as u16).saturating_add(2))
                    .collect();
                for row in &rows {
                    for (i, cell) in row.iter().enumerate() {
                        let w = (cell.chars().count() as u16).saturating_add(2);
                        if w > col_widths[i] {
                            col_widths[i] = w;
                        }
                    }
                }
                return Some(QueryResult {
                    columns: header,
                    rows,
                    col_widths,
                });
            }
        }
        None
    }

    /// K-on-table fast path that needs a column fetch: queue a lazy
    /// `SchemaLoadRequest::Columns` and open the popup in a loading
    /// state. The host loop polls `try_install_pending_hover_table`
    /// after each schema_cache_rx drain and swaps to the table view
    /// once columns are populated — saves the LSP round-trip on the
    /// first hover for any table.
    pub fn open_hover_pending_columns(&mut self, db: String, table: String) {
        self.request_schema_load(SchemaLoadRequest::Columns {
            db: db.clone(),
            table: table.clone(),
        });
        self.hover_pending_table = Some((db, table));
        self.open_hover_loading();
    }

    /// Polled by the main loop after each schema cache rebuild.
    /// Returns `true` (and installs the cached table view) once the
    /// pending column load has populated. Caller drops the loading
    /// spinner when this returns `true`.
    pub fn try_install_pending_hover_table(&mut self) -> bool {
        let Some((_, table)) = self.hover_pending_table.clone() else {
            return false;
        };
        if !self.hover_loading {
            // User dismissed the popup before the load returned;
            // drop the pending state so a stale install can't fire
            // on the next schema rebuild.
            self.hover_pending_table = None;
            return false;
        }
        if let Some(t) = self.hover_table_from_cache(&table) {
            self.hover_pending_table = None;
            self.open_hover_table(t);
            return true;
        }
        false
    }

    /// Open the hover popup in a loading state — focus transfers
    /// immediately so the user can see the popup + cancel it with
    /// `Esc` even before the LSP has answered. Payload-install paths
    /// below overwrite the loading flag when the response arrives.
    pub fn open_hover_loading(&mut self) {
        // Don't stomp a previous hover_prev_focus if the user mashes K
        // twice — keep the oldest so the eventual close still lands on
        // the original pane.
        if self.focus != Focus::Hover {
            self.hover_prev_focus = Some(self.focus);
        }
        self.focus = Focus::Hover;
        self.hover_loading = true;
        self.hover_text = None;
        self.hover_table = None;
        self.hover_selection = None;
        self.hover_scroll = 0;
        self.hover_col_scroll = 0;
    }

    /// Install a hover grid and switch focus to `Focus::Hover`. Stashes
    /// the previous focus so Esc can restore it.
    pub fn open_hover_table(&mut self, table: QueryResult) {
        if self.focus != Focus::Hover {
            self.hover_prev_focus = Some(self.focus);
        }
        self.focus = Focus::Hover;
        self.hover_loading = false;
        self.hover_cursor = ResultsCursor::Cell { row: 0, col: 0 };
        self.hover_selection = None;
        self.hover_scroll = 0;
        self.hover_col_scroll = 0;
        self.hover_table = Some(table);
    }

    /// Install a plain-text hover payload and switch focus to
    /// `Focus::Hover`. Markdown rendering happens at draw time; the
    /// popup is scrollable via `j` / `k` while focused, and `Esc`
    /// closes it.
    pub fn open_hover_text(&mut self, text: String) {
        if self.focus != Focus::Hover {
            self.hover_prev_focus = Some(self.focus);
        }
        self.focus = Focus::Hover;
        self.hover_loading = false;
        self.hover_text = Some(text);
        self.hover_table = None;
        self.hover_selection = None;
        self.hover_scroll = 0;
        self.hover_col_scroll = 0;
    }

    /// Close the hover popup and restore focus to wherever `K` was
    /// pressed. Clears both the text and table payloads.
    pub fn close_hover(&mut self) {
        self.hover_loading = false;
        self.hover_text = None;
        self.hover_table = None;
        self.hover_selection = None;
        self.hover_pending_table = None;
        if let Some(prev) = self.hover_prev_focus.take() {
            self.focus = prev;
        }
    }

    /// Move the hover-grid cursor. `dr`/`dc` are deltas (positive =
    /// down / right). Clamped to the grid's bounds; row and column
    /// scroll follow the cursor so it never leaves the popup window.
    pub fn hover_cursor_move(&mut self, dr: i32, dc: i32) {
        let Some(ref t) = self.hover_table else {
            return;
        };
        if t.rows.is_empty() || t.columns.is_empty() {
            return;
        }
        let (row, col) = match self.hover_cursor {
            ResultsCursor::Cell { row, col } => (row as i32, col as i32),
            _ => (0, 0),
        };
        let new_row = (row + dr).clamp(0, t.rows.len() as i32 - 1) as usize;
        let new_col = (col + dc).clamp(0, t.columns.len() as i32 - 1) as usize;
        self.hover_cursor = ResultsCursor::Cell {
            row: new_row,
            col: new_col,
        };
        self.clamp_hover_scroll();
    }

    /// Drag-extended variant of [`Self::results_click_to_cell`] —
    /// when the pointer leaves the body the cursor keeps stepping
    /// toward the edge it crossed, so a drag-select that runs off the
    /// visible area still grows the selection (and auto-scrolls via
    /// `clamp_results_cursor`).
    pub fn results_drag_to_cell(&self, mx: u16, my: u16) -> Option<(usize, usize)> {
        if let Some(cell) = self.results_click_to_cell(mx, my) {
            return Some(cell);
        }
        let tab = self.active_result()?;
        let ResultsPane::Results(r) = &tab.kind else {
            return None;
        };
        if r.rows.is_empty() || r.columns.is_empty() {
            return None;
        }
        let body_x = self.results_body_x.load(Ordering::Relaxed);
        let body_y = self.results_body_y.load(Ordering::Relaxed);
        let body_w = self.results_body_width.load(Ordering::Relaxed);
        let body_h = self.results_body_rows.load(Ordering::Relaxed);
        if body_w == 0 || body_h == 0 {
            return None;
        }
        let (cur_row, cur_col) = match tab.cursor {
            ResultsCursor::Cell { row, col } => (row, col),
            ResultsCursor::Header(c) => (0, c),
            _ => (0, 0),
        };
        let mut row = cur_row;
        let mut col = cur_col;
        if my < body_y {
            row = row.saturating_sub(1);
        } else if my >= body_y + body_h {
            row = (row + 1).min(r.rows.len().saturating_sub(1));
        }
        if mx < body_x {
            col = col.saturating_sub(1);
        } else if mx >= body_x + body_w {
            col = (col + 1).min(r.columns.len().saturating_sub(1));
        }
        Some((row, col))
    }

    /// Translate a terminal-space mouse click into a `(row, col)` on
    /// the active results grid. Mirrors [`Self::hover_click_to_cell`]
    /// so both panes share the same click → cell idiom (drag-select,
    /// click-to-cursor, etc.). Returns `None` when the click misses
    /// the body or no results are visible.
    pub fn results_click_to_cell(&self, mx: u16, my: u16) -> Option<(usize, usize)> {
        let tab = self.active_result()?;
        let ResultsPane::Results(r) = &tab.kind else {
            return None;
        };
        let body_x = self.results_body_x.load(Ordering::Relaxed);
        let body_y = self.results_body_y.load(Ordering::Relaxed);
        let body_h = self.results_body_rows.load(Ordering::Relaxed);
        let body_w = self.results_body_width.load(Ordering::Relaxed);
        if body_w == 0 || body_h == 0 {
            return None;
        }
        if mx < body_x || mx >= body_x + body_w || my < body_y || my >= body_y + body_h {
            return None;
        }
        let rel_row = (my - body_y) as usize;
        let row = tab.scroll + rel_row;
        if row >= r.rows.len() {
            return None;
        }
        let rel_x = (mx - body_x) as u32;
        let mut acc: u32 = 0;
        let mut col = tab.col_scroll;
        while col < r.columns.len() {
            let w = r.col_widths.get(col).copied().unwrap_or(0) as u32 + 1;
            if rel_x < acc + w {
                return Some((row, col));
            }
            acc += w;
            col += 1;
        }
        None
    }

    /// Drag-extended variant of [`Self::hover_click_to_cell`] — when
    /// the pointer leaves the body during a mouse drag we step the
    /// cursor (and the scroll offset) one cell toward the edge the
    /// drag is crossing, so the selection keeps growing in that
    /// direction. Returns `None` only when there's no hover grid.
    pub fn hover_drag_to_cell(&self, mx: u16, my: u16) -> Option<(usize, usize)> {
        if let Some(cell) = self.hover_click_to_cell(mx, my) {
            return Some(cell);
        }
        let t = self.hover_table.as_ref()?;
        if t.rows.is_empty() || t.columns.is_empty() {
            return None;
        }
        let body_x = self.hover_body_x.load(Ordering::Relaxed);
        let body_y = self.hover_body_y.load(Ordering::Relaxed);
        let body_w = self.hover_body_width.load(Ordering::Relaxed);
        let body_h = self.hover_body_height.load(Ordering::Relaxed);
        if body_w == 0 || body_h == 0 {
            return None;
        }
        let (cur_row, cur_col) = match self.hover_cursor {
            ResultsCursor::Cell { row, col } => (row, col),
            _ => (0, 0),
        };
        let mut row = cur_row;
        let mut col = cur_col;
        if my < body_y {
            row = row.saturating_sub(1);
        } else if my >= body_y + body_h {
            row = (row + 1).min(t.rows.len().saturating_sub(1));
        }
        if mx < body_x {
            col = col.saturating_sub(1);
        } else if mx >= body_x + body_w {
            col = (col + 1).min(t.columns.len().saturating_sub(1));
        }
        Some((row, col))
    }

    /// Translate a terminal-space mouse click into a `(row, col)` on
    /// the hover grid, returning `None` when the click didn't land
    /// inside the body area. Uses the dimensions published by the
    /// render path each frame, plus `hover_scroll` / `hover_col_scroll`
    /// for the current viewport offset.
    pub fn hover_click_to_cell(&self, mx: u16, my: u16) -> Option<(usize, usize)> {
        let t = self.hover_table.as_ref()?;
        let body_x = self.hover_body_x.load(Ordering::Relaxed);
        let body_y = self.hover_body_y.load(Ordering::Relaxed);
        let body_w = self.hover_body_width.load(Ordering::Relaxed);
        let body_h = self.hover_body_height.load(Ordering::Relaxed);
        if body_w == 0 || body_h == 0 {
            return None;
        }
        if mx < body_x || mx >= body_x + body_w || my < body_y || my >= body_y + body_h {
            return None;
        }
        let rel_row = (my - body_y) as usize;
        let row = self.hover_scroll + rel_row;
        if row >= t.rows.len() {
            return None;
        }
        // Walk columns from `hover_col_scroll`, accumulating widths +
        // the 1-cell separator, until we cover the click's x offset.
        let rel_x = (mx - body_x) as u32;
        let mut acc: u32 = 0;
        let mut col = self.hover_col_scroll;
        while col < t.columns.len() {
            let w = t.col_widths.get(col).copied().unwrap_or(0) as u32 + 1;
            if rel_x < acc + w {
                return Some((row, col));
            }
            acc += w;
            col += 1;
        }
        None
    }

    /// Re-clamp the hover row + column scroll offsets so the cursor
    /// cell stays inside the visible viewport. Published dimensions
    /// come from the render path; if they're 0 (popup hasn't drawn
    /// yet) we leave scroll alone and let the next frame fix it up.
    pub fn clamp_hover_scroll(&mut self) {
        let Some(ref t) = self.hover_table else {
            return;
        };
        let (cur_row, cur_col) = match self.hover_cursor {
            ResultsCursor::Cell { row, col } => (row, col),
            _ => return,
        };
        let rows = self.hover_body_height.load(Ordering::Relaxed) as usize;
        if rows > 0 {
            if cur_row < self.hover_scroll {
                self.hover_scroll = cur_row;
            } else if cur_row >= self.hover_scroll + rows {
                self.hover_scroll = cur_row + 1 - rows;
            }
        }
        let width = self.hover_body_width.load(Ordering::Relaxed);
        if width > 0 {
            scroll_cols_into_view_slice(&t.col_widths, &mut self.hover_col_scroll, cur_col, width);
        }
    }

    /// Jump the hover cursor to the first / last row (`gg` / `G`) or
    /// first / last column (`0` / `$`) of the current row. `which`
    /// selects among them.
    pub fn hover_cursor_edge(&mut self, which: HoverEdge) {
        let Some(ref t) = self.hover_table else {
            return;
        };
        if t.rows.is_empty() || t.columns.is_empty() {
            return;
        }
        let (row, col) = match self.hover_cursor {
            ResultsCursor::Cell { row, col } => (row, col),
            _ => (0, 0),
        };
        self.hover_cursor = match which {
            HoverEdge::FirstRow => ResultsCursor::Cell { row: 0, col },
            HoverEdge::LastRow => ResultsCursor::Cell {
                row: t.rows.len() - 1,
                col,
            },
            HoverEdge::RowStart => ResultsCursor::Cell { row, col: 0 },
            HoverEdge::RowEnd => ResultsCursor::Cell {
                row,
                col: t.columns.len() - 1,
            },
        };
        self.clamp_hover_scroll();
    }

    /// Yank the current hover-cell, or the selection if one is active.
    /// TSV format when the selection covers a rectangle, matching the
    /// results-pane behaviour.
    pub fn hover_yank(&self) -> Option<(String, &'static str)> {
        let t = self.hover_table.as_ref()?;
        if let Some(sel) = self.hover_selection {
            let (ar, ac) = sel.anchor;
            let (cr, cc) = match self.hover_cursor {
                ResultsCursor::Cell { row, col } => (row, col),
                _ => return None,
            };
            let (top, bot) = (ar.min(cr), ar.max(cr));
            let (left, right) = match sel.mode {
                ResultsSelectionMode::Line => (0, t.columns.len().saturating_sub(1)),
                ResultsSelectionMode::Block => (ac.min(cc), ac.max(cc)),
            };
            let label = match sel.mode {
                ResultsSelectionMode::Line => "Rows",
                ResultsSelectionMode::Block => "Block",
            };
            let mut out = String::new();
            for r in top..=bot.min(t.rows.len().saturating_sub(1)) {
                let row = &t.rows[r];
                for c in left..=right.min(row.len().saturating_sub(1)) {
                    if c > left {
                        out.push('\t');
                    }
                    out.push_str(row.get(c).map(|s| s.as_str()).unwrap_or(""));
                }
                if r < bot.min(t.rows.len() - 1) {
                    out.push('\n');
                }
            }
            return Some((out, label));
        }
        // No selection — yank just the single cell under the cursor.
        if let ResultsCursor::Cell { row, col } = self.hover_cursor {
            let v = t
                .rows
                .get(row)
                .and_then(|r| r.get(col))
                .cloned()
                .unwrap_or_default();
            return Some((v, "Cell"));
        }
        None
    }

    /// Scan result cells for the first occurrence of `needle` (case-
    /// insensitive) and park the cursor on the matching cell. `forward`
    /// picks the scan direction; `skip_current` starts from the cell
    /// after the cursor (`n` / `N` don't get stuck on their current
    /// match). Wraps around the buffer end. Returns `true` when a match
    /// was found and the cursor moved.
    pub fn results_find(&mut self, needle: &str, forward: bool, skip_current: bool) -> bool {
        if needle.is_empty() {
            return false;
        }
        let needle_lc = needle.to_lowercase();
        let tab_idx = self.active_result_tab;
        let Some(tab) = self.result_tabs.get_mut(tab_idx) else {
            return false;
        };
        let ResultsPane::Results(r) = &tab.kind else {
            return false;
        };
        if r.rows.is_empty() || r.columns.is_empty() {
            return false;
        }
        let total_cols = r.columns.len();
        let total_rows = r.rows.len();
        let (cur_row, cur_col) = match tab.cursor {
            ResultsCursor::Cell { row, col } => (row, col),
            ResultsCursor::Header(c) => (0, c),
            _ => (0, 0),
        };
        let total = total_rows * total_cols;
        let start = cur_row * total_cols + cur_col;
        let step = if forward { 1 } else { total - 1 };
        let skip = if skip_current { 1 } else { 0 };
        for i in skip..total {
            let probe = (start + i * step) % total;
            let row = probe / total_cols;
            let col = probe % total_cols;
            let cell = r.rows.get(row).and_then(|r| r.get(col));
            if let Some(v) = cell
                && v.to_lowercase().contains(&needle_lc)
            {
                tab.cursor = ResultsCursor::Cell { row, col };
                return true;
            }
        }
        false
    }

    /// Scan the hover grid for `needle` (case-insensitive), parking
    /// the hover cursor on the first match. Semantics mirror
    /// [`Self::results_find`] so `/` in the popup behaves exactly
    /// like `/` in the results pane.
    pub fn hover_find(&mut self, needle: &str, forward: bool, skip_current: bool) -> bool {
        if needle.is_empty() {
            return false;
        }
        let needle_lc = needle.to_lowercase();
        let hit = {
            let Some(t) = self.hover_table.as_ref() else {
                return false;
            };
            if t.rows.is_empty() || t.columns.is_empty() {
                return false;
            }
            let total_cols = t.columns.len();
            let total_rows = t.rows.len();
            let (cur_row, cur_col) = match self.hover_cursor {
                ResultsCursor::Cell { row, col } => (row, col),
                _ => (0, 0),
            };
            let total = total_rows * total_cols;
            let start = cur_row * total_cols + cur_col;
            let step = if forward { 1 } else { total - 1 };
            let skip = if skip_current { 1 } else { 0 };
            let mut found: Option<(usize, usize)> = None;
            for i in skip..total {
                let probe = (start + i * step) % total;
                let row = probe / total_cols;
                let col = probe % total_cols;
                let cell = t.rows.get(row).and_then(|r| r.get(col));
                if let Some(v) = cell
                    && v.to_lowercase().contains(&needle_lc)
                {
                    found = Some((row, col));
                    break;
                }
            }
            found
        };
        match hit {
            Some((row, col)) => {
                self.hover_cursor = ResultsCursor::Cell { row, col };
                self.clamp_hover_scroll();
                true
            }
            None => false,
        }
    }

    /// `$` — jump to the last column of the current row.
    pub fn results_cursor_row_end(&mut self) {
        self.with_active_tab(|t| {
            t.cursor = match (&t.kind, t.cursor) {
                (ResultsPane::Results(r), ResultsCursor::Header(_)) if !r.columns.is_empty() => {
                    ResultsCursor::Header(r.columns.len() - 1)
                }
                (ResultsPane::Results(r), ResultsCursor::Cell { row, .. })
                    if !r.columns.is_empty() =>
                {
                    ResultsCursor::Cell {
                        row,
                        col: r.columns.len() - 1,
                    }
                }
                (_, c) => c,
            };
        });
    }

    /// Yank the entire row under the cursor as tab-separated values. Returns
    /// `None` when the active tab isn't a Results pane or has no row selected.
    pub fn results_cursor_yank_row(&self) -> Option<(String, &'static str)> {
        let tab = self.active_result()?;
        let ResultsPane::Results(r) = &tab.kind else {
            return None;
        };
        let row_idx = match tab.cursor {
            ResultsCursor::Cell { row, .. } => row,
            ResultsCursor::Header(_) | ResultsCursor::Query => 0,
            _ => return None,
        };
        let row = r.rows.get(row_idx)?;
        Some((row.join("\t"), "Row"))
    }

    /// Return the text currently under the results cursor + a short label used
    /// in the toast ("Query", "Column", "Value", "Line").
    pub fn results_cursor_yank(&self) -> Option<(String, &'static str)> {
        let tab = self.active_result()?;
        match (&tab.kind, tab.cursor) {
            (_, ResultsCursor::Query) => Some((tab.query.clone(), "Query")),
            (ResultsPane::Results(r), ResultsCursor::Header(c)) => {
                r.columns.get(c).map(|s| (s.clone(), "Column"))
            }
            (ResultsPane::Results(r), ResultsCursor::Cell { row, col }) => r
                .rows
                .get(row)
                .and_then(|r| r.get(col))
                .map(|s| (s.clone(), "Value")),
            (ResultsPane::Error(e), ResultsCursor::MessageLine(i)) => {
                e.lines().nth(i).map(|s| (s.to_string(), "Line"))
            }
            (ResultsPane::Cancelled, ResultsCursor::MessageLine(_)) => {
                Some(("Skipped after earlier error".to_string(), "Line"))
            }
            _ => None,
        }
    }

    /// Begin a visual-line or visual-block selection in the results body.
    /// Anchors at the current cursor cell; no-op if the cursor isn't on
    /// a Header or Cell (query line / message line selections aren't
    /// meaningful here).
    pub fn results_enter_selection(&mut self, mode: ResultsSelectionMode) {
        let tab_idx = self.active_result_tab;
        let Some(tab) = self.result_tabs.get_mut(tab_idx) else {
            return;
        };
        let (row, col) = match tab.cursor {
            ResultsCursor::Cell { row, col } => (row, col),
            ResultsCursor::Header(c) => (0, c),
            _ => return,
        };
        // If cursor sat on a header, drop to the first body row so the
        // selection lives in the data grid (mirrors how `V` on a non-row
        // line would behave if we had one).
        if matches!(tab.cursor, ResultsCursor::Header(c) if c == col)
            && let ResultsPane::Results(r) = &tab.kind
            && !r.rows.is_empty()
        {
            tab.cursor = ResultsCursor::Cell { row: 0, col };
        }
        tab.selection = Some(ResultsSelection {
            anchor: (row, col),
            mode,
        });
    }

    pub fn results_clear_selection(&mut self) {
        if let Some(tab) = self.result_tabs.get_mut(self.active_result_tab) {
            tab.selection = None;
        }
    }

    /// Current selection bounds as `(top_row, bot_row, left_col, right_col)`,
    /// or `None` if no active selection. For Line mode the column range
    /// spans the entire row regardless of anchor/cursor columns.
    pub fn results_selection_bounds(&self) -> Option<(usize, usize, usize, usize)> {
        let tab = self.active_result()?;
        let sel = tab.selection?;
        let ResultsPane::Results(r) = &tab.kind else {
            return None;
        };
        let (cur_row, cur_col) = match tab.cursor {
            ResultsCursor::Cell { row, col } => (row, col),
            ResultsCursor::Header(c) => (0, c),
            _ => return None,
        };
        let (ar, ac) = sel.anchor;
        let top = ar.min(cur_row);
        let bot = ar.max(cur_row);
        let (left, right) = match sel.mode {
            ResultsSelectionMode::Line => (0, r.columns.len().saturating_sub(1)),
            ResultsSelectionMode::Block => (ac.min(cur_col), ac.max(cur_col)),
        };
        Some((top, bot, left, right))
    }

    /// Yank whatever the current selection covers as TSV (tab between
    /// columns, newline between rows). Returns `None` when no selection
    /// is active. Does not clear the selection — callers usually do that
    /// after consuming the string.
    pub fn results_selection_yank(&self) -> Option<(String, &'static str)> {
        let (top, bot, left, right) = self.results_selection_bounds()?;
        let tab = self.active_result()?;
        let ResultsPane::Results(r) = &tab.kind else {
            return None;
        };
        let label = match tab.selection?.mode {
            ResultsSelectionMode::Line => "Rows",
            ResultsSelectionMode::Block => "Block",
        };
        let mut out = String::new();
        for row_idx in top..=bot.min(r.rows.len().saturating_sub(1)) {
            let row = &r.rows[row_idx];
            let mut first = true;
            for col_idx in left..=right.min(r.columns.len().saturating_sub(1)) {
                if !first {
                    out.push('\t');
                }
                first = false;
                if let Some(cell) = row.get(col_idx) {
                    out.push_str(cell);
                }
            }
            if row_idx < bot {
                out.push('\n');
            }
        }
        Some((out, label))
    }

    pub fn set_diagnostics(&mut self, diags: Vec<Diagnostic>) {
        self.lsp_diagnostics = diags;
    }

    pub fn set_highlights(&mut self, spans: Vec<HighlightSpan>) {
        self.highlight_spans = spans;
    }

    pub fn set_completions(&mut self, items: Vec<String>) {
        self.show_completions = !items.is_empty();
        if !items.is_empty() {
            self.completion_cursor = self.completion_cursor.min(items.len().saturating_sub(1));
        }
        self.completions = items;
    }

    pub fn dismiss_completions(&mut self) {
        self.show_completions = false;
        self.completion_cursor = 0;
    }

    pub fn completion_cursor_up(&mut self) {
        self.completion_cursor = self.completion_cursor.saturating_sub(1);
    }

    pub fn completion_cursor_down(&mut self) {
        let max = self.completions.len().saturating_sub(1);
        if self.completion_cursor < max {
            self.completion_cursor += 1;
        }
    }

    /// Return the currently selected completion label, if any.
    pub fn selected_completion(&self) -> Option<&str> {
        self.completions
            .get(self.completion_cursor)
            .map(|s| s.as_str())
    }

    pub fn has_errors(&self) -> bool {
        self.lsp_diagnostics
            .iter()
            .any(|d| d.severity == DiagnosticSeverity::ERROR)
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some(msg.into());
    }

    pub fn clear_status(&mut self) {
        self.status_message = None;
    }

    /// Sorted, deduplicated identifier names from the schema tree. Cheap to clone —
    /// backed by an `Arc` that is only rebuilt when the schema changes.
    pub fn schema_identifier_names(&self) -> Arc<Vec<String>> {
        Arc::clone(&self.schema_identifier_cache)
    }

    /// Context-aware completion candidates. Returns names matching `prefix`
    /// (case-insensitive), scoped to what `ctx` says makes sense at the cursor.
    ///
    /// - `Qualified { parent }` — children of the named db or table.
    /// - `Table` — table names across all databases.
    /// - `Column { tables }` — columns of those tables (all columns if empty).
    /// - `Any` — every identifier in the tree (original behavior).
    pub fn completions_for_context(&self, ctx: &CompletionCtx, prefix: &str) -> Vec<String> {
        let p = prefix.to_lowercase();
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<String> = Vec::new();
        let push = |name: &str, out: &mut Vec<String>, seen: &mut HashSet<String>| {
            if name.to_lowercase().starts_with(&p) && seen.insert(name.to_owned()) {
                out.push(name.to_owned());
            }
        };
        match ctx {
            CompletionCtx::Qualified { parent } => {
                for node in &self.schema_nodes {
                    if let SchemaNode::Database { name, tables, .. } = node {
                        if name.eq_ignore_ascii_case(parent) {
                            for t in tables {
                                push(t.name(), &mut out, &mut seen);
                            }
                        }
                        // `parent` might also be a bare table name (no db qualifier).
                        for t in tables {
                            if let SchemaNode::Table {
                                name: tn, columns, ..
                            } = t
                                && tn.eq_ignore_ascii_case(parent)
                            {
                                for c in columns {
                                    push(c.name(), &mut out, &mut seen);
                                }
                            }
                        }
                    }
                }
            }
            CompletionCtx::Table => {
                for node in &self.schema_nodes {
                    if let SchemaNode::Database { tables, .. } = node {
                        for t in tables {
                            push(t.name(), &mut out, &mut seen);
                        }
                    }
                }
            }
            CompletionCtx::Column { tables } => {
                if tables.is_empty() {
                    for node in &self.schema_nodes {
                        if let SchemaNode::Database { tables: ts, .. } = node {
                            for t in ts {
                                if let SchemaNode::Table { columns, .. } = t {
                                    for c in columns {
                                        push(c.name(), &mut out, &mut seen);
                                    }
                                }
                            }
                        }
                    }
                } else {
                    let wanted: HashSet<String> = tables.iter().map(|t| t.to_lowercase()).collect();
                    for node in &self.schema_nodes {
                        if let SchemaNode::Database { tables: ts, .. } = node {
                            for t in ts {
                                if let SchemaNode::Table { name, columns, .. } = t
                                    && wanted.contains(&name.to_lowercase())
                                {
                                    for c in columns {
                                        push(c.name(), &mut out, &mut seen);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            CompletionCtx::Any => {
                for name in self.schema_identifier_cache.iter() {
                    push(name, &mut out, &mut seen);
                }
            }
        }
        out.sort();
        out
    }

    /// Fire any lazy-load requests implied by `ctx` so the completion pool
    /// can fill in on the next tick. Treats stale caches (past the schema
    /// TTL) the same as missing, so long-lived sessions still see fresh
    /// data. Dedup'd via `request_schema_load`.
    pub fn lazy_load_for_context(&mut self, ctx: &CompletionCtx) {
        let ttl = self.schema_ttl;
        match ctx {
            CompletionCtx::Qualified { parent } => {
                // Parent as Database with stale / missing tables.
                let req = self.schema_nodes.iter().find_map(|n| match n {
                    SchemaNode::Database {
                        name,
                        tables_loaded_at,
                        ..
                    } if name.eq_ignore_ascii_case(parent)
                        && !crate::schema::is_fresh(*tables_loaded_at, ttl) =>
                    {
                        Some(SchemaLoadRequest::Tables { db: name.clone() })
                    }
                    _ => None,
                });
                if let Some(req) = req {
                    self.request_schema_load(req);
                    return;
                }
                // Parent as Table with stale / missing columns (bare table reference).
                let req = self.schema_nodes.iter().find_map(|n| match n {
                    SchemaNode::Database {
                        name: db, tables, ..
                    } => tables.iter().find_map(|t| match t {
                        SchemaNode::Table {
                            name,
                            columns_loaded_at,
                            ..
                        } if name.eq_ignore_ascii_case(parent)
                            && !crate::schema::is_fresh(*columns_loaded_at, ttl) =>
                        {
                            Some(SchemaLoadRequest::Columns {
                                db: db.clone(),
                                table: name.clone(),
                            })
                        }
                        _ => None,
                    }),
                    _ => None,
                });
                if let Some(req) = req {
                    self.request_schema_load(req);
                }
            }
            CompletionCtx::Column { tables } => {
                let reqs: Vec<SchemaLoadRequest> = self
                    .schema_nodes
                    .iter()
                    .flat_map(|n| match n {
                        SchemaNode::Database {
                            name: db,
                            tables: ts,
                            ..
                        } => ts
                            .iter()
                            .filter_map(|t| match t {
                                SchemaNode::Table {
                                    name,
                                    columns_loaded_at,
                                    ..
                                } if tables.iter().any(|w| w.eq_ignore_ascii_case(name))
                                    && !crate::schema::is_fresh(*columns_loaded_at, ttl) =>
                                {
                                    Some(SchemaLoadRequest::Columns {
                                        db: db.clone(),
                                        table: name.clone(),
                                    })
                                }
                                _ => None,
                            })
                            .collect::<Vec<_>>(),
                        _ => vec![],
                    })
                    .collect();
                for req in reqs {
                    self.request_schema_load(req);
                }
            }
            _ => {}
        }
    }

    /// When sidebar search matches a table/database whose children are not yet
    /// loaded, fire lazy-load requests so the filtered tree can populate on the
    /// next tick. Dedup'd via `request_schema_load`.
    pub fn lazy_load_for_schema_search(&mut self, query: &str) {
        if query.is_empty() {
            return;
        }
        let q = query.to_lowercase();
        let ttl = self.schema_ttl;
        let mut reqs: Vec<SchemaLoadRequest> = Vec::new();
        for node in &self.schema_nodes {
            let SchemaNode::Database {
                name: db,
                tables,
                tables_loaded_at,
                ..
            } = node
            else {
                continue;
            };
            let db_matches = crate::schema::label_matches(db, &q);
            if db_matches && !crate::schema::is_fresh(*tables_loaded_at, ttl) {
                reqs.push(SchemaLoadRequest::Tables { db: db.clone() });
            }
            for t in tables {
                let SchemaNode::Table {
                    name: table,
                    columns_loaded_at,
                    ..
                } = t
                else {
                    continue;
                };
                let matches = db_matches || crate::schema::label_matches(table, &q);
                if matches && !crate::schema::is_fresh(*columns_loaded_at, ttl) {
                    reqs.push(SchemaLoadRequest::Columns {
                        db: db.clone(),
                        table: table.clone(),
                    });
                }
            }
        }
        for req in reqs {
            self.request_schema_load(req);
        }
    }

    /// Collect all identifier names from the schema tree (databases, tables, columns),
    /// filter by case-insensitive prefix, deduplicate, and return sorted.
    pub fn schema_identifier_completions(&self, prefix: &str) -> Vec<String> {
        let prefix_lower = prefix.to_lowercase();
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        let mut stack: Vec<&SchemaNode> = self.schema_nodes.iter().collect();
        while let Some(node) = stack.pop() {
            let name = node.name();
            if name.to_lowercase().starts_with(&prefix_lower) && seen.insert(name.to_owned()) {
                out.push(name.to_owned());
            }
            match node {
                SchemaNode::Database { tables, .. } => stack.extend(tables.iter()),
                SchemaNode::Table { columns, .. } => stack.extend(columns.iter()),
                SchemaNode::Column { .. } => {}
            }
        }
        out.sort();
        out
    }

    pub fn visible_schema_items(&self) -> &[SchemaTreeItem] {
        &self.schema_items_cache
    }

    pub fn all_schema_items(&self) -> &[SchemaTreeItem] {
        &self.all_schema_items_cache
    }

    /// Returns path strings for every expanded node, e.g. `["mydb", "mydb/users"]`.
    pub fn schema_expanded_paths(&self) -> Vec<String> {
        collect_expanded_paths(&self.schema_nodes)
    }

    /// Expand nodes from a saved list of path strings.
    pub fn restore_schema_expanded_paths(&mut self, paths: &[String]) {
        restore_expanded_paths(&mut self.schema_nodes, paths);
        self.rebuild_schema_cache();
    }

    /// Returns the path string for the currently selected schema item, e.g. `"mydb/users/id"`.
    pub fn schema_cursor_path_string(&self) -> Option<String> {
        let item = self.schema_items_cache.get(self.schema_cursor)?;
        let path = item.node_path.clone();
        Some(path_to_string(&path, &self.schema_nodes))
    }

    /// Expands ancestor nodes then moves the cursor to the item matching `path_str`.
    /// Returns true if found.
    pub fn restore_schema_cursor_by_path(&mut self, path_str: &str) -> bool {
        expand_path(&mut self.schema_nodes, path_str);
        self.rebuild_schema_cache();
        if let Some(idx) =
            find_cursor_by_path(&self.schema_items_cache, &self.schema_nodes, path_str)
        {
            self.schema_cursor = idx;
            self.ensure_schema_cursor_visible();
            true
        } else {
            false
        }
    }

    pub fn schema_cursor_down(&mut self) {
        let max = self.schema_items_cache.len().saturating_sub(1);
        if self.schema_cursor < max {
            self.schema_cursor += 1;
        }
        self.ensure_schema_cursor_visible();
    }

    pub fn schema_cursor_up(&mut self) {
        self.schema_cursor = self.schema_cursor.saturating_sub(1);
        self.ensure_schema_cursor_visible();
    }

    pub fn schema_cursor_top(&mut self) {
        self.schema_cursor = 0;
        self.schema_scroll_offset = 0;
    }

    pub fn schema_cursor_bottom(&mut self) {
        self.rebuild_schema_cache_if_dirty();
        self.schema_cursor = self.visible_schema_items().len().saturating_sub(1);
        self.ensure_schema_cursor_visible();
    }

    /// After a cursor move, shift `schema_scroll_offset` just enough to keep
    /// the cursor inside the viewport. Needs the TUI's last-rendered viewport
    /// height, which it writes to `schema_viewport_rows`.
    pub fn ensure_schema_cursor_visible(&mut self) {
        let height = self.schema_viewport_rows.load(Ordering::Relaxed) as usize;
        if height == 0 {
            return;
        }
        let item_count = self.schema_items_cache.len();
        if self.schema_cursor < self.schema_scroll_offset {
            self.schema_scroll_offset = self.schema_cursor;
        } else if self.schema_cursor >= self.schema_scroll_offset + height {
            self.schema_scroll_offset = self.schema_cursor + 1 - height;
        }
        let max_offset = item_count.saturating_sub(height);
        if self.schema_scroll_offset > max_offset {
            self.schema_scroll_offset = max_offset;
        }
    }

    /// Scroll the schema viewport by `delta` rows (positive = down). Leaves the
    /// cursor untouched — the mouse wheel should only move the view.
    pub fn scroll_schema_viewport(&mut self, delta: i32) {
        let height = self.schema_viewport_rows.load(Ordering::Relaxed) as usize;
        let item_count = self.schema_items_cache.len();
        let max_offset = item_count.saturating_sub(height.max(1));
        let new =
            (self.schema_scroll_offset as i64 + delta as i64).clamp(0, max_offset as i64) as usize;
        self.schema_scroll_offset = new;
    }

    pub fn schema_toggle_path(&mut self, path: &[usize]) {
        toggle_node(&mut self.schema_nodes, path);
        self.maybe_lazy_load(path);
        self.rebuild_schema_cache();
    }

    pub fn schema_toggle_current(&mut self) {
        let path = self
            .schema_items_cache
            .get(self.schema_cursor)
            .map(|item| item.node_path.clone());
        if let Some(path) = path {
            toggle_node(&mut self.schema_nodes, &path);
            self.maybe_lazy_load(&path);
            self.rebuild_schema_cache();
        }
    }

    /// After toggling a node, decide whether we need to fetch its children.
    /// Only fires when the node just expanded AND we haven't loaded that level
    /// in this session yet.
    fn maybe_lazy_load(&mut self, path: &[usize]) {
        let Some(req) = self.lazy_load_request_for_path(path) else {
            return;
        };
        self.request_schema_load(req);
    }

    fn lazy_load_request_for_path(&self, path: &[usize]) -> Option<SchemaLoadRequest> {
        let ttl = self.schema_ttl;
        match path.len() {
            1 => {
                let node = self.schema_nodes.get(path[0])?;
                if let SchemaNode::Database {
                    name,
                    expanded: true,
                    tables_loaded_at,
                    ..
                } = node
                    && !crate::schema::is_fresh(*tables_loaded_at, ttl)
                {
                    return Some(SchemaLoadRequest::Tables { db: name.clone() });
                }
                None
            }
            2 => {
                let db_node = self.schema_nodes.get(path[0])?;
                let (db_name, tables) = match db_node {
                    SchemaNode::Database { name, tables, .. } => (name, tables),
                    _ => return None,
                };
                let table = tables.get(path[1])?;
                if let SchemaNode::Table {
                    name,
                    expanded: true,
                    columns_loaded_at,
                    ..
                } = table
                    && !crate::schema::is_fresh(*columns_loaded_at, ttl)
                {
                    return Some(SchemaLoadRequest::Columns {
                        db: db_name.clone(),
                        table: name.clone(),
                    });
                }
                None
            }
            _ => None,
        }
    }

    /// Send a lazy-load request. Bumps `schema_pending_loads` so the sidebar
    /// shows a spinner until the loader reports back. Dedupes against
    /// `schema_loads_inflight` so bursts of identical requests (autocomplete
    /// hot path) collapse to a single fetch; once the loader finishes the
    /// request is eligible to fire again when data goes stale.
    pub fn request_schema_load(&mut self, req: SchemaLoadRequest) {
        if self.schema_loads_inflight.contains(&req) {
            return;
        }
        if let Some(tx) = &self.schema_load_tx
            && tx.send(req.clone()).is_ok()
        {
            self.schema_loads_inflight.insert(req);
            self.schema_pending_loads += 1;
            self.schema_loading = true;
        }
    }

    /// Called by the loader task after each request finishes. Removes the
    /// request from the in-flight set so later TTL-driven refreshes can
    /// re-fire it.
    pub fn finish_schema_load(&mut self, req: &SchemaLoadRequest) {
        self.schema_loads_inflight.remove(req);
        self.schema_pending_loads = self.schema_pending_loads.saturating_sub(1);
        if self.schema_pending_loads == 0 {
            self.schema_loading = false;
        }
    }

    /// Fire refresh requests to pick up schema changes caused by a DDL
    /// statement. Granularity matches the DDL scope: a `DROP DATABASE`
    /// reloads the db list; `CREATE TABLE mydb.foo` reloads just `mydb`'s
    /// tables; `ALTER TABLE foo` refreshes that table's columns.
    /// Unqualified statements fan out across every known database.
    pub fn invalidate_for_ddl(&mut self, effect: &DdlEffect) {
        match effect {
            DdlEffect::Databases => {
                self.request_schema_load(SchemaLoadRequest::Databases);
            }
            DdlEffect::Tables { db: Some(db) } => {
                self.request_schema_load(SchemaLoadRequest::Tables { db: db.clone() });
            }
            DdlEffect::Tables { db: None } => {
                // Unknown which database — refresh the db list, plus tables
                // for every database whose table cache we've populated this
                // session (don't preemptively fetch previously-untouched dbs).
                self.request_schema_load(SchemaLoadRequest::Databases);
                let dbs: Vec<String> = self
                    .schema_nodes
                    .iter()
                    .filter_map(|n| match n {
                        SchemaNode::Database {
                            name,
                            tables_loaded_at: Some(_),
                            ..
                        } => Some(name.clone()),
                        _ => None,
                    })
                    .collect();
                for db in dbs {
                    self.request_schema_load(SchemaLoadRequest::Tables { db });
                }
            }
            DdlEffect::Columns {
                db: Some(db),
                table,
            } => {
                self.request_schema_load(SchemaLoadRequest::Columns {
                    db: db.clone(),
                    table: table.clone(),
                });
            }
            DdlEffect::Columns { db: None, table } => {
                let reqs: Vec<SchemaLoadRequest> = self
                    .schema_nodes
                    .iter()
                    .flat_map(|n| match n {
                        SchemaNode::Database {
                            name: db, tables, ..
                        } => tables
                            .iter()
                            .filter_map(|t| match t {
                                SchemaNode::Table { name, .. }
                                    if name.eq_ignore_ascii_case(table) =>
                                {
                                    Some(SchemaLoadRequest::Columns {
                                        db: db.clone(),
                                        table: name.clone(),
                                    })
                                }
                                _ => None,
                            })
                            .collect::<Vec<_>>(),
                        _ => vec![],
                    })
                    .collect();
                for req in reqs {
                    self.request_schema_load(req);
                }
            }
        }
    }

    /// Fire refresh requests for any cached schema state that has gone past
    /// its TTL. Called periodically from the TUI tick. Cheap enough to run
    /// every second — walks the tree once, only sends for stale + not
    /// already-in-flight nodes.
    pub fn refresh_stale_schema(&mut self) {
        let ttl = self.schema_ttl;
        if ttl.is_zero() {
            return;
        }
        // Root database list — fetch if never loaded or stale.
        if !crate::schema::is_fresh(self.databases_loaded_at, ttl) {
            self.request_schema_load(SchemaLoadRequest::Databases);
        }
        // For nested data, only refresh what was previously fetched. Never
        // pre-fetch tables for a db the user hasn't expanded, or columns for
        // a table we've never loaded.
        let mut reqs: Vec<SchemaLoadRequest> = Vec::new();
        for node in &self.schema_nodes {
            if let SchemaNode::Database {
                name: db,
                tables_loaded_at,
                tables,
                ..
            } = node
            {
                if tables_loaded_at.is_some() && !crate::schema::is_fresh(*tables_loaded_at, ttl) {
                    reqs.push(SchemaLoadRequest::Tables { db: db.clone() });
                }
                for t in tables {
                    if let SchemaNode::Table {
                        name: tn,
                        columns_loaded_at,
                        ..
                    } = t
                        && columns_loaded_at.is_some()
                        && !crate::schema::is_fresh(*columns_loaded_at, ttl)
                    {
                        reqs.push(SchemaLoadRequest::Columns {
                            db: db.clone(),
                            table: tn.clone(),
                        });
                    }
                }
            }
        }
        for req in reqs {
            self.request_schema_load(req);
        }
    }

    pub fn set_schema_nodes(&mut self, nodes: Vec<SchemaNode>) {
        self.schema_nodes = nodes;
        self.schema_cursor = 0;
        self.databases_loaded_at = Some(Instant::now());
        self.rebuild_schema_cache();
    }

    /// Append a batch of table nodes (no columns yet) to the named database.
    /// Does not touch other databases or reset the cursor.
    pub fn append_db_tables(&mut self, db_name: &str, tables: Vec<SchemaNode>) {
        let mut changed = false;
        for node in self.schema_nodes.iter_mut() {
            if let SchemaNode::Database {
                name, tables: t, ..
            } = node
                && name == db_name
            {
                t.extend(tables);
                changed = true;
                break;
            }
        }
        if changed {
            self.mark_schema_cache_dirty();
        }
    }

    /// Merge a fresh db-name list into the existing tree without dropping
    /// cached tables/columns. Databases in the new list that already exist are
    /// left untouched; databases missing from the new list are removed.
    /// Returns true if anything changed.
    pub fn merge_db_list(&mut self, new_names: &[String]) -> bool {
        use std::collections::HashSet;
        let new_set: HashSet<&str> = new_names.iter().map(String::as_str).collect();

        let before = self.schema_nodes.len();
        self.schema_nodes.retain(
            |n| matches!(n, SchemaNode::Database { name, .. } if new_set.contains(name.as_str())),
        );
        let mut changed = self.schema_nodes.len() != before;

        let existing: HashSet<String> = self
            .schema_nodes
            .iter()
            .filter_map(|n| match n {
                SchemaNode::Database { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        for name in new_names {
            if !existing.contains(name) {
                self.schema_nodes.push(SchemaNode::Database {
                    name: name.clone(),
                    expanded: false,
                    tables: vec![],
                    tables_loaded_at: None,
                });
                changed = true;
            }
        }

        // Preserve new-list order.
        self.schema_nodes.sort_by_key(|n| {
            new_names
                .iter()
                .position(|x| x == n.name())
                .unwrap_or(usize::MAX)
        });

        if changed {
            self.mark_schema_cache_dirty();
        }
        self.databases_loaded_at = Some(Instant::now());
        changed
    }

    /// Replace the table list for a database with a fresh set of names, reusing
    /// existing table nodes (with their columns + expansion state) when names
    /// match. Tables not in `new_names` are dropped.
    pub fn set_db_tables(&mut self, db_name: &str, new_names: &[String]) {
        use std::collections::HashMap;
        let mut changed = false;
        for node in self.schema_nodes.iter_mut() {
            if let SchemaNode::Database {
                name,
                tables: t,
                tables_loaded_at,
                ..
            } = node
                && name == db_name
            {
                let mut existing: HashMap<String, SchemaNode> = std::mem::take(t)
                    .into_iter()
                    .filter_map(|n| match &n {
                        SchemaNode::Table { name, .. } => Some((name.clone(), n)),
                        _ => None,
                    })
                    .collect();
                let merged: Vec<SchemaNode> = new_names
                    .iter()
                    .map(|name| {
                        existing.remove(name).unwrap_or_else(|| SchemaNode::Table {
                            name: name.clone(),
                            expanded: false,
                            columns: vec![],
                            columns_loaded_at: None,
                        })
                    })
                    .collect();
                *t = merged;
                *tables_loaded_at = Some(Instant::now());
                changed = true;
                break;
            }
        }
        if changed {
            self.mark_schema_cache_dirty();
        }
    }

    /// Set the columns for one specific table without touching anything else.
    pub fn set_table_columns(&mut self, db_name: &str, table_name: &str, columns: Vec<SchemaNode>) {
        let mut changed = false;
        'outer: for node in self.schema_nodes.iter_mut() {
            if let SchemaNode::Database { name, tables, .. } = node
                && name == db_name
            {
                for table in tables.iter_mut() {
                    if let SchemaNode::Table {
                        name,
                        columns: c,
                        columns_loaded_at,
                        ..
                    } = table
                        && name == table_name
                    {
                        *c = columns;
                        *columns_loaded_at = Some(Instant::now());
                        changed = true;
                        break 'outer;
                    }
                }
                break;
            }
        }
        if changed {
            self.mark_schema_cache_dirty();
        }
    }

    /// Like `set_schema_nodes` but preserves the cursor position and the
    /// expanded/collapsed state of nodes that exist in both old and new trees.
    pub fn refresh_schema_nodes(&mut self, mut nodes: Vec<SchemaNode>) {
        merge_expansion(&self.schema_nodes.clone(), &mut nodes);
        self.schema_nodes = nodes;
        self.rebuild_schema_cache();
        let max = self.schema_items_cache.len().saturating_sub(1);
        self.schema_cursor = self.schema_cursor.min(max);
    }

    pub fn set_available_connections(&mut self, conns: Vec<ConnectionConfig>) {
        self.available_connections = conns;
        self.connection_switcher_cursor = 0;
    }

    pub fn open_connection_switcher(&mut self) {
        self.show_connection_switcher = true;
        self.connection_switcher_cursor = 0;
        self.disarm_connection_delete();
    }

    pub fn close_connection_switcher(&mut self) {
        self.show_connection_switcher = false;
        self.disarm_connection_delete();
    }

    pub fn switcher_up(&mut self) {
        self.connection_switcher_cursor = self.connection_switcher_cursor.saturating_sub(1);
        self.disarm_connection_delete();
    }

    pub fn switcher_down(&mut self) {
        let max = self.available_connections.len().saturating_sub(1);
        if self.connection_switcher_cursor < max {
            self.connection_switcher_cursor += 1;
        }
        self.disarm_connection_delete();
    }

    /// Confirm the highlighted connection — returns its URL if one exists.
    /// When a delete is armed for the current selection, this commits the
    /// delete instead and returns `None` (so the caller doesn't switch).
    pub fn confirm_connection_switch(&mut self) -> Option<String> {
        if self.connection_delete_armed.is_some() {
            // Treat Enter-while-armed as the second confirmation step.
            let _ = self.delete_selected_connection();
            return None;
        }
        let url = self
            .available_connections
            .get(self.connection_switcher_cursor)
            .map(|c| c.url.clone());
        if let Some(ref u) = url {
            self.pending_reconnect = Some(u.clone());
            // Flip the sidebar to "Connecting…" synchronously so the
            // user doesn't see a stale schema tree (or the previous
            // connection's error) during the ~100ms before the
            // watcher loop picks up `pending_reconnect`.
            self.schema_connecting = true;
            self.schema_connect_error = None;
            self.show_connect_error_popup = false;
            self.schema_nodes.clear();
            self.mark_schema_cache_dirty();
        }
        self.show_connection_switcher = false;
        self.disarm_connection_delete();
        url
    }

    /// Open the connection-error details popup. No-op when there's
    /// nothing to show.
    pub fn open_connect_error_popup(&mut self) -> bool {
        if self.schema_connect_error.is_some() {
            self.show_connect_error_popup = true;
            true
        } else {
            false
        }
    }

    pub fn close_connect_error_popup(&mut self) {
        self.show_connect_error_popup = false;
    }

    /// Re-trigger the handshake for the last failed connection.
    /// Called from the schema pane's `r` binding when the placeholder
    /// is showing a "Connection failed: …" error. Returns `true` when
    /// a retry was queued; `false` if there's nothing to retry (no
    /// stored URL — e.g. the failure predates this field or the user
    /// has since switched connections).
    pub fn retry_connection(&mut self) -> bool {
        let Some(url) = self.schema_connect_url.clone() else {
            return false;
        };
        let name = self
            .available_connections
            .iter()
            .find(|c| c.url == url)
            .map(|c| c.name.clone())
            .or_else(|| self.active_connection.clone())
            .unwrap_or_else(|| url.clone());
        self.schema_connect_error = None;
        self.show_connect_error_popup = false;
        self.schema_connecting = true;
        self.pending_reconnect = Some(url);
        self.set_status(format!("Reconnecting to {name}…"));
        true
    }

    pub fn open_add_connection(&mut self) {
        self.show_add_connection = true;
        self.add_connection_name.clear();
        self.add_connection_url.clear();
        self.add_connection_name_cursor = 0;
        self.add_connection_url_cursor = 0;
        self.add_connection_field = AddConnectionField::Name;
        self.edit_connection_original_name = None;
        self.disarm_connection_delete();
    }

    pub fn open_edit_connection(&mut self) {
        let Some(conn) = self
            .available_connections
            .get(self.connection_switcher_cursor)
            .cloned()
        else {
            return;
        };
        self.show_add_connection = true;
        self.add_connection_name = conn.name.clone();
        self.add_connection_url = conn.url.clone();
        self.add_connection_name_cursor = self.add_connection_name.chars().count();
        self.add_connection_url_cursor = self.add_connection_url.chars().count();
        self.add_connection_field = AddConnectionField::Name;
        self.edit_connection_original_name = Some(conn.name);
        self.disarm_connection_delete();
    }

    pub fn close_add_connection(&mut self) {
        self.show_add_connection = false;
        self.edit_connection_original_name = None;
    }

    /// Drop any in-flight delete arming and clear its status hint.
    pub fn disarm_connection_delete(&mut self) {
        if self.connection_delete_armed.take().is_some() {
            self.clear_status();
        }
    }

    pub fn open_help(&mut self) {
        self.show_help = true;
        self.help_scroll = 0;
    }

    pub fn close_help(&mut self) {
        self.show_help = false;
        self.help_scroll = 0;
    }

    pub fn add_connection_tab(&mut self) {
        self.add_connection_field = match self.add_connection_field {
            AddConnectionField::Name => AddConnectionField::Url,
            AddConnectionField::Url => AddConnectionField::Name,
        };
    }

    fn add_connection_active(&mut self) -> (&mut String, &mut usize) {
        match self.add_connection_field {
            AddConnectionField::Name => (
                &mut self.add_connection_name,
                &mut self.add_connection_name_cursor,
            ),
            AddConnectionField::Url => (
                &mut self.add_connection_url,
                &mut self.add_connection_url_cursor,
            ),
        }
    }

    pub fn add_connection_type_char(&mut self, ch: char) {
        let (text, cur) = self.add_connection_active();
        let byte = text
            .char_indices()
            .nth(*cur)
            .map(|(b, _)| b)
            .unwrap_or(text.len());
        text.insert(byte, ch);
        *cur += 1;
    }

    pub fn add_connection_backspace(&mut self) {
        let (text, cur) = self.add_connection_active();
        if *cur == 0 {
            return;
        }
        let end = text
            .char_indices()
            .nth(*cur)
            .map(|(b, _)| b)
            .unwrap_or(text.len());
        let start = text
            .char_indices()
            .nth(*cur - 1)
            .map(|(b, _)| b)
            .unwrap_or(0);
        text.replace_range(start..end, "");
        *cur -= 1;
    }

    pub fn add_connection_delete(&mut self) {
        let (text, cur) = self.add_connection_active();
        let count = text.chars().count();
        if *cur >= count {
            return;
        }
        let start = text
            .char_indices()
            .nth(*cur)
            .map(|(b, _)| b)
            .unwrap_or(text.len());
        let end = text
            .char_indices()
            .nth(*cur + 1)
            .map(|(b, _)| b)
            .unwrap_or(text.len());
        text.replace_range(start..end, "");
    }

    pub fn add_connection_left(&mut self) {
        let (_, cur) = self.add_connection_active();
        *cur = cur.saturating_sub(1);
    }

    pub fn add_connection_right(&mut self) {
        let (text, cur) = self.add_connection_active();
        let count = text.chars().count();
        if *cur < count {
            *cur += 1;
        }
    }

    pub fn add_connection_home(&mut self) {
        let (_, cur) = self.add_connection_active();
        *cur = 0;
    }

    pub fn add_connection_end(&mut self) {
        let (text, cur) = self.add_connection_active();
        *cur = text.chars().count();
    }

    /// Validate, persist, and register the connection. Handles both add and edit.
    pub fn save_new_connection(&mut self) -> anyhow::Result<()> {
        let name = self.add_connection_name.trim().to_string();
        let url = self.add_connection_url.trim().to_string();
        if name.is_empty() || url.is_empty() {
            anyhow::bail!("Name and URL are required");
        }
        if let Err(reason) = validate_connection_url(&url) {
            anyhow::bail!("Bad URL: {reason}");
        }
        if let Some(ref original) = self.edit_connection_original_name.clone() {
            // Editing: rename file if name changed, then overwrite
            if *original != name {
                crate::config::delete_connection(original)?;
            }
            crate::config::save_connection(&name, &url)?;
            // Update in-memory entry
            if let Some(entry) = self
                .available_connections
                .iter_mut()
                .find(|c| c.name == *original)
            {
                entry.name = name.clone();
                entry.url = url.clone();
            }
        } else {
            crate::config::save_connection(&name, &url)?;
            self.available_connections
                .push(crate::config::ConnectionConfig {
                    name: name.clone(),
                    url: url.clone(),
                });
        }
        self.show_add_connection = false;
        self.edit_connection_original_name = None;
        // Plaintext-password heads-up. Save still succeeded; just
        // surface the risk in the status bar.
        if url_has_plaintext_password(&url) {
            self.set_status(format!(
                "⚠ password stored in plaintext at ~/.config/sqeel/conns/{name}.toml — chmod 0600 or use `{}://user@host`",
                url.split_once(':').map(|(s, _)| s).unwrap_or("scheme"),
            ));
        }
        Ok(())
    }

    /// Two-step delete entry point. First `d` arms the delete on the
    /// highlighted connection and surfaces a status-bar confirmation
    /// hint ("Delete `{name}`? d/Enter to confirm."). Second `d`
    /// (called while armed for the same name) commits the delete.
    /// Movement / Esc / opening another modal disarms (see
    /// `disarm_connection_delete`).
    pub fn delete_selected_connection(&mut self) -> anyhow::Result<()> {
        let Some(conn) = self
            .available_connections
            .get(self.connection_switcher_cursor)
            .cloned()
        else {
            return Ok(());
        };
        // Already armed for this name → second `d` commits.
        if self.connection_delete_armed.as_deref() == Some(conn.name.as_str()) {
            crate::config::delete_connection(&conn.name)?;
            self.available_connections
                .remove(self.connection_switcher_cursor);
            let max = self.available_connections.len().saturating_sub(1);
            self.connection_switcher_cursor = self.connection_switcher_cursor.min(max);
            self.connection_delete_armed = None;
            self.clear_status();
            return Ok(());
        }
        // Otherwise arm and prompt.
        self.connection_delete_armed = Some(conn.name.clone());
        self.set_status(format!(
            "Delete `{}`? d/Enter to confirm, any other key cancels.",
            conn.name
        ));
        Ok(())
    }

    /// Try to send a single query to the active executor. Returns false if not connected.
    pub fn send_query(&self, query: String, tab_idx: usize) -> bool {
        self.send_query_request(QueryRequest::Single(query, tab_idx))
    }

    /// Try to send a batch of queries to the active executor. Returns false if not connected.
    pub fn send_batch(&self, queries: Vec<String>, start_idx: usize) -> bool {
        self.send_query_request(QueryRequest::Batch(queries, start_idx))
    }

    fn send_query_request(&self, req: QueryRequest) -> bool {
        if let Some(tx) = &self.query_tx {
            tx.try_send(req).is_ok()
        } else {
            false
        }
    }

    /// Load the global scratch-query tabs from disk.
    /// Sets `tab_content_pending` so the TUI loads the first tab into the editor.
    pub fn load_tabs(&mut self) {
        let names = persistence::list_queries().unwrap_or_default();
        if names.is_empty() {
            match persistence::next_scratch_name() {
                Ok(name) => {
                    let _ = persistence::save_query(&name, "");
                    self.tabs = vec![TabEntry::open(name, String::new())];
                }
                Err(_) => {
                    self.tabs = vec![];
                    self.tab_content_pending = Some(String::new());
                    self.active_tab = 0;
                    return;
                }
            }
        } else {
            self.tabs = names.into_iter().map(TabEntry::new).collect();
        }
        self.active_tab = 0;
        if let Some(tab) = self.tabs.first_mut() {
            tab.last_accessed = Some(Instant::now());
            if let Some(content) = tab.content.clone() {
                // Seeded from disk via main() (scratch_xxx.sql already
                // loaded). Publish directly; no deferred read needed.
                self.tab_content_pending = Some(content);
            } else {
                // Defer the disk read so the TUI's spawn_blocking
                // handler runs it off the render loop. Show an empty
                // buffer immediately.
                let name = tab.name.clone();
                self.tab_content_pending = Some(String::new());
                self.pending_tab_load = Some(PendingTabLoad { tab_index: 0, name });
            }
        }
    }

    /// Switch to the tab at `idx`, saving current content first.
    /// Sets `tab_content_pending` for the TUI to drain.
    pub fn switch_to_tab(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        // Persist current tab content in memory before leaving. Skip when the
        // TUI hasn't synced the live editor into `editor_content` yet —
        // otherwise we'd clobber the freshly loaded tab content with the
        // default empty buffer during startup restoration.
        if self.editor_content_synced
            && let Some(tab) = self.tabs.get_mut(self.active_tab)
        {
            tab.content = Some((*self.editor_content).clone());
        }
        self.active_tab = idx;
        let (content, cursor, needs_load) = if let Some(tab) = self.tabs.get_mut(idx) {
            tab.last_accessed = Some(Instant::now());
            if let Some(ref c) = tab.content {
                (c.clone(), tab.cursor, None)
            } else {
                // Cold tab: show empty immediately so the editor isn't
                // stuck on the previous tab, and queue the disk read
                // for the TUI to run off the render loop.
                let load = PendingTabLoad {
                    tab_index: idx,
                    name: tab.name.clone(),
                };
                (String::new(), tab.cursor, Some(load))
            }
        } else {
            (String::new(), None, None)
        };
        self.tab_content_pending = Some(content);
        self.tab_cursor_pending = cursor;
        if let Some(load) = needs_load {
            self.pending_tab_load = Some(load);
        }
    }

    /// Called by the TUI when a deferred tab-content load completes.
    /// Stale results (the user switched elsewhere while the load ran)
    /// are still cached onto their tab but only published to the
    /// editor when `tab_index` matches the currently active tab.
    pub fn apply_loaded_tab_content(&mut self, tab_index: usize, content: String) {
        if let Some(tab) = self.tabs.get_mut(tab_index) {
            tab.content = Some(content.clone());
        }
        if tab_index == self.active_tab {
            self.tab_content_pending = Some(content);
            self.tab_cursor_pending = self.tabs.get(tab_index).and_then(|t| t.cursor);
        }
    }

    /// Update the active tab's stored cursor `(row, col)` (0-based). Called
    /// frequently from the TUI so the in-memory + persisted cursor stays fresh.
    pub fn update_active_tab_cursor(&mut self, cursor: (usize, usize)) {
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.cursor = Some(cursor);
        }
    }

    /// Snapshot of `(tab_name, row, col)` for every tab with a known cursor.
    /// Used by the session writer.
    pub fn tab_cursor_snapshot(&self) -> Vec<(String, usize, usize)> {
        self.tabs
            .iter()
            .filter_map(|t| t.cursor.map(|(r, c)| (t.name.clone(), r, c)))
            .collect()
    }

    /// Apply persisted cursors (from session.toml) onto matching tabs by name.
    /// Also seeds `tab_cursor_pending` for the active tab so the editor jumps
    /// to the saved position on startup.
    pub fn apply_tab_cursors(&mut self, cursors: &[(String, usize, usize)]) {
        for (name, r, c) in cursors {
            if let Some(tab) = self.tabs.iter_mut().find(|t| &t.name == name) {
                tab.cursor = Some((*r, *c));
            }
        }
        if let Some(tab) = self.tabs.get(self.active_tab)
            && let Some(cur) = tab.cursor
        {
            self.tab_cursor_pending = Some(cur);
        }
    }

    pub fn next_tab(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        let next = (self.active_tab + 1) % self.tabs.len();
        self.switch_to_tab(next);
    }

    pub fn prev_tab(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        let prev = if self.active_tab == 0 {
            self.tabs.len() - 1
        } else {
            self.active_tab - 1
        };
        self.switch_to_tab(prev);
    }

    /// Open a new scratch file and switch to it.
    /// Rename the active tab's on-disk file and in-memory entry.
    /// `new_name` is sanitized and forced to end in `.sql`.
    pub fn rename_active_tab(&mut self, new_name: &str) -> anyhow::Result<()> {
        let trimmed = new_name.trim();
        if trimmed.is_empty() {
            anyhow::bail!("Name cannot be empty");
        }
        let stem = trimmed.strip_suffix(".sql").unwrap_or(trimmed);
        if !stem
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            anyhow::bail!("Name may only contain letters, digits, '-', '_', '.'");
        }
        let final_name = format!("{stem}.sql");
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            anyhow::bail!("No active tab");
        };
        if tab.name == final_name {
            return Ok(());
        }
        persistence::rename_query(&tab.name, &final_name)?;
        tab.name = final_name;
        Ok(())
    }

    /// Delete the active tab's on-disk file and drop the in-memory entry.
    /// If this was the last tab, a fresh empty scratch tab is created so the
    /// editor always has something to edit.
    pub fn delete_active_tab(&mut self) -> anyhow::Result<()> {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            anyhow::bail!("No active tab");
        };
        let name = tab.name.clone();
        persistence::delete_query(&name)?;
        self.tabs.remove(self.active_tab);
        if self.tabs.is_empty() {
            self.new_tab();
        } else {
            let new_idx = self.active_tab.min(self.tabs.len() - 1);
            self.active_tab = self.tabs.len(); // force switch_to_tab to reload
            self.switch_to_tab(new_idx);
        }
        Ok(())
    }

    pub fn new_tab(&mut self) {
        // Save current tab content before leaving
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.content = Some((*self.editor_content).clone());
        }
        if let Ok(name) = persistence::next_scratch_name() {
            let _ = persistence::save_query(&name, "");
            self.tabs.push(TabEntry::open(name, String::new()));
            self.active_tab = self.tabs.len() - 1;
            self.tab_content_pending = Some(String::new());
        }
    }

    /// Evict content of cold tabs (not active, not accessed within 5 min) to free RAM.
    pub fn evict_cold_tabs(&mut self) {
        let cutoff = std::time::Duration::from_secs(300);
        let now = Instant::now();
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            if i == self.active_tab {
                continue;
            }
            if let Some(accessed) = tab.last_accessed
                && now.duration_since(accessed) > cutoff
            {
                tab.content = None;
            }
        }
    }

    /// Mark the active tab as having unsaved changes.  The live buffer
    /// lives in `editor_content`; we don't duplicate it into `tab.content`
    /// on every keystroke — `switch_to_tab` / `save_active_tab` pull from
    /// `editor_content` when the cached copy is actually needed.
    pub fn mark_active_dirty(&mut self) {
        if !self.editor_content_synced {
            return;
        }
        if self.tabs.is_empty() {
            let Ok(name) = persistence::next_scratch_name() else {
                return;
            };
            let mut entry = TabEntry::open(name, String::new());
            entry.content = None;
            entry.dirty = true;
            self.tabs.push(entry);
            self.active_tab = 0;
            return;
        }
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.last_accessed = Some(Instant::now());
            tab.dirty = true;
        }
    }

    /// Prepare a disk-write for the active tab. Updates the in-memory
    /// `tab.content` snapshot so it stays in sync with what we're
    /// about to write, but does NOT clear the tab's dirty flag — the
    /// caller calls [`mark_tab_saved`] once the disk write lands, so
    /// a failed write doesn't silently drop the user's unsaved state.
    ///
    /// Splits the sync disk I/O off from the state mutation so the
    /// TUI can ship [`PendingSave::commit`] to `spawn_blocking` and
    /// keep the render loop responsive during large saves.
    pub fn prepare_save_active_tab(&mut self) -> std::io::Result<PendingSave> {
        if self.tabs.is_empty() {
            let name = persistence::next_scratch_name()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let content: String = (*self.editor_content).clone();
            self.tabs
                .push(TabEntry::open(name.clone(), content.clone()));
            self.active_tab = 0;
            return Ok(PendingSave {
                name,
                content,
                tab_index: Some(0),
            });
        }
        let idx = self.active_tab;
        let tab = self
            .tabs
            .get_mut(idx)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no active tab"))?;
        let content: String = if self.editor_content_synced {
            (*self.editor_content).clone()
        } else {
            tab.content.clone().unwrap_or_default()
        };
        tab.content = Some(content.clone());
        tab.last_accessed = Some(Instant::now());
        let name = tab.name.clone();
        Ok(PendingSave {
            name,
            content,
            tab_index: Some(idx),
        })
    }

    /// Clear the dirty flag on the tab that [`prepare_save_active_tab`]
    /// or [`prepare_save_all_dirty`] scheduled for a write, now that
    /// the disk write has landed successfully.
    pub fn mark_tab_saved(&mut self, tab_index: usize) {
        if let Some(tab) = self.tabs.get_mut(tab_index) {
            tab.dirty = false;
        }
    }

    /// Prepare disk writes for every dirty tab. Mirrors
    /// [`prepare_save_active_tab`] but returns one [`PendingSave`] per
    /// dirty tab. Caller commits each write on a blocking task then
    /// calls [`mark_tab_saved`] for the successes.
    pub fn prepare_save_all_dirty(&mut self) -> Vec<PendingSave> {
        let mut out = Vec::new();
        let active = self.active_tab;
        let synced = self.editor_content_synced;
        let fresh_active_content: Option<String> = if synced {
            Some((*self.editor_content).clone())
        } else {
            None
        };
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            if !tab.dirty {
                continue;
            }
            let content = if i == active {
                fresh_active_content
                    .as_ref()
                    .cloned()
                    .or_else(|| tab.content.clone())
            } else {
                tab.content.clone()
            };
            let Some(content) = content else {
                continue;
            };
            tab.content = Some(content.clone());
            out.push(PendingSave {
                name: tab.name.clone(),
                content,
                tab_index: Some(i),
            });
        }
        out
    }

    /// Synchronous convenience wrapper — prepares + commits + marks
    /// saved in one shot. Used by tests and any non-async caller; the
    /// TUI itself prefers the split form so the disk write can run on
    /// a blocking task.
    pub fn save_active_tab(&mut self) -> std::io::Result<String> {
        let pending = self.prepare_save_active_tab()?;
        pending.commit()?;
        if let Some(idx) = pending.tab_index {
            self.mark_tab_saved(idx);
        }
        Ok(pending.name)
    }

    /// Synchronous wrapper mirroring [`prepare_save_all_dirty`] +
    /// inline commit. Returns the names of tabs whose disk write
    /// failed.
    pub fn save_all_dirty(&mut self) -> Vec<String> {
        let pending = self.prepare_save_all_dirty();
        let mut failed = Vec::new();
        for p in pending {
            match p.commit() {
                Ok(()) => {
                    if let Some(idx) = p.tab_index {
                        self.mark_tab_saved(idx);
                    }
                }
                Err(_) => failed.push(p.name.clone()),
            }
        }
        failed
    }

    /// Names of tabs with unsaved in-memory changes.
    pub fn dirty_tab_names(&self) -> Vec<String> {
        self.tabs
            .iter()
            .filter(|t| t.dirty)
            .map(|t| t.name.clone())
            .collect()
    }

    /// True if any open tab has unsaved changes.
    pub fn any_dirty(&self) -> bool {
        self.tabs.iter().any(|t| t.dirty)
    }

    /// Persist a successful query result to disk (errors are never stored).
    /// Returns the on-disk filename on success, so the caller can record it on
    /// the owning `ResultsTab` for session restoration.
    pub fn persist_result(&self, query: &str, result: &QueryResult) -> Option<String> {
        let slug =
            persistence::sanitize_conn_slug(self.active_connection.as_deref().unwrap_or("default"));
        persistence::save_result(&slug, query, result).ok()
    }

    /// Record a query in history (dedup consecutive identical entries, max 100).
    pub fn push_history(&mut self, query: &str) {
        let trimmed = query.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        if self.query_history.last().map(|s| s.as_str()) != Some(&trimmed) {
            self.query_history.push(trimmed);
        }
        if self.query_history.len() > 100 {
            self.query_history.remove(0);
        }
        self.history_cursor = None;
    }

    /// Move cursor back in history and return that query, if available.
    pub fn history_prev(&mut self) -> Option<&str> {
        if self.query_history.is_empty() {
            return None;
        }
        let max = self.query_history.len() - 1;
        let idx = match self.history_cursor {
            None => max,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_cursor = Some(idx);
        self.query_history.get(idx).map(|s| s.as_str())
    }

    /// Move cursor forward in history; returns None when past the end.
    pub fn history_next(&mut self) -> Option<&str> {
        let idx = self.history_cursor? + 1;
        if idx >= self.query_history.len() {
            self.history_cursor = None;
            return None;
        }
        self.history_cursor = Some(idx);
        self.query_history.get(idx).map(|s| s.as_str())
    }
}

/// True when `url`'s userinfo carries a non-empty password segment
/// (`scheme://user:password@host/...`). Drives the password-warning
/// toast on save. Query-string passwords (`?password=foo`) aren't
/// detected — sqlx accepts them but they're rare in practice and
/// adding a parser pulls in a dependency for one warning.
fn url_has_plaintext_password(url: &str) -> bool {
    let Some((_, rest)) = url.split_once("://") else {
        return false;
    };
    // Authority ends at the first `/`, `?`, or `#`. Anything past
    // those bytes isn't userinfo.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let Some((userinfo, _)) = authority.rsplit_once('@') else {
        return false;
    };
    let Some((_, password)) = userinfo.split_once(':') else {
        return false;
    };
    !password.is_empty()
}

/// Shape-check the user's connection URL before persisting. Catches
/// typos like `mysql:/host` or empty bodies (`mysql://`) at form-save
/// time so the failure surfaces inline instead of seconds later when
/// the async sqlx handshake errors. Stays intentionally shallow —
/// authoritative validation is sqlx's own connect call.
fn validate_connection_url(url: &str) -> Result<(), String> {
    const SCHEMES: &[&str] = &["mysql", "mariadb", "postgres", "postgresql", "sqlite"];
    let Some((scheme, rest)) = url.split_once(':') else {
        return Err("missing scheme (try `mysql://user@host/db`)".into());
    };
    if !SCHEMES.contains(&scheme) {
        return Err(format!(
            "unsupported scheme `{scheme}` — supported: {}",
            SCHEMES.join(", ")
        ));
    }
    // Network drivers want `scheme://...`; sqlite also accepts the
    // `sqlite::memory:` form so it skips the `//` requirement.
    if scheme != "sqlite" && !rest.starts_with("//") {
        return Err(format!("expected `{scheme}://...`"));
    }
    let body = rest.trim_start_matches("//");
    if body.is_empty() || body == "/" {
        return Err("URL has no host or path".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state() {
        let state = AppState::new();
        let s = state.lock().unwrap();
        assert_eq!(s.keybinding_mode, KeybindingMode::Vim);
        assert_eq!(s.vim_mode, VimMode::Normal);
        assert_eq!(s.focus, Focus::Editor);
        assert!(matches!(s.results(), ResultsPane::Empty));
        assert_eq!(s.editor_ratio, 1.0);
    }

    #[test]
    fn results_shrinks_editor() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_results(QueryResult {
            columns: vec!["id".into()],
            rows: vec![vec!["1".into()]],
            col_widths: vec![],
        });
        assert_eq!(s.editor_ratio, 0.5);
        assert!(matches!(s.results(), ResultsPane::Results(_)));
    }

    #[test]
    fn error_shrinks_editor() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_error("syntax error".into());
        assert_eq!(s.editor_ratio, 0.5);
        assert!(matches!(s.results(), ResultsPane::Error(_)));
    }

    #[test]
    fn scroll_results_bounds() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_results(QueryResult {
            columns: vec!["id".into()],
            rows: vec![vec!["1".into()], vec!["2".into()], vec!["3".into()]],
            col_widths: vec![],
        });
        assert_eq!(s.results_scroll(), 0);
        s.scroll_results_down();
        assert_eq!(s.results_scroll(), 1);
        s.scroll_results_down();
        assert_eq!(s.results_scroll(), 2);
        // Cannot go past last row
        s.scroll_results_down();
        assert_eq!(s.results_scroll(), 2);
        s.scroll_results_up();
        assert_eq!(s.results_scroll(), 1);
    }

    #[test]
    fn results_visual_line_yank_tsv() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_results(QueryResult {
            columns: vec!["a".into(), "b".into(), "c".into()],
            rows: vec![
                vec!["1".into(), "2".into(), "3".into()],
                vec!["4".into(), "5".into(), "6".into()],
                vec!["7".into(), "8".into(), "9".into()],
            ],
            col_widths: vec![],
        });
        s.result_tabs[0].cursor = ResultsCursor::Cell { row: 0, col: 1 };
        s.results_enter_selection(ResultsSelectionMode::Line);
        s.result_tabs[0].cursor = ResultsCursor::Cell { row: 1, col: 2 };
        let (text, label) = s.results_selection_yank().unwrap();
        assert_eq!(label, "Rows");
        assert_eq!(text, "1\t2\t3\n4\t5\t6");
    }

    #[test]
    fn results_visual_block_yank_rectangle() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_results(QueryResult {
            columns: vec!["a".into(), "b".into(), "c".into()],
            rows: vec![
                vec!["1".into(), "2".into(), "3".into()],
                vec!["4".into(), "5".into(), "6".into()],
                vec!["7".into(), "8".into(), "9".into()],
            ],
            col_widths: vec![],
        });
        s.result_tabs[0].cursor = ResultsCursor::Cell { row: 0, col: 1 };
        s.results_enter_selection(ResultsSelectionMode::Block);
        s.result_tabs[0].cursor = ResultsCursor::Cell { row: 2, col: 2 };
        let (text, label) = s.results_selection_yank().unwrap();
        assert_eq!(label, "Block");
        assert_eq!(text, "2\t3\n5\t6\n8\t9");
    }

    #[test]
    fn results_selection_bounds_order() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_results(QueryResult {
            columns: vec!["a".into(), "b".into(), "c".into()],
            rows: vec![
                vec!["1".into(), "2".into(), "3".into()],
                vec!["4".into(), "5".into(), "6".into()],
            ],
            col_widths: vec![],
        });
        s.result_tabs[0].cursor = ResultsCursor::Cell { row: 1, col: 2 };
        s.results_enter_selection(ResultsSelectionMode::Block);
        // Move cursor back up-left.
        s.result_tabs[0].cursor = ResultsCursor::Cell { row: 0, col: 0 };
        let (top, bot, left, right) = s.results_selection_bounds().unwrap();
        assert_eq!((top, bot, left, right), (0, 1, 0, 2));
    }

    #[test]
    fn results_clear_selection() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_results(QueryResult {
            columns: vec!["x".into()],
            rows: vec![vec!["1".into()]],
            col_widths: vec![],
        });
        s.result_tabs[0].cursor = ResultsCursor::Cell { row: 0, col: 0 };
        s.results_enter_selection(ResultsSelectionMode::Line);
        assert!(s.result_tabs[0].selection.is_some());
        s.results_clear_selection();
        assert!(s.result_tabs[0].selection.is_none());
    }

    fn seeded_results() -> Arc<Mutex<AppState>> {
        let state = AppState::new();
        {
            let mut s = state.lock().unwrap();
            s.set_results(QueryResult {
                columns: vec!["a".into(), "b".into(), "c".into()],
                rows: (0..5)
                    .map(|i| vec![format!("{i}a"), format!("{i}b"), format!("{i}c")])
                    .collect(),
                col_widths: vec![],
            });
        }
        state
    }

    #[test]
    fn results_cursor_first_and_last_row_from_cell() {
        let state = seeded_results();
        let mut s = state.lock().unwrap();
        s.result_tabs[0].cursor = ResultsCursor::Cell { row: 2, col: 1 };
        s.results_cursor_first_row();
        assert_eq!(
            s.result_tabs[0].cursor,
            ResultsCursor::Cell { row: 0, col: 1 }
        );
        s.results_cursor_last_row();
        assert_eq!(
            s.result_tabs[0].cursor,
            ResultsCursor::Cell { row: 4, col: 1 }
        );
    }

    #[test]
    fn results_row_start_and_end_clamp_to_columns() {
        let state = seeded_results();
        let mut s = state.lock().unwrap();
        s.result_tabs[0].cursor = ResultsCursor::Cell { row: 2, col: 1 };
        s.results_cursor_row_start();
        assert_eq!(
            s.result_tabs[0].cursor,
            ResultsCursor::Cell { row: 2, col: 0 }
        );
        s.results_cursor_row_end();
        assert_eq!(
            s.result_tabs[0].cursor,
            ResultsCursor::Cell { row: 2, col: 2 }
        );
    }

    #[test]
    fn results_find_forward_jumps_to_first_match() {
        let state = seeded_results();
        let mut s = state.lock().unwrap();
        s.result_tabs[0].cursor = ResultsCursor::Cell { row: 0, col: 0 };
        assert!(s.results_find("2b", true, false));
        assert_eq!(
            s.result_tabs[0].cursor,
            ResultsCursor::Cell { row: 2, col: 1 }
        );
    }

    #[test]
    fn results_find_skip_current_walks_past_active_match() {
        let state = seeded_results();
        let mut s = state.lock().unwrap();
        // "a" appears in every row, col 0. Seed cursor on a match and
        // confirm skip_current advances to the next one.
        s.result_tabs[0].cursor = ResultsCursor::Cell { row: 1, col: 0 };
        assert!(s.results_find("a", true, true));
        assert_eq!(
            s.result_tabs[0].cursor,
            ResultsCursor::Cell { row: 2, col: 0 }
        );
    }

    #[test]
    fn results_find_backward_wraps_around() {
        let state = seeded_results();
        let mut s = state.lock().unwrap();
        s.result_tabs[0].cursor = ResultsCursor::Cell { row: 0, col: 0 };
        // Backward from (0,0) should wrap to the last match in the grid.
        assert!(s.results_find("4c", false, false));
        assert_eq!(
            s.result_tabs[0].cursor,
            ResultsCursor::Cell { row: 4, col: 2 }
        );
    }

    #[test]
    fn results_find_is_case_insensitive() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_results(QueryResult {
            columns: vec!["x".into()],
            rows: vec![vec!["Alpha".into()], vec!["beta".into()]],
            col_widths: vec![],
        });
        s.result_tabs[0].cursor = ResultsCursor::Cell { row: 0, col: 0 };
        assert!(s.results_find("BETA", true, true));
        assert_eq!(
            s.result_tabs[0].cursor,
            ResultsCursor::Cell { row: 1, col: 0 }
        );
    }

    #[test]
    fn parse_hover_table_picks_up_pipe_grid() {
        let text = "\
description preamble

| name | type |
| ---- | ---- |
| id   | int  |
| name | text |

trailing prose ignored";
        let t = AppState::parse_hover_table(text).expect("table parsed");
        assert_eq!(t.columns, vec!["name", "type"]);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[0], vec!["id", "int"]);
        assert_eq!(t.rows[1], vec!["name", "text"]);
    }

    #[test]
    fn parse_hover_table_strips_inline_backticks() {
        let text = "| name | type |\n| ---- | ---- |\n| `id` | `int` |\n| **pk** | _bool_ |";
        let t = AppState::parse_hover_table(text).expect("table parsed");
        assert_eq!(t.rows[0], vec!["id", "int"]);
        assert_eq!(t.rows[1], vec!["pk", "bool"]);
    }

    #[test]
    fn parse_hover_table_none_for_plain_text() {
        assert!(AppState::parse_hover_table("just words").is_none());
        assert!(AppState::parse_hover_table("| only | header |").is_none());
    }

    #[test]
    fn open_hover_table_shifts_focus() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.focus = Focus::Editor;
        let t = QueryResult {
            columns: vec!["a".into()],
            rows: vec![vec!["x".into()]],
            col_widths: vec![3],
        };
        s.open_hover_table(t);
        assert_eq!(s.focus, Focus::Hover);
        assert_eq!(s.hover_prev_focus, Some(Focus::Editor));
        assert!(s.hover_table.is_some());
    }

    #[test]
    fn close_hover_restores_focus() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.focus = Focus::Editor;
        let t = QueryResult {
            columns: vec!["a".into()],
            rows: vec![vec!["x".into()]],
            col_widths: vec![3],
        };
        s.open_hover_table(t);
        s.close_hover();
        assert_eq!(s.focus, Focus::Editor);
        assert!(s.hover_table.is_none());
    }

    #[test]
    fn hover_cursor_move_clamps_to_grid() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        let t = QueryResult {
            columns: vec!["a".into(), "b".into()],
            rows: vec![vec!["1".into(), "2".into()], vec!["3".into(), "4".into()]],
            col_widths: vec![3, 3],
        };
        s.open_hover_table(t);
        s.hover_cursor_move(10, 10);
        assert_eq!(s.hover_cursor, ResultsCursor::Cell { row: 1, col: 1 });
        s.hover_cursor_move(-10, -10);
        assert_eq!(s.hover_cursor, ResultsCursor::Cell { row: 0, col: 0 });
    }

    #[test]
    fn hover_yank_cell_returns_value_under_cursor() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        let t = QueryResult {
            columns: vec!["a".into(), "b".into()],
            rows: vec![vec!["x".into(), "y".into()]],
            col_widths: vec![3, 3],
        };
        s.open_hover_table(t);
        s.hover_cursor_move(0, 1);
        let (text, label) = s.hover_yank().unwrap();
        assert_eq!(label, "Cell");
        assert_eq!(text, "y");
    }

    #[test]
    fn results_find_no_match_returns_false() {
        let state = seeded_results();
        let mut s = state.lock().unwrap();
        s.result_tabs[0].cursor = ResultsCursor::Cell { row: 0, col: 0 };
        assert!(!s.results_find("zzz", true, false));
        // Cursor unchanged on miss.
        assert_eq!(
            s.result_tabs[0].cursor,
            ResultsCursor::Cell { row: 0, col: 0 }
        );
    }

    #[test]
    fn results_first_row_from_header_lands_on_body() {
        let state = seeded_results();
        let mut s = state.lock().unwrap();
        s.result_tabs[0].cursor = ResultsCursor::Header(2);
        s.results_cursor_first_row();
        assert_eq!(
            s.result_tabs[0].cursor,
            ResultsCursor::Cell { row: 0, col: 2 }
        );
    }

    #[test]
    fn scroll_schema_viewport_moves_offset() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        // Populate a deep-enough tree that there's something to scroll.
        s.set_schema_nodes(
            (0..20)
                .map(|i| SchemaNode::Database {
                    name: format!("db{i}"),
                    expanded: false,
                    tables: vec![],
                    tables_loaded_at: None,
                })
                .collect(),
        );
        s.schema_viewport_rows.store(5, Ordering::Relaxed);
        assert_eq!(s.schema_scroll_offset, 0);
        s.scroll_schema_viewport(3);
        assert_eq!(s.schema_scroll_offset, 3);
        s.scroll_schema_viewport(-1);
        assert_eq!(s.schema_scroll_offset, 2);
        // Clamped to max_offset = 20 - 5 = 15.
        s.scroll_schema_viewport(100);
        assert_eq!(s.schema_scroll_offset, 15);
    }

    #[test]
    fn dismiss_restores_editor() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_error("oops".into());
        s.dismiss_results();
        assert_eq!(s.editor_ratio, 1.0);
        assert!(matches!(s.results(), ResultsPane::Empty));
    }

    #[test]
    fn history_push_and_recall() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.push_history("SELECT 1");
        s.push_history("SELECT 2");
        s.push_history("SELECT 3");
        assert_eq!(s.query_history.len(), 3);
        assert_eq!(s.history_prev(), Some("SELECT 3"));
        assert_eq!(s.history_prev(), Some("SELECT 2"));
        assert_eq!(s.history_prev(), Some("SELECT 1"));
        // At start — stays at first
        assert_eq!(s.history_prev(), Some("SELECT 1"));
        assert_eq!(s.history_next(), Some("SELECT 2"));
        assert_eq!(s.history_next(), Some("SELECT 3"));
        // Past end
        assert_eq!(s.history_next(), None);
        assert_eq!(s.history_cursor, None);
    }

    #[test]
    fn history_deduplicates_consecutive() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.push_history("SELECT 1");
        s.push_history("SELECT 1");
        s.push_history("SELECT 1");
        assert_eq!(s.query_history.len(), 1);
    }

    #[test]
    fn history_max_100() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        for i in 0..110 {
            s.push_history(&format!("SELECT {i}"));
        }
        assert_eq!(s.query_history.len(), 100);
    }

    fn sample_schema() -> Vec<SchemaNode> {
        vec![
            SchemaNode::Database {
                name: "deepci_maindb".into(),
                expanded: false,
                tables_loaded_at: Some(std::time::Instant::now()),
                tables: vec![
                    SchemaNode::Table {
                        name: "users".into(),
                        expanded: false,
                        columns_loaded_at: Some(std::time::Instant::now()),
                        columns: vec![
                            SchemaNode::Column {
                                name: "id".into(),
                                type_name: "INT".into(),
                                nullable: false,
                                is_pk: true,
                            },
                            SchemaNode::Column {
                                name: "email".into(),
                                type_name: "TEXT".into(),
                                nullable: false,
                                is_pk: false,
                            },
                        ],
                    },
                    SchemaNode::Table {
                        name: "orders".into(),
                        expanded: false,
                        columns_loaded_at: None,
                        columns: vec![],
                    },
                ],
            },
            SchemaNode::Database {
                name: "analytics".into(),
                expanded: false,
                tables_loaded_at: None,
                tables: vec![],
            },
        ]
    }

    #[test]
    fn completions_qualified_by_database() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_schema_nodes(sample_schema());
        let ctx = CompletionCtx::Qualified {
            parent: "deepci_maindb".into(),
        };
        let items = s.completions_for_context(&ctx, "");
        assert_eq!(items, vec!["orders".to_string(), "users".to_string()]);
    }

    #[test]
    fn completions_qualified_by_database_case_insensitive() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_schema_nodes(sample_schema());
        let ctx = CompletionCtx::Qualified {
            parent: "DEEPCI_MAINDB".into(),
        };
        let items = s.completions_for_context(&ctx, "use");
        assert_eq!(items, vec!["users".to_string()]);
    }

    #[test]
    fn completions_qualified_by_table() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_schema_nodes(sample_schema());
        let ctx = CompletionCtx::Qualified {
            parent: "users".into(),
        };
        let items = s.completions_for_context(&ctx, "");
        assert_eq!(items, vec!["email".to_string(), "id".to_string()]);
    }

    #[test]
    fn completions_table_context_lists_all_tables() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_schema_nodes(sample_schema());
        let ctx = CompletionCtx::Table;
        let items = s.completions_for_context(&ctx, "");
        assert_eq!(items, vec!["orders".to_string(), "users".to_string()]);
    }

    #[test]
    fn completions_column_context_scopes_to_tables() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_schema_nodes(sample_schema());
        let ctx = CompletionCtx::Column {
            tables: vec!["users".into()],
        };
        let items = s.completions_for_context(&ctx, "");
        assert_eq!(items, vec!["email".to_string(), "id".to_string()]);
    }

    #[test]
    fn completions_column_context_empty_tables_returns_all_columns() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_schema_nodes(sample_schema());
        let ctx = CompletionCtx::Column { tables: vec![] };
        let items = s.completions_for_context(&ctx, "");
        assert_eq!(items, vec!["email".to_string(), "id".to_string()]);
    }

    #[test]
    fn completions_any_context_returns_everything() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_schema_nodes(sample_schema());
        let ctx = CompletionCtx::Any;
        let items = s.completions_for_context(&ctx, "");
        // Databases, tables, and columns — all deduped + sorted.
        assert!(items.contains(&"deepci_maindb".to_string()));
        assert!(items.contains(&"analytics".to_string()));
        assert!(items.contains(&"users".to_string()));
        assert!(items.contains(&"email".to_string()));
    }

    #[test]
    fn completions_prefix_filter_case_insensitive() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_schema_nodes(sample_schema());
        let ctx = CompletionCtx::Table;
        let items = s.completions_for_context(&ctx, "OR");
        assert_eq!(items, vec!["orders".to_string()]);
    }

    #[test]
    fn lazy_load_fires_tables_for_unloaded_database() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SchemaLoadRequest>();
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_schema_nodes(sample_schema());
        s.schema_load_tx = Some(tx);

        let ctx = CompletionCtx::Qualified {
            parent: "analytics".into(),
        };
        s.lazy_load_for_context(&ctx);

        let req = rx.try_recv().expect("expected Tables request");
        assert_eq!(
            req,
            SchemaLoadRequest::Tables {
                db: "analytics".into()
            }
        );
    }

    #[test]
    fn lazy_load_skips_loaded_database() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SchemaLoadRequest>();
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_schema_nodes(sample_schema());
        s.schema_load_tx = Some(tx);

        let ctx = CompletionCtx::Qualified {
            parent: "deepci_maindb".into(),
        };
        s.lazy_load_for_context(&ctx);

        assert!(rx.try_recv().is_err(), "should not load already-loaded db");
    }

    #[test]
    fn lazy_load_fires_columns_for_unloaded_table() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SchemaLoadRequest>();
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_schema_nodes(sample_schema());
        s.schema_load_tx = Some(tx);

        let ctx = CompletionCtx::Qualified {
            parent: "orders".into(),
        };
        s.lazy_load_for_context(&ctx);

        let req = rx.try_recv().expect("expected Columns request");
        assert_eq!(
            req,
            SchemaLoadRequest::Columns {
                db: "deepci_maindb".into(),
                table: "orders".into()
            }
        );
    }

    #[test]
    fn lazy_load_column_ctx_fires_for_referenced_tables() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SchemaLoadRequest>();
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_schema_nodes(sample_schema());
        s.schema_load_tx = Some(tx);

        let ctx = CompletionCtx::Column {
            tables: vec!["orders".into(), "users".into()],
        };
        s.lazy_load_for_context(&ctx);

        // Only `orders` is unloaded — users already has columns.
        let req = rx.try_recv().expect("expected Columns request");
        assert_eq!(
            req,
            SchemaLoadRequest::Columns {
                db: "deepci_maindb".into(),
                table: "orders".into()
            }
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn request_schema_load_dedupes_inflight() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SchemaLoadRequest>();
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.schema_load_tx = Some(tx);

        let req = SchemaLoadRequest::Tables { db: "foo".into() };
        s.request_schema_load(req.clone());
        s.request_schema_load(req.clone());
        s.request_schema_load(req.clone());

        assert!(rx.try_recv().is_ok());
        assert!(
            rx.try_recv().is_err(),
            "in-flight duplicates must be suppressed"
        );
        assert_eq!(s.schema_pending_loads, 1);

        // After the loader finishes, the same request is eligible again.
        s.finish_schema_load(&req);
        assert_eq!(s.schema_pending_loads, 0);
        s.request_schema_load(req);
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn refresh_stale_schema_skips_fresh_entries() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SchemaLoadRequest>();
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.schema_ttl = std::time::Duration::from_secs(300);
        s.schema_load_tx = Some(tx);
        s.set_schema_nodes(sample_schema()); // stamps databases_loaded_at = now
        // Drain the dedup set so nothing is falsely blocked.
        s.schema_loads_inflight.clear();
        s.schema_pending_loads = 0;

        s.refresh_stale_schema();
        assert!(
            rx.try_recv().is_err(),
            "fresh caches must not trigger refreshes"
        );
    }

    #[test]
    fn refresh_stale_schema_refetches_expired_tables() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SchemaLoadRequest>();
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.schema_ttl = std::time::Duration::from_millis(1);
        s.schema_load_tx = Some(tx);
        s.set_schema_nodes(sample_schema());
        std::thread::sleep(std::time::Duration::from_millis(5));
        s.schema_loads_inflight.clear();
        s.schema_pending_loads = 0;

        s.refresh_stale_schema();

        let mut got = Vec::new();
        while let Ok(req) = rx.try_recv() {
            got.push(req);
        }
        // Expect at least a Databases refresh plus Tables for deepci_maindb.
        assert!(got.contains(&SchemaLoadRequest::Databases));
        assert!(got.contains(&SchemaLoadRequest::Tables {
            db: "deepci_maindb".into()
        }));
        // `users` had columns_loaded_at set — it should refetch. `orders`
        // never loaded columns, so we must NOT preemptively fetch them.
        assert!(got.contains(&SchemaLoadRequest::Columns {
            db: "deepci_maindb".into(),
            table: "users".into()
        }));
        assert!(!got.contains(&SchemaLoadRequest::Columns {
            db: "deepci_maindb".into(),
            table: "orders".into()
        }));
    }

    #[test]
    fn refresh_stale_schema_noop_when_ttl_is_zero() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SchemaLoadRequest>();
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.schema_ttl = std::time::Duration::ZERO;
        s.schema_load_tx = Some(tx);
        s.set_schema_nodes(sample_schema());

        s.refresh_stale_schema();
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn invalidate_for_ddl_databases_fires_databases_load() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SchemaLoadRequest>();
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.schema_load_tx = Some(tx);

        s.invalidate_for_ddl(&DdlEffect::Databases);
        assert_eq!(rx.try_recv().unwrap(), SchemaLoadRequest::Databases);
    }

    #[test]
    fn invalidate_for_ddl_tables_qualified() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SchemaLoadRequest>();
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.schema_load_tx = Some(tx);

        s.invalidate_for_ddl(&DdlEffect::Tables {
            db: Some("deepci_maindb".into()),
        });
        assert_eq!(
            rx.try_recv().unwrap(),
            SchemaLoadRequest::Tables {
                db: "deepci_maindb".into()
            }
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn invalidate_for_ddl_tables_unqualified_fans_out() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SchemaLoadRequest>();
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.schema_load_tx = Some(tx);
        s.set_schema_nodes(sample_schema());

        s.invalidate_for_ddl(&DdlEffect::Tables { db: None });

        let mut got = Vec::new();
        while let Ok(req) = rx.try_recv() {
            got.push(req);
        }
        // Databases reload, plus Tables for every db with a loaded cache.
        // `analytics` has tables_loaded_at=None so it must NOT be fetched.
        assert!(got.contains(&SchemaLoadRequest::Databases));
        assert!(got.contains(&SchemaLoadRequest::Tables {
            db: "deepci_maindb".into()
        }));
        assert!(!got.contains(&SchemaLoadRequest::Tables {
            db: "analytics".into()
        }));
    }

    #[test]
    fn invalidate_for_ddl_columns_qualified() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SchemaLoadRequest>();
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.schema_load_tx = Some(tx);

        s.invalidate_for_ddl(&DdlEffect::Columns {
            db: Some("mydb".into()),
            table: "users".into(),
        });
        assert_eq!(
            rx.try_recv().unwrap(),
            SchemaLoadRequest::Columns {
                db: "mydb".into(),
                table: "users".into()
            }
        );
    }

    #[test]
    fn invalidate_for_ddl_columns_unqualified_fans_across_dbs() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SchemaLoadRequest>();
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.schema_load_tx = Some(tx);
        // Two databases, both containing a `users` table.
        s.set_schema_nodes(vec![
            SchemaNode::Database {
                name: "db1".into(),
                expanded: false,
                tables_loaded_at: Some(Instant::now()),
                tables: vec![SchemaNode::Table {
                    name: "users".into(),
                    expanded: false,
                    columns_loaded_at: None,
                    columns: vec![],
                }],
            },
            SchemaNode::Database {
                name: "db2".into(),
                expanded: false,
                tables_loaded_at: Some(Instant::now()),
                tables: vec![SchemaNode::Table {
                    name: "users".into(),
                    expanded: false,
                    columns_loaded_at: None,
                    columns: vec![],
                }],
            },
        ]);

        s.invalidate_for_ddl(&DdlEffect::Columns {
            db: None,
            table: "users".into(),
        });

        let mut got = Vec::new();
        while let Ok(req) = rx.try_recv() {
            got.push(req);
        }
        assert!(got.contains(&SchemaLoadRequest::Columns {
            db: "db1".into(),
            table: "users".into()
        }));
        assert!(got.contains(&SchemaLoadRequest::Columns {
            db: "db2".into(),
            table: "users".into()
        }));
    }

    #[test]
    fn lazy_load_for_context_honors_ttl() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SchemaLoadRequest>();
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.schema_ttl = std::time::Duration::from_millis(1);
        s.schema_load_tx = Some(tx);
        s.set_schema_nodes(sample_schema());
        std::thread::sleep(std::time::Duration::from_millis(5));
        s.schema_loads_inflight.clear();
        s.schema_pending_loads = 0;

        // `deepci_maindb` is loaded but stale → must re-fire Tables.
        let ctx = CompletionCtx::Qualified {
            parent: "deepci_maindb".into(),
        };
        s.lazy_load_for_context(&ctx);
        let req = rx.try_recv().expect("expected stale Tables refresh");
        assert_eq!(
            req,
            SchemaLoadRequest::Tables {
                db: "deepci_maindb".into()
            }
        );
    }

    /// Redirect the data dir to a fresh tempdir so tests exercising
    /// `save_active_tab` don't write to the user's real
    /// `~/.local/share/sqeel`. Returns the dir for the caller to keep
    /// alive for the duration of the test.
    fn isolated_data_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        // SAFETY: tests are single-threaded per process unless marked
        // `#[test]` with parallel execution — these two tests touch
        // the same env var, so they share one tempdir via this setter
        // being idempotent per process.
        unsafe {
            std::env::set_var("XDG_DATA_HOME", dir.path());
        }
        dir
    }

    /// Regression: above the heavy-pipeline gate (buffers > 2 MB) the
    /// TUI used to leave `editor_content_synced = false`, so a `:w`
    /// would write the stale cached `tab.content` instead of the
    /// freshly updated `editor_content`. Pins the underlying
    /// `save_active_tab` contract — the TUI is expected to flip
    /// `editor_content_synced = true` before invoking save.
    #[test]
    fn save_active_tab_uses_editor_content_when_synced() {
        use std::sync::Arc;
        let _dir = isolated_data_dir();
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.tabs.push(TabEntry::open(
            "scratch_sync".into(),
            "stale on disk".to_string(),
        ));
        s.active_tab = 0;
        s.editor_content = Arc::new("fresh edit".to_string());
        s.editor_content_synced = true;
        let _ = s.save_active_tab();
        assert_eq!(s.tabs[0].content.as_deref(), Some("fresh edit"));
    }

    #[test]
    fn switch_to_cold_tab_queues_pending_load_and_shows_empty() {
        let _dir = isolated_data_dir();
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        // Two tabs — first warm (has content), second cold.
        s.tabs.push(TabEntry::open("warm".into(), "hello".into()));
        let mut cold = TabEntry::open("cold".into(), "ignored".into());
        cold.content = None;
        s.tabs.push(cold);
        s.active_tab = 0;
        s.editor_content_synced = false;

        s.switch_to_tab(1);
        // Pending load queued for tab 1; editor gets an empty string
        // immediately so it doesn't appear stuck on the previous tab.
        assert_eq!(s.active_tab, 1);
        assert_eq!(s.tab_content_pending.as_deref(), Some(""));
        let load = s.pending_tab_load.as_ref().expect("pending load queued");
        assert_eq!(load.tab_index, 1);
        assert_eq!(load.name, "cold");
    }

    #[test]
    fn apply_loaded_tab_content_publishes_for_active_tab() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.tabs
            .push(TabEntry::open("only".into(), "placeholder".into()));
        s.tabs[0].content = None;
        s.active_tab = 0;

        s.apply_loaded_tab_content(0, "disk content".into());
        assert_eq!(s.tabs[0].content.as_deref(), Some("disk content"));
        assert_eq!(s.tab_content_pending.as_deref(), Some("disk content"));
    }

    #[test]
    fn apply_loaded_tab_content_stale_switch_does_not_clobber_editor() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.tabs.push(TabEntry::open("a".into(), "a content".into()));
        s.tabs.push(TabEntry::open("b".into(), "b content".into()));
        s.tabs[1].content = None;
        s.active_tab = 0; // looking at tab a
        s.tab_content_pending = None;

        // Slow disk read for tab b finally lands — we're on a now.
        s.apply_loaded_tab_content(1, "b disk".into());
        // Cache updated for future visits to tab b.
        assert_eq!(s.tabs[1].content.as_deref(), Some("b disk"));
        // But the editor (tab_content_pending) is NOT clobbered with
        // b's content — we're viewing a.
        assert!(s.tab_content_pending.is_none());
    }

    #[test]
    fn cancel_control_flag_set_and_reset() {
        let c = CancelControl::default();
        assert!(!c.is_cancelled());
        c.cancel();
        assert!(c.is_cancelled());
        c.reset();
        assert!(!c.is_cancelled());
    }

    #[tokio::test]
    async fn cancel_control_cancelled_future_resolves_after_cancel() {
        let c = std::sync::Arc::new(CancelControl::default());
        let c2 = c.clone();
        let waiter = tokio::spawn(async move { c2.cancelled().await });
        // Tiny yield so the waiter enters the Notify await.
        tokio::task::yield_now().await;
        c.cancel();
        tokio::time::timeout(std::time::Duration::from_millis(100), waiter)
            .await
            .expect("cancelled() should resolve after cancel()")
            .expect("waiter task ok");
    }

    #[tokio::test]
    async fn cancel_control_cancelled_future_resolves_if_already_cancelled() {
        let c = CancelControl::default();
        c.cancel();
        // Should return immediately without timing out.
        tokio::time::timeout(std::time::Duration::from_millis(50), c.cancelled())
            .await
            .expect("cancelled() should resolve immediately when flag is already set");
    }

    #[test]
    fn query_in_flight_true_during_batch_or_loading_tab() {
        use crate::state::{ResultsPane, ResultsTab};
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        assert!(!s.query_in_flight());
        s.batch_in_progress = true;
        assert!(s.query_in_flight());
        s.batch_in_progress = false;
        s.result_tabs.push(ResultsTab {
            query: "q".into(),
            kind: ResultsPane::Loading,
            cursor: crate::state::ResultsCursor::Query,
            scroll: 0,
            col_scroll: 0,
            saved_filename: None,
            selection: None,
        });
        assert!(s.query_in_flight());
    }

    #[test]
    fn schema_snapshot_for_rebuild_returns_none_when_clean() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        assert!(s.schema_snapshot_for_rebuild().is_none());
    }

    #[test]
    fn schema_snapshot_for_rebuild_yields_once_and_marks_in_flight() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.schema_cache_dirty = true;
        let snap = s.schema_snapshot_for_rebuild();
        assert!(snap.is_some());
        assert!(s.schema_rebuild_in_flight);
        assert!(!s.schema_cache_dirty, "dirty cleared on snapshot");
        // Second call returns None because in_flight blocks it.
        s.schema_cache_dirty = true;
        assert!(s.schema_snapshot_for_rebuild().is_none());
    }

    #[test]
    fn apply_schema_cache_rebuild_clears_in_flight_and_installs_caches() {
        use crate::schema::{SchemaItemKind, SchemaTreeItem};
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.schema_rebuild_in_flight = true;
        let items = vec![SchemaTreeItem {
            label: "x".into(),
            depth: 0,
            node_path: vec![0],
            name: "x".into(),
            kind: SchemaItemKind::Database,
        }];
        s.apply_schema_cache_rebuild(items.clone(), items.clone(), vec!["x".into()]);
        assert!(!s.schema_rebuild_in_flight);
        assert_eq!(s.schema_identifier_cache.len(), 1);
    }

    #[test]
    fn prepare_save_active_tab_leaves_dirty_until_marked() {
        use std::sync::Arc;
        let _dir = isolated_data_dir();
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        let mut tab = TabEntry::open("scratch_split".into(), "old".into());
        tab.dirty = true;
        s.tabs.push(tab);
        s.active_tab = 0;
        s.editor_content = Arc::new("new".into());
        s.editor_content_synced = true;

        let pending = s.prepare_save_active_tab().expect("prepare should succeed");
        assert_eq!(pending.name, "scratch_split");
        assert_eq!(pending.content, "new");
        assert_eq!(pending.tab_index, Some(0));
        // In-memory snapshot updated, but dirty stays set until the
        // disk write lands and we explicitly mark.
        assert_eq!(s.tabs[0].content.as_deref(), Some("new"));
        assert!(s.tabs[0].dirty, "dirty flag must stay set pre-commit");

        pending.commit().unwrap();
        s.mark_tab_saved(0);
        assert!(!s.tabs[0].dirty);
    }

    #[test]
    fn save_active_tab_falls_back_to_tab_content_when_not_synced() {
        use std::sync::Arc;
        let _dir = isolated_data_dir();
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.tabs.push(TabEntry::open(
            "scratch_unsynced".into(),
            "cached content".to_string(),
        ));
        s.active_tab = 0;
        s.editor_content = Arc::new("fresh edit but not synced".to_string());
        s.editor_content_synced = false;
        let _ = s.save_active_tab();
        // Fall back to the cached tab.content so a freshly loaded tab
        // doesn't get clobbered with whatever `editor_content` held
        // before the TUI sync happened (startup race).
        assert_eq!(s.tabs[0].content.as_deref(), Some("cached content"));
    }

    #[test]
    fn validate_connection_url_accepts_supported_schemes() {
        assert!(validate_connection_url("mysql://user:pass@localhost/db").is_ok());
        assert!(validate_connection_url("mariadb://localhost/db").is_ok());
        assert!(validate_connection_url("postgres://localhost/db").is_ok());
        assert!(validate_connection_url("postgresql://localhost/db").is_ok());
        assert!(validate_connection_url("sqlite:///path/to/file.db").is_ok());
        assert!(validate_connection_url("sqlite::memory:").is_ok());
    }

    #[test]
    fn validate_connection_url_rejects_missing_scheme() {
        let err = validate_connection_url("localhost/db").unwrap_err();
        assert!(err.contains("scheme"), "got: {err}");
    }

    #[test]
    fn validate_connection_url_rejects_unknown_scheme() {
        let err = validate_connection_url("oracle://h/db").unwrap_err();
        assert!(err.contains("oracle"), "got: {err}");
    }

    #[test]
    fn validate_connection_url_rejects_missing_authority() {
        let err = validate_connection_url("mysql://").unwrap_err();
        assert!(err.contains("host"), "got: {err}");
    }

    #[test]
    fn validate_connection_url_rejects_single_colon_form_for_network_schemes() {
        let err = validate_connection_url("mysql:localhost/db").unwrap_err();
        assert!(err.contains("mysql://"), "got: {err}");
    }

    #[test]
    fn url_has_plaintext_password_detects_userinfo_password() {
        assert!(url_has_plaintext_password("mysql://user:pass@h/db"));
        assert!(url_has_plaintext_password("postgres://u:p@h:5432/db"));
    }

    #[test]
    fn url_has_plaintext_password_ignores_userless_url() {
        assert!(!url_has_plaintext_password("mysql://host/db"));
        assert!(!url_has_plaintext_password("mysql://user@host/db"));
        assert!(!url_has_plaintext_password("mysql://user:@host/db"));
        assert!(!url_has_plaintext_password("sqlite:///tmp/x.db"));
    }

    #[test]
    fn url_has_plaintext_password_ignores_path_colon_after_at() {
        // The `:5432` in the host portion isn't a password.
        assert!(!url_has_plaintext_password(
            "mysql://user@host:5432/db?foo=bar"
        ));
    }

    #[test]
    fn delete_selected_connection_arms_first_then_commits_second() {
        let _dir = isolated_data_dir();
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.available_connections = vec![
            crate::config::ConnectionConfig {
                name: "alpha".into(),
                url: "sqlite::memory:".into(),
            },
            crate::config::ConnectionConfig {
                name: "beta".into(),
                url: "sqlite::memory:".into(),
            },
        ];
        s.connection_switcher_cursor = 0;
        // First press arms — entry stays in the list.
        s.delete_selected_connection().unwrap();
        assert_eq!(s.connection_delete_armed.as_deref(), Some("alpha"));
        assert_eq!(s.available_connections.len(), 2);
        assert!(s.status_message.is_some());
        // Second press commits.
        s.delete_selected_connection().unwrap();
        assert!(s.connection_delete_armed.is_none());
        assert_eq!(s.available_connections.len(), 1);
        assert_eq!(s.available_connections[0].name, "beta");
    }

    #[test]
    fn moving_selection_disarms_pending_delete() {
        let _dir = isolated_data_dir();
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.available_connections = vec![
            crate::config::ConnectionConfig {
                name: "alpha".into(),
                url: "sqlite::memory:".into(),
            },
            crate::config::ConnectionConfig {
                name: "beta".into(),
                url: "sqlite::memory:".into(),
            },
        ];
        s.delete_selected_connection().unwrap();
        assert!(s.connection_delete_armed.is_some());
        s.switcher_down();
        assert!(s.connection_delete_armed.is_none());
        assert_eq!(s.available_connections.len(), 2);
    }
}
