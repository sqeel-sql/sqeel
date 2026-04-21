use crate::config::ConnectionConfig;
use crate::highlight::HighlightSpan;
use crate::lsp::Diagnostic;
use crate::persistence;
use crate::schema::{SchemaNode, SchemaTreeItem, flatten_tree, toggle_node};
use lsp_types::DiagnosticSeverity;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeybindingMode {
    #[default]
    Vim,
    Emacs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VimMode {
    #[default]
    Normal,
    Insert,
    Visual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AddConnectionField {
    #[default]
    Name,
    Url,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
}

#[derive(Debug, Clone, Default)]
pub enum ResultsPane {
    #[default]
    Empty,
    Results(QueryResult),
    Error(String),
}

#[derive(Default)]
pub struct AppState {
    pub editor_content: String,
    pub current_file: Option<String>,
    pub keybinding_mode: KeybindingMode,
    pub vim_mode: VimMode,
    pub focus: Focus,
    pub results: ResultsPane,
    pub editor_ratio: f32,
    pub lsp_diagnostics: Vec<Diagnostic>,
    pub highlight_spans: Vec<HighlightSpan>,
    pub completions: Vec<String>,
    pub show_completions: bool,
    pub active_connection: Option<String>,
    pub status_message: Option<String>,
    pub results_scroll: usize,
    pub schema_nodes: Vec<SchemaNode>,
    pub schema_cursor: usize,
    pub query_history: Vec<String>,
    pub history_cursor: Option<usize>,
    // Connection switcher
    pub available_connections: Vec<ConnectionConfig>,
    pub show_connection_switcher: bool,
    pub connection_switcher_cursor: usize,
    pub pending_reconnect: Option<String>,
    // Add connection dialog
    pub show_add_connection: bool,
    pub add_connection_name: String,
    pub add_connection_url: String,
    pub add_connection_field: AddConnectionField,
    // Live query channel — set by the binary when connected
    pub query_tx: Option<tokio::sync::mpsc::Sender<String>>,
}

impl AppState {
    pub fn new() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            editor_ratio: 1.0,
            ..Default::default()
        }))
    }

    pub fn set_results(&mut self, result: QueryResult) {
        self.results = ResultsPane::Results(result);
        self.editor_ratio = 0.5;
        self.results_scroll = 0;
    }

    pub fn set_error(&mut self, msg: String) {
        self.results = ResultsPane::Error(msg);
        self.editor_ratio = 0.5;
        self.results_scroll = 0;
    }

    pub fn dismiss_results(&mut self) {
        self.results = ResultsPane::Empty;
        self.editor_ratio = 1.0;
        self.results_scroll = 0;
    }

    pub fn scroll_results_down(&mut self) {
        let max = match &self.results {
            ResultsPane::Results(r) => r.rows.len().saturating_sub(1),
            _ => 0,
        };
        if self.results_scroll < max {
            self.results_scroll += 1;
        }
    }

    pub fn scroll_results_up(&mut self) {
        self.results_scroll = self.results_scroll.saturating_sub(1);
    }

    pub fn set_diagnostics(&mut self, diags: Vec<Diagnostic>) {
        self.lsp_diagnostics = diags;
    }

    pub fn set_highlights(&mut self, spans: Vec<HighlightSpan>) {
        self.highlight_spans = spans;
    }

    pub fn set_completions(&mut self, items: Vec<String>) {
        self.show_completions = !items.is_empty();
        self.completions = items;
    }

    pub fn dismiss_completions(&mut self) {
        self.show_completions = false;
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

    pub fn visible_schema_items(&self) -> Vec<SchemaTreeItem> {
        flatten_tree(&self.schema_nodes)
    }

    pub fn schema_cursor_down(&mut self) {
        let max = self.visible_schema_items().len().saturating_sub(1);
        if self.schema_cursor < max {
            self.schema_cursor += 1;
        }
    }

    pub fn schema_cursor_up(&mut self) {
        self.schema_cursor = self.schema_cursor.saturating_sub(1);
    }

    pub fn schema_toggle_current(&mut self) {
        let items = self.visible_schema_items();
        if let Some(item) = items.get(self.schema_cursor) {
            let path = item.node_path.clone();
            toggle_node(&mut self.schema_nodes, &path);
        }
    }

    pub fn set_schema_nodes(&mut self, nodes: Vec<SchemaNode>) {
        self.schema_nodes = nodes;
        self.schema_cursor = 0;
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
        self.add_connection_field = AddConnectionField::Name;
    }

    pub fn close_add_connection(&mut self) {
        self.show_add_connection = false;
    }

    pub fn add_connection_tab(&mut self) {
        self.add_connection_field = match self.add_connection_field {
            AddConnectionField::Name => AddConnectionField::Url,
            AddConnectionField::Url => AddConnectionField::Name,
        };
    }

    pub fn add_connection_type_char(&mut self, ch: char) {
        match self.add_connection_field {
            AddConnectionField::Name => self.add_connection_name.push(ch),
            AddConnectionField::Url => self.add_connection_url.push(ch),
        }
    }

    pub fn add_connection_backspace(&mut self) {
        match self.add_connection_field {
            AddConnectionField::Name => {
                self.add_connection_name.pop();
            }
            AddConnectionField::Url => {
                self.add_connection_url.pop();
            }
        }
    }

    /// Validate, persist, and register the new connection. Closes dialog on success.
    pub fn save_new_connection(&mut self) -> anyhow::Result<()> {
        let name = self.add_connection_name.trim().to_string();
        let url = self.add_connection_url.trim().to_string();
        if name.is_empty() || url.is_empty() {
            anyhow::bail!("Name and URL are required");
        }
        crate::config::save_connection(&name, &url)?;
        self.available_connections
            .push(crate::config::ConnectionConfig { name, url });
        self.show_add_connection = false;
        Ok(())
    }

    /// Try to send a query to the active executor. Returns false if not connected.
    pub fn send_query(&self, query: String) -> bool {
        if let Some(tx) = &self.query_tx {
            tx.try_send(query).is_ok()
        } else {
            false
        }
    }

    /// Auto-save editor content to disk. Creates a scratch file if none is open.
    pub fn autosave(&mut self) {
        let file = match &self.current_file {
            Some(f) => f.clone(),
            None => match persistence::next_scratch_name() {
                Ok(name) => {
                    self.current_file = Some(name.clone());
                    name
                }
                Err(_) => return,
            },
        };
        let _ = persistence::save_query(&file, &self.editor_content);
    }

    /// Persist a successful query result to disk (errors are never stored).
    pub fn persist_result(&self, query: &str, result: &QueryResult) {
        let _ = persistence::save_result(query, result);
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
        assert!(matches!(s.results, ResultsPane::Empty));
        assert_eq!(s.editor_ratio, 1.0);
    }

    #[test]
    fn results_shrinks_editor() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_results(QueryResult {
            columns: vec!["id".into()],
            rows: vec![vec!["1".into()]],
        });
        assert_eq!(s.editor_ratio, 0.5);
        assert!(matches!(s.results, ResultsPane::Results(_)));
    }

    #[test]
    fn error_shrinks_editor() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_error("syntax error".into());
        assert_eq!(s.editor_ratio, 0.5);
        assert!(matches!(s.results, ResultsPane::Error(_)));
    }

    #[test]
    fn scroll_results_bounds() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_results(QueryResult {
            columns: vec!["id".into()],
            rows: vec![vec!["1".into()], vec!["2".into()], vec!["3".into()]],
        });
        assert_eq!(s.results_scroll, 0);
        s.scroll_results_down();
        assert_eq!(s.results_scroll, 1);
        s.scroll_results_down();
        assert_eq!(s.results_scroll, 2);
        // Cannot go past last row
        s.scroll_results_down();
        assert_eq!(s.results_scroll, 2);
        s.scroll_results_up();
        assert_eq!(s.results_scroll, 1);
    }

    #[test]
    fn dismiss_restores_editor() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_error("oops".into());
        s.dismiss_results();
        assert_eq!(s.editor_ratio, 1.0);
        assert!(matches!(s.results, ResultsPane::Empty));
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
