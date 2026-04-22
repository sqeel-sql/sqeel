use crate::config::ConnectionConfig;
use crate::highlight::HighlightSpan;
use crate::lsp::Diagnostic;
use crate::persistence;
use crate::schema::{
    SchemaNode, SchemaTreeItem, collect_expanded_paths, expand_path, find_cursor_by_path,
    flatten_all, flatten_tree, merge_expansion, path_to_string, restore_expanded_paths,
    toggle_node,
};
use lsp_types::DiagnosticSeverity;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// One open editor tab. Content is lazily loaded and evicted after 5 min of inactivity.
#[derive(Debug, Clone)]
pub struct TabEntry {
    pub name: String,
    pub content: Option<String>,
    pub last_accessed: Option<Instant>,
}

impl TabEntry {
    fn new(name: String) -> Self {
        Self {
            name,
            content: None,
            last_accessed: None,
        }
    }
    fn open(name: String, content: String) -> Self {
        Self {
            name,
            content: Some(content),
            last_accessed: Some(Instant::now()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeybindingMode {
    #[default]
    Vim,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VimMode {
    #[default]
    Normal,
    Insert,
    Visual,
    VisualLine,
}

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

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, Default)]
pub enum ResultsPane {
    #[default]
    Empty,
    Results(QueryResult),
    Error(String),
}

/// One entry in the results pane's tab bar — the query that produced it and
/// either a result set or an error. `kind` is never `ResultsPane::Empty`.
#[derive(Debug, Clone)]
pub struct ResultsTab {
    pub query: String,
    pub kind: ResultsPane,
    /// Per-tab vertical scroll position; preserved across tab switches.
    pub scroll: usize,
    /// Per-tab horizontal column scroll position.
    pub col_scroll: usize,
}

/// A query request sent over the executor channel — single statement or batch.
#[derive(Debug, Clone)]
pub enum QueryRequest {
    Single(String),
    Batch(Vec<String>),
}

#[derive(Default)]
pub struct AppState {
    pub editor_content: Arc<String>,
    pub tabs: Vec<TabEntry>,
    pub active_tab: usize,
    /// Set when the active tab changes; TUI drains this to reload the editor.
    pub tab_content_pending: Option<String>,
    pub keybinding_mode: KeybindingMode,
    pub vim_mode: VimMode,
    pub focus: Focus,
    pub result_tabs: Vec<ResultsTab>,
    pub active_result_tab: usize,
    /// Soft cap for accumulated single-query result tabs. A run-all batch
    /// may temporarily exceed this; the cap re-applies on the next single push.
    pub max_result_tabs: usize,
    /// Whether a run-all batch should stop on the first query error.
    pub stop_on_error: bool,
    /// Set while a run-all batch is in progress so cap enforcement is skipped.
    pub batch_in_progress: bool,
    pub editor_ratio: f32,
    pub lsp_diagnostics: Vec<Diagnostic>,
    pub highlight_spans: Vec<HighlightSpan>,
    pub completions: Vec<String>,
    pub show_completions: bool,
    pub completion_cursor: usize,
    pub active_connection: Option<String>,
    pub status_message: Option<String>,
    pub schema_nodes: Vec<SchemaNode>,
    pub schema_cursor: usize,
    pub schema_loading: bool,
    schema_items_cache: Vec<SchemaTreeItem>,
    all_schema_items_cache: Vec<SchemaTreeItem>,
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
    pub sidebar_visible: bool,
    // Debug mode — enabled via --debug CLI flag
    pub debug_mode: bool,
    pub lsp_available: bool,
    pub lsp_binary: String,
    // Live query channel — set by the binary when connected
    pub query_tx: Option<tokio::sync::mpsc::Sender<QueryRequest>>,
    /// Schema sidebar search query — persisted to session so it survives app restart.
    pub schema_search_query: Option<String>,
}

impl AppState {
    pub fn new() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            editor_ratio: 1.0,
            sidebar_visible: false,
            max_result_tabs: 10,
            stop_on_error: true,
            ..Default::default()
        }))
    }

    pub fn apply_editor_config(&mut self, cfg: &crate::config::EditorConfig) {
        self.max_result_tabs = cfg.max_result_tabs;
        self.stop_on_error = cfg.stop_on_error;
    }

    fn rebuild_schema_cache(&mut self) {
        self.schema_items_cache = flatten_tree(&self.schema_nodes);
        self.all_schema_items_cache = flatten_all(&self.schema_nodes);
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

    pub fn results_col_scroll(&self) -> usize {
        self.active_result().map(|t| t.col_scroll).unwrap_or(0)
    }

    /// Append a new result tab. Single-query semantics (cap enforcement) when
    /// `batch_in_progress` is false; batch pushes append unbounded.
    pub fn push_result_tab(&mut self, query: String, kind: ResultsPane) {
        let tab = ResultsTab {
            query,
            kind,
            scroll: 0,
            col_scroll: 0,
        };
        self.result_tabs.push(tab);
        if !self.batch_in_progress && self.max_result_tabs > 0 {
            while self.result_tabs.len() > self.max_result_tabs {
                self.result_tabs.remove(0);
            }
            self.active_result_tab = self.result_tabs.len().saturating_sub(1);
        }
        self.editor_ratio = 0.5;
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
        let max = match self.active_result().map(|t| &t.kind) {
            Some(ResultsPane::Results(r)) => r.rows.len().saturating_sub(1),
            _ => 0,
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
        let max = match self.active_result().map(|t| &t.kind) {
            Some(ResultsPane::Results(r)) => r.columns.len().saturating_sub(1),
            _ => 0,
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

    /// Collect all identifier names from the schema tree (databases, tables, columns)
    /// with no filtering — for passing to a background completion thread.
    pub fn schema_identifier_names(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack: Vec<&SchemaNode> = self.schema_nodes.iter().collect();
        while let Some(node) = stack.pop() {
            out.push(node.name().to_owned());
            match node {
                SchemaNode::Database { tables, .. } => stack.extend(tables.iter()),
                SchemaNode::Table { columns, .. } => stack.extend(columns.iter()),
                SchemaNode::Column { .. } => {}
            }
        }
        out
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
    }

    pub fn schema_cursor_up(&mut self) {
        self.schema_cursor = self.schema_cursor.saturating_sub(1);
    }

    pub fn schema_toggle_path(&mut self, path: &[usize]) {
        toggle_node(&mut self.schema_nodes, path);
        self.rebuild_schema_cache();
    }

    pub fn schema_toggle_current(&mut self) {
        let path = self
            .schema_items_cache
            .get(self.schema_cursor)
            .map(|item| item.node_path.clone());
        if let Some(path) = path {
            toggle_node(&mut self.schema_nodes, &path);
            self.rebuild_schema_cache();
        }
    }

    pub fn set_schema_nodes(&mut self, nodes: Vec<SchemaNode>) {
        self.schema_nodes = nodes;
        self.schema_cursor = 0;
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
            self.rebuild_schema_cache();
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
                        name, columns: c, ..
                    } = table
                        && name == table_name
                    {
                        *c = columns;
                        changed = true;
                        break 'outer;
                    }
                }
                break;
            }
        }
        if changed {
            self.rebuild_schema_cache();
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
    pub fn send_query(&self, query: String) -> bool {
        self.send_query_request(QueryRequest::Single(query))
    }

    /// Try to send a batch of queries to the active executor. Returns false if not connected.
    pub fn send_batch(&self, queries: Vec<String>) -> bool {
        self.send_query_request(QueryRequest::Batch(queries))
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
        // Persist current tab content in memory before leaving
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.content = Some((*self.editor_content).clone());
        }
        self.active_tab = idx;
        let slug =
            persistence::sanitize_conn_slug(self.active_connection.as_deref().unwrap_or("default"));
        let content = if let Some(tab) = self.tabs.get_mut(idx) {
            tab.last_accessed = Some(Instant::now());
            if let Some(ref c) = tab.content {
                c.clone()
            } else {
                let c = persistence::load_query(&slug, &tab.name).unwrap_or_default();
                tab.content = Some(c.clone());
                c
            }
        } else {
            String::new()
        };
        self.tab_content_pending = Some(content);
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

    /// Auto-save editor content to the active tab on disk.
    pub fn autosave(&mut self) {
        let slug =
            persistence::sanitize_conn_slug(self.active_connection.as_deref().unwrap_or("default"));
        if self.tabs.is_empty() {
            if let Ok(name) = persistence::next_scratch_name(&slug) {
                let _ = persistence::save_query(&slug, &name, &self.editor_content);
                self.tabs
                    .push(TabEntry::open(name, (*self.editor_content).clone()));
                self.active_tab = 0;
            }
            return;
        }
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.content = Some((*self.editor_content).clone());
            tab.last_accessed = Some(Instant::now());
            let _ = persistence::save_query(&slug, &tab.name, &self.editor_content);
        }
    }

    /// Persist a successful query result to disk (errors are never stored).
    pub fn persist_result(&self, query: &str, result: &QueryResult) {
        let slug =
            persistence::sanitize_conn_slug(self.active_connection.as_deref().unwrap_or("default"));
        let _ = persistence::save_result(&slug, query, result);
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
    fn dismiss_restores_editor() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_error("oops".into());
        s.dismiss_results();
        assert_eq!(s.editor_ratio, 1.0);
        assert!(matches!(s.results(), ResultsPane::Empty));
    }

    #[test]
    fn singles_accumulate_then_cap_evicts_fifo() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.max_result_tabs = 3;
        for i in 0..5 {
            s.push_result_tab(format!("q{i}"), ResultsPane::Error(format!("e{i}")));
        }
        assert_eq!(s.result_tabs.len(), 3);
        assert_eq!(s.result_tabs[0].query, "q2");
        assert_eq!(s.result_tabs[2].query, "q4");
        assert_eq!(s.active_result_tab, 2);
    }

    #[test]
    fn batch_bypasses_cap_then_single_reapplies() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.max_result_tabs = 3;
        let start = s.start_batch();
        for i in 0..5 {
            s.push_result_tab(format!("b{i}"), ResultsPane::Error("e".into()));
        }
        assert_eq!(s.result_tabs.len(), 5);
        s.end_batch(start);
        assert_eq!(s.active_result_tab, 0);
        // Next single push triggers cap re-application.
        s.push_result_tab("after".into(), ResultsPane::Error("e".into()));
        assert_eq!(s.result_tabs.len(), 3);
        assert_eq!(s.result_tabs[2].query, "after");
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
}
