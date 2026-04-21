use std::sync::{Arc, Mutex};
use crate::lsp::Diagnostic;
use crate::highlight::HighlightSpan;
use lsp_types::DiagnosticSeverity;

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
pub enum Focus {
    #[default]
    Editor,
    Schema,
    Results,
}

#[derive(Debug, Clone, Default)]
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

#[derive(Debug, Default)]
pub struct AppState {
    pub editor_content: String,
    pub keybinding_mode: KeybindingMode,
    pub vim_mode: VimMode,
    pub focus: Focus,
    pub results: ResultsPane,
    pub schema_tree: Vec<String>,
    pub editor_ratio: f32,
    pub lsp_diagnostics: Vec<Diagnostic>,
    pub highlight_spans: Vec<HighlightSpan>,
    pub completions: Vec<String>,
    pub show_completions: bool,
    pub active_connection: Option<String>,
    pub status_message: Option<String>,
    pub results_scroll: usize,
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
        self.lsp_diagnostics.iter().any(|d| d.severity == DiagnosticSeverity::ERROR)
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some(msg.into());
    }

    pub fn clear_status(&mut self) {
        self.status_message = None;
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
}
