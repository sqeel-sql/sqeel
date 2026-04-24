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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Focus {
    #[default]
    Editor,
    Schema,
    Results,
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
/// cells, given the column widths baked into `r.col_widths` (plus the 1-cell
/// `│` separator between columns). Leaves `col_scroll` unchanged if already
/// visible.
fn scroll_cols_into_view(r: &QueryResult, col_scroll: &mut usize, col: usize, width: u16) {
    if col < *col_scroll {
        *col_scroll = col;
        return;
    }
    if width == 0 || r.col_widths.is_empty() {
        return;
    }
    // Shrink col_scroll until the cursor column's right edge fits in the
    // viewport. Each column contributes its width plus a 1-cell separator
    // (except after the final column, but over-counting by 1 is safe).
    loop {
        let used: u32 = r
            .col_widths
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
    pub active_connection: Option<String>,
    /// SQL dialect of the current connection. Drives per-dialect
    /// keyword highlighting; `Generic` before any connection opens.
    pub active_dialect: Dialect,
    /// When a connection resolves we write a sqls config file from its
    /// URL and park the path here. The TUI main loop takes it, restarts
    /// the LSP with `--config=<path>`, so sqls can emit schema-aware
    /// diagnostics instead of running blind.
    pub pending_sqls_config: Option<std::path::PathBuf>,
    pub status_message: Option<String>,
    pub schema_nodes: Vec<SchemaNode>,
    pub schema_cursor: usize,
    pub schema_loading: bool,
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
    pub query_history: Vec<String>,
    pub history_cursor: Option<usize>,
    // Connection switcher
    pub available_connections: Vec<ConnectionConfig>,
    pub show_connection_switcher: bool,
    pub connection_switcher_cursor: usize,
    pub pending_reconnect: Option<String>,
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
                    scroll_cols_into_view(r, &mut tab.col_scroll, col, width);
                }
            }
            ResultsCursor::Header(col) => {
                if let ResultsPane::Results(r) = &tab.kind {
                    scroll_cols_into_view(r, &mut tab.col_scroll, col, width);
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
    }

    pub fn close_connection_switcher(&mut self) {
        self.show_connection_switcher = false;
    }

    pub fn switcher_up(&mut self) {
        self.connection_switcher_cursor = self.connection_switcher_cursor.saturating_sub(1);
    }

    pub fn switcher_down(&mut self) {
        let max = self.available_connections.len().saturating_sub(1);
        if self.connection_switcher_cursor < max {
            self.connection_switcher_cursor += 1;
        }
    }

    /// Confirm the highlighted connection — returns its URL if one exists.
    pub fn confirm_connection_switch(&mut self) -> Option<String> {
        let url = self
            .available_connections
            .get(self.connection_switcher_cursor)
            .map(|c| c.url.clone());
        if let Some(ref u) = url {
            self.pending_reconnect = Some(u.clone());
        }
        self.show_connection_switcher = false;
        url
    }

    pub fn open_add_connection(&mut self) {
        self.show_add_connection = true;
        self.add_connection_name.clear();
        self.add_connection_url.clear();
        self.add_connection_name_cursor = 0;
        self.add_connection_url_cursor = 0;
        self.add_connection_field = AddConnectionField::Name;
        self.edit_connection_original_name = None;
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
    }

    pub fn close_add_connection(&mut self) {
        self.show_add_connection = false;
        self.edit_connection_original_name = None;
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
                .push(crate::config::ConnectionConfig { name, url });
        }
        self.show_add_connection = false;
        self.edit_connection_original_name = None;
        Ok(())
    }

    /// Remove the currently highlighted connection from disk and memory.
    pub fn delete_selected_connection(&mut self) -> anyhow::Result<()> {
        let Some(conn) = self
            .available_connections
            .get(self.connection_switcher_cursor)
            .cloned()
        else {
            return Ok(());
        };
        crate::config::delete_connection(&conn.name)?;
        self.available_connections
            .remove(self.connection_switcher_cursor);
        let max = self.available_connections.len().saturating_sub(1);
        self.connection_switcher_cursor = self.connection_switcher_cursor.min(max);
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

    /// Load tabs from disk for the given connection slug.
    /// Sets `tab_content_pending` so the TUI loads the first tab into the editor.
    pub fn load_tabs_for_connection(&mut self, conn_slug: &str) {
        let names = persistence::list_queries_for(conn_slug).unwrap_or_default();
        if names.is_empty() {
            match persistence::next_scratch_name(conn_slug) {
                Ok(name) => {
                    let _ = persistence::save_query(conn_slug, &name, "");
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
            let content = persistence::load_query(conn_slug, &tab.name).unwrap_or_default();
            tab.content = Some(content.clone());
            self.tab_content_pending = Some(content);
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
        let slug =
            persistence::sanitize_conn_slug(self.active_connection.as_deref().unwrap_or("default"));
        let (content, cursor) = if let Some(tab) = self.tabs.get_mut(idx) {
            tab.last_accessed = Some(Instant::now());
            let c = if let Some(ref c) = tab.content {
                c.clone()
            } else {
                let c = persistence::load_query(&slug, &tab.name).unwrap_or_default();
                tab.content = Some(c.clone());
                c
            };
            (c, tab.cursor)
        } else {
            (String::new(), None)
        };
        self.tab_content_pending = Some(content);
        self.tab_cursor_pending = cursor;
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
        let slug =
            persistence::sanitize_conn_slug(self.active_connection.as_deref().unwrap_or("default"));
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            anyhow::bail!("No active tab");
        };
        if tab.name == final_name {
            return Ok(());
        }
        persistence::rename_query(&slug, &tab.name, &final_name)?;
        tab.name = final_name;
        Ok(())
    }

    /// Delete the active tab's on-disk file and drop the in-memory entry.
    /// If this was the last tab, a fresh empty scratch tab is created so the
    /// editor always has something to edit.
    pub fn delete_active_tab(&mut self) -> anyhow::Result<()> {
        let slug =
            persistence::sanitize_conn_slug(self.active_connection.as_deref().unwrap_or("default"));
        let Some(tab) = self.tabs.get(self.active_tab) else {
            anyhow::bail!("No active tab");
        };
        let name = tab.name.clone();
        persistence::delete_query(&slug, &name)?;
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
        let slug =
            persistence::sanitize_conn_slug(self.active_connection.as_deref().unwrap_or("default"));
        if let Ok(name) = persistence::next_scratch_name(&slug) {
            let _ = persistence::save_query(&slug, &name, "");
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
            let slug = persistence::sanitize_conn_slug(
                self.active_connection.as_deref().unwrap_or("default"),
            );
            let Ok(name) = persistence::next_scratch_name(&slug) else {
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

    /// Persist the active tab's content to disk and clear its dirty flag.
    /// Returns the saved tab's name on success.
    pub fn save_active_tab(&mut self) -> std::io::Result<String> {
        let slug =
            persistence::sanitize_conn_slug(self.active_connection.as_deref().unwrap_or("default"));
        if self.tabs.is_empty() {
            let name = persistence::next_scratch_name(&slug)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let content: String = (*self.editor_content).clone();
            persistence::save_query(&slug, &name, &content)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            self.tabs.push(TabEntry::open(name.clone(), content));
            self.active_tab = 0;
            return Ok(name);
        }
        let idx = self.active_tab;
        let (name, content) = {
            let tab = self.tabs.get_mut(idx).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "no active tab")
            })?;
            let content: String = if self.editor_content_synced {
                (*self.editor_content).clone()
            } else {
                tab.content.clone().unwrap_or_default()
            };
            tab.content = Some(content.clone());
            tab.last_accessed = Some(Instant::now());
            (tab.name.clone(), content)
        };
        persistence::save_query(&slug, &name, &content)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        if let Some(tab) = self.tabs.get_mut(idx) {
            tab.dirty = false;
        }
        Ok(name)
    }

    /// Persist every dirty tab's cached content to disk. Returns the
    /// names of tabs that failed to save.
    pub fn save_all_dirty(&mut self) -> Vec<String> {
        let slug =
            persistence::sanitize_conn_slug(self.active_connection.as_deref().unwrap_or("default"));
        let mut failed = Vec::new();
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            if !tab.dirty {
                continue;
            }
            let content = if i == self.active_tab && self.editor_content_synced {
                (*self.editor_content).clone()
            } else if let Some(c) = &tab.content {
                c.clone()
            } else {
                continue;
            };
            tab.content = Some(content.clone());
            match persistence::save_query(&slug, &tab.name, &content) {
                Ok(_) => tab.dirty = false,
                Err(_) => failed.push(tab.name.clone()),
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
}
