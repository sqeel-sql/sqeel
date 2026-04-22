mod clipboard;
mod completion_thread;
pub mod editor;
mod highlight_thread;
mod theme;

use clipboard::Clipboard;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use completion_thread::CompletionThread;
use crossterm::{
    cursor::SetCursorStyle,
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers,
        KeyboardEnhancementFlags, MouseButton, MouseEventKind, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use editor::Editor;
use highlight_thread::HighlightThread;
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
    config::load_main_config,
    highlight::{
        HighlightSpan, Highlighter, TokenKind, first_syntax_error, statement_at_byte,
        statement_ranges, strip_sql_comments,
    },
    lsp::{LspClient, LspEvent},
    schema::{self, SchemaTreeItem},
    state::{AddConnectionField, Focus, KeybindingMode, ResultsCursor, ResultsPane, VimMode},
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
    let mut lsp: Option<LspClient> = LspClient::start(&lsp_binary, None).await.ok();
    if let Some(ref mut client) = lsp {
        let _ = client.open_document(scratch_uri.clone(), "").await;
    }
    {
        let mut s = state.lock().unwrap();
        s.lsp_available = lsp.is_some();
        s.lsp_binary = lsp_binary.clone();
    }

    let mut editor_dirty = false;
    let mut last_save_time = Instant::now();
    let mut doc_version: i32 = 0;
    let mut last_completion_id: Option<i64> = None;
    let mut last_schema_completions: Vec<String> = Vec::new();
    let mut tick: u32 = 0;
    let mut command_input: Option<TextInput> = None;
    let mut rename_input: Option<TextInput> = None;
    let mut file_picker: Option<FilePicker> = None;
    let mut delete_confirm: Option<String> = None;
    let mut schema_search =
        SchemaSearch::from_initial(state.lock().unwrap().schema_search_query.clone());
    let mut editor_search: Option<TextInput> = None;
    let mut last_editor_search: Option<String> = None;

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
    // Force redraw on first iteration and after every event.
    let mut event_triggered_redraw = true;
    let mut last_terminal_size = terminal.size()?;
    let mut last_schema_loading = false;
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

        // Drain pending tab content (set when connection loads or tab switches)
        {
            let pending = state.lock().unwrap().tab_content_pending.take();
            if let Some(content) = pending {
                editor.set_content(&content);
                editor_dirty = false;
                needs_redraw = true;
            }
        }

        // Evict cold tabs (content not accessed for 5 min released from RAM)
        state.lock().unwrap().evict_cold_tabs();

        // Sync editor content + submit to highlight thread when changed.
        let content_changed = editor.take_dirty();
        if content_changed {
            needs_redraw = true;
        }
        let content: Option<Arc<String>> = if content_changed {
            Some(Arc::new(editor.content()))
        } else {
            None
        };
        {
            let mut s = state.lock().unwrap();
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
                editor_dirty = true;
            }
            // Apply any completed highlight results from the background thread.
            if let Some(spans) = highlight_thread.try_recv() {
                let row_count = editor.textarea.lines().len();
                editor
                    .textarea
                    .set_syntax_spans(syntax_spans_by_row(&spans, row_count));
                s.set_highlights(spans);
                needs_redraw = true;
            }
            if editor_dirty && last_save_time.elapsed() >= Duration::from_millis(1000) {
                s.autosave();
                editor_dirty = false;
                last_save_time = Instant::now();
            }
        }
        if let Some(ref c) = content {
            highlight_thread.submit(c.clone());
        }

        // Auto-complete: on every content change, submit a schema completion query to the
        // background thread and (if LSP is available) request supplemental completions.
        if let Some(ref content) = content {
            doc_version += 1;

            let (row, col) = editor.textarea.cursor();

            // Suppress completions when the character immediately left of the
            // cursor is whitespace, a newline, or `;`.
            let char_left = editor.textarea.lines().get(row).and_then(|line| {
                let before = &line[..col.min(line.len())];
                before.chars().next_back()
            });
            let suppress = matches!(char_left, Some(c) if c.is_whitespace() || c == ';')
                || char_left.is_none();

            if suppress {
                state.lock().unwrap().dismiss_completions();
                last_completion_id = None;
            } else {
                let prefix = word_prefix_at(editor.textarea.lines(), row, col);
                let identifiers = state.lock().unwrap().schema_identifier_names();
                completion_thread.submit(prefix, identifiers);

                if let Some(ref mut client) = lsp {
                    let _ = client
                        .change_document(scratch_uri.clone(), doc_version, content)
                        .await;
                    if let Ok(id) = client
                        .request_completion(scratch_uri.clone(), row as u32, col as u32)
                        .await
                    {
                        last_completion_id = Some(id);
                    }
                }
            }
        }

        // Poll schema completion thread results.
        if let Some(schema_items) = completion_thread.try_recv() {
            last_schema_completions = schema_items.clone();
            state.lock().unwrap().set_completions(schema_items);
            needs_redraw = true;
        }

        // Drain LSP events
        if let Some(ref mut client) = lsp {
            while let Ok(event) = client.events.try_recv() {
                needs_redraw = true;
                match event {
                    LspEvent::Diagnostics(diags) => {
                        state.lock().unwrap().set_diagnostics(diags);
                    }
                    LspEvent::Completion(id, lsp_items) => {
                        if Some(id) == last_completion_id {
                            // LSP results lead; schema identifiers fill in any gaps.
                            let mut merged = lsp_items;
                            for item in &last_schema_completions {
                                if !merged.contains(item) {
                                    merged.push(item.clone());
                                }
                            }
                            state.lock().unwrap().set_completions(merged);
                        }
                    }
                }
            }
        }

        // Spinner needs periodic redraws while schema is loading, plus one final
        // redraw on the loading→idle transition so the spinner is replaced by the ✓.
        let schema_loading = state.lock().unwrap().schema_loading;
        if schema_loading || last_schema_loading != schema_loading {
            needs_redraw = true;
        }
        last_schema_loading = schema_loading;

        // Executor finished a query — redraw to show results/error.
        {
            let mut s = state.lock().unwrap();
            if s.results_dirty {
                needs_redraw = true;
                s.results_dirty = false;
            }
        }

        if needs_redraw {
            tick = tick.wrapping_add(1);
            let tick_snap = tick;
            let cmd_snap = command_input.clone();
            let rename_snap = rename_input.clone();
            let picker_snap = file_picker.clone();
            let delete_snap = delete_confirm.clone();
            let schema_search_snap = schema_search.clone();
            let editor_search_snap = editor_search.clone();
            let last_editor_search_snap = last_editor_search.clone();
            let editor_search_text_snap: Option<String> =
                editor_search_snap.as_ref().map(|t| t.text.clone());
            let toast_snap: Vec<(String, ToastKind)> = toasts
                .iter()
                .map(|(msg, kind, _)| (msg.clone(), *kind))
                .collect();
            terminal.draw(|f| {
                let s = state.lock().unwrap();
                last_draw_areas = draw(
                    f,
                    &s,
                    &mut editor,
                    tick_snap,
                    cmd_snap.as_ref(),
                    rename_snap.as_ref(),
                    picker_snap.as_ref(),
                    delete_snap.as_deref(),
                    &schema_search_snap,
                    editor_search_snap.as_ref(),
                    editor_search_text_snap.as_deref(),
                    last_editor_search_snap.as_deref(),
                    &toast_snap,
                );
            })?;
            // Apply the cursor shape requested by draw(). Hidden is handled by
            // ratatui (no set_cursor_position call leaves the cursor hidden).
            match last_draw_areas.cursor_shape {
                CursorShape::Bar => {
                    let _ = execute!(terminal.backend_mut(), SetCursorStyle::SteadyBar);
                }
                CursorShape::Block => {
                    let _ = execute!(terminal.backend_mut(), SetCursorStyle::SteadyBlock);
                }
                CursorShape::Hidden => {}
            }
            last_terminal_size = terminal.size()?;
        }

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }

        event_triggered_redraw = true;
        match event::read()? {
            Event::Mouse(mouse) => {
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
                                        s.editor_content = Arc::new(editor.content());
                                        s.autosave();
                                        editor_dirty = false;
                                        last_save_time = Instant::now();
                                    }
                                    s.switch_to_tab(idx);
                                    s.tab_content_pending.take()
                                };
                                if let Some(c) = content {
                                    editor.set_content(&c);
                                    editor_dirty = false;
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
                                {
                                    clipboard.set_text(&text);
                                }
                                toasts.push((
                                    format!("{label} copied to clipboard"),
                                    ToastKind::Info,
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
                                {
                                    clipboard.set_text(&text);
                                }
                                toasts.push((
                                    "Row copied to clipboard".to_string(),
                                    ToastKind::Info,
                                    std::time::Instant::now(),
                                ));
                            }
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        let mut s = state.lock().unwrap();
                        if s.show_help {
                            s.help_scroll = s.help_scroll.saturating_add(mouse_scroll_lines as u16);
                        } else {
                            s.focus = pane;
                            match pane {
                                Focus::Schema => {
                                    schema_search.focused = false;
                                    if schema_search.is_filtering() {
                                        for _ in 0..mouse_scroll_lines {
                                            schema_search
                                                .cursor_down(last_draw_areas.schema_list_count);
                                        }
                                    } else {
                                        for _ in 0..mouse_scroll_lines {
                                            s.schema_cursor_down();
                                        }
                                    }
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
                                    if schema_search.is_filtering() {
                                        for _ in 0..mouse_scroll_lines {
                                            schema_search.cursor_up();
                                        }
                                    } else {
                                        for _ in 0..mouse_scroll_lines {
                                            s.schema_cursor_up();
                                        }
                                    }
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
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::Key(key) => {
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
                let s = state.lock().unwrap();
                let focus = s.focus;
                let vim_mode = s.vim_mode;
                let show_completions = s.show_completions;
                let show_switcher = s.show_connection_switcher;
                let show_add = s.show_add_connection;
                let show_help = s.show_help;
                let show_results = s.has_results();
                drop(s);

                // ── Leader-key chord ─────────────────────────────────────────────
                // Eligible context: no modal open, schema search box not focused,
                // and either we're outside the editor or in Vim Normal mode.
                let leader_eligible = command_input.is_none()
                    && rename_input.is_none()
                    && file_picker.is_none()
                    && delete_confirm.is_none()
                    && editor_search.is_none()
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
                                    editor_dirty = false;
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

                // ── Command input mode ───────────────────────────────────────────
                if let Some(ref mut cmd) = command_input {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            command_input = None;
                        }
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            let cmd_str = command_input.take().unwrap_or_default().text;
                            let trimmed = cmd_str.trim();
                            if let Ok(line) = trimmed.parse::<usize>() {
                                state.lock().unwrap().focus = Focus::Editor;
                                editor.goto_line(line);
                            } else {
                                match trimmed {
                                    "q" | "q!" => break,
                                    other => {
                                        toasts.push((
                                            format!("Unknown command: :{other}"),
                                            ToastKind::Error,
                                            std::time::Instant::now(),
                                        ));
                                    }
                                }
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
                                        s.editor_content = Arc::new(editor.content());
                                        s.autosave();
                                        editor_dirty = false;
                                        last_save_time = Instant::now();
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
                        }
                        (KeyModifiers::NONE, code) if schema_search.handle_nav(code) => {}
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

                // ── Editor search input ──────────────────────────────────────────
                if editor_search.is_some() {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            let q = editor_search.take().map(|t| t.text).unwrap_or_default();
                            last_editor_search = if q.is_empty() { None } else { Some(q) };
                        }
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            let q = editor_search.take().map(|t| t.text).unwrap_or_default();
                            if !q.is_empty() {
                                let _ = editor.textarea.set_search_pattern(&q);
                                editor.textarea.search_forward(false);
                                last_editor_search = Some(q);
                            } else {
                                last_editor_search = None;
                            }
                        }
                        (KeyModifiers::NONE, KeyCode::Backspace) => {
                            if let Some(ref mut q) = editor_search {
                                q.backspace();
                                let _ = editor.textarea.set_search_pattern(&q.text);
                            }
                        }
                        (KeyModifiers::NONE, KeyCode::Char(c)) => {
                            if let Some(ref mut q) = editor_search {
                                q.insert_char(c);
                                let _ = editor.textarea.set_search_pattern(&q.text);
                            }
                        }
                        _ => {
                            if let Some(ref mut q) = editor_search {
                                q.handle_nav(key.code);
                            }
                        }
                    }
                    continue;
                }

                // ── Help overlay ─────────────────────────────────────────────────
                if show_help {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            state.lock().unwrap().close_help();
                        }
                        (KeyModifiers::NONE, KeyCode::Char('j') | KeyCode::Down) => {
                            let mut s = state.lock().unwrap();
                            s.help_scroll = s.help_scroll.saturating_add(1);
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
                                editor.force_normal();
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
                                s.editor_content = Arc::new(editor.content());
                                s.autosave();
                                editor_dirty = false;
                                last_save_time = Instant::now();
                            }
                            s.next_tab();
                            s.tab_content_pending.take()
                        };
                        if let Some(c) = content {
                            editor.set_content(&c);
                            editor_dirty = false;
                        }
                    }
                    (KeyModifiers::SHIFT, KeyCode::Char('H'))
                        if focus != Focus::Editor || vim_mode == VimMode::Normal =>
                    {
                        let content = {
                            let mut s = state.lock().unwrap();
                            if editor_dirty {
                                s.editor_content = Arc::new(editor.content());
                                s.autosave();
                                editor_dirty = false;
                                last_save_time = Instant::now();
                            }
                            s.prev_tab();
                            s.tab_content_pending.take()
                        };
                        if let Some(c) = content {
                            editor.set_content(&c);
                            editor_dirty = false;
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
                    (KeyModifiers::NONE, KeyCode::Enter | KeyCode::Char('l'))
                        if focus == Focus::Schema =>
                    {
                        state.lock().unwrap().schema_toggle_current();
                    }
                    // Schema search
                    (KeyModifiers::NONE, KeyCode::Char('/')) if focus == Focus::Schema => {
                        schema_search.start();
                    }
                    // Results pane navigation
                    (KeyModifiers::NONE, KeyCode::Char('j')) if focus == Focus::Results => {
                        state.lock().unwrap().results_cursor_down();
                    }
                    (KeyModifiers::NONE, KeyCode::Char('k')) if focus == Focus::Results => {
                        state.lock().unwrap().results_cursor_up();
                    }
                    (KeyModifiers::NONE, KeyCode::Char('l')) if focus == Focus::Results => {
                        state.lock().unwrap().results_cursor_right();
                    }
                    (KeyModifiers::NONE, KeyCode::Char('h')) if focus == Focus::Results => {
                        state.lock().unwrap().results_cursor_left();
                    }
                    (KeyModifiers::NONE, KeyCode::Char('y')) if focus == Focus::Results => {
                        let now = std::time::Instant::now();
                        let is_yy = pending_results_y
                            .is_some_and(|t| now.duration_since(t).as_millis() < 500);
                        let yanked = if is_yy {
                            state.lock().unwrap().results_cursor_yank_row()
                        } else {
                            state.lock().unwrap().results_cursor_yank()
                        };
                        pending_results_y = if is_yy { None } else { Some(now) };
                        if let Some((text, label)) = yanked {
                            {
                                clipboard.set_text(&text);
                            }
                            toasts.push((
                                format!("{label} copied to clipboard"),
                                ToastKind::Info,
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
                        if strip_sql_comments(&stmt).trim().is_empty() {
                            // nothing to run on empty/whitespace-only content
                        } else if let Some(err) = first_syntax_error(&stmt) {
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
                        if stmts.is_empty() {
                            // nothing to run on empty/whitespace-only content
                        } else if let Some(err) = first_syntax_error(&content) {
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
                        }
                    }
                    (KeyModifiers::CONTROL, KeyCode::Char('n')) if focus == Focus::Editor => {
                        let recalled = state.lock().unwrap().history_next().map(|s| s.to_owned());
                        if let Some(q) = recalled {
                            editor.set_content(&q);
                        } else {
                            editor.set_content("");
                        }
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
                    // Editor vim search: / in Normal mode
                    (KeyModifiers::NONE, KeyCode::Char('/'))
                        if focus == Focus::Editor && vim_mode == VimMode::Normal =>
                    {
                        editor_search = Some(TextInput::default());
                        last_editor_search = None;
                        let _ = editor.textarea.set_search_pattern("");
                    }
                    // n / N — navigate search matches
                    (KeyModifiers::NONE, KeyCode::Char('n'))
                        if focus == Focus::Editor
                            && vim_mode == VimMode::Normal
                            && last_editor_search.is_some() =>
                    {
                        editor.textarea.search_forward(false);
                    }
                    (KeyModifiers::SHIFT, KeyCode::Char('N'))
                    | (KeyModifiers::NONE, KeyCode::Char('N'))
                        if focus == Focus::Editor
                            && vim_mode == VimMode::Normal
                            && last_editor_search.is_some() =>
                    {
                        editor.textarea.search_back(false);
                    }
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
                            {
                                clipboard.set_text(&text);
                            }
                            toasts.push((
                                "Yanked to clipboard".to_string(),
                                ToastKind::Info,
                                std::time::Instant::now(),
                            ));
                        }
                    }
                    _ => {}
                }
            } // Event::Key
            Event::Resize(_, _) => {
                terminal.autoresize()?;
            }
            _ => {} // FocusGained, FocusLost, Paste — ignore
        } // match event
    }
    {
        let mut s = state.lock().unwrap();
        s.editor_content = Arc::new(editor.content());
        s.autosave_all();
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
}

#[allow(clippy::too_many_arguments)]
fn draw(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    editor: &mut Editor,
    tick: u32,
    command_input: Option<&TextInput>,
    rename_input: Option<&TextInput>,
    file_picker: Option<&FilePicker>,
    delete_confirm: Option<&str>,
    schema_search: &SchemaSearch,
    editor_search: Option<&TextInput>,
    editor_search_text: Option<&str>,
    last_editor_search: Option<&str>,
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
    ) = draw_schema(f, state, outer[0], schema_focused, tick, schema_search);

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
        draw_results(f, state, right_chunks[1], results_focused, tick);
    }

    // Completion popup (overlay)
    if state.show_completions && !state.completions.is_empty() {
        let (cur_row, cur_col) = editor.textarea.cursor();
        draw_completions(f, state, right_chunks[0], cur_row, cur_col);
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
        draw_help(f, area, state.help_scroll);
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

    // Editor `/` search: same shape as command palette.
    if let Some(query) = editor_search {
        dialog_cursor = Some(draw_input_dialog(f, area, "/ ", query));
    }

    // Delete confirmation: centered borderless dialog.
    if let Some(name) = delete_confirm {
        draw_confirm_dialog(f, area, &format!("Delete '{name}'?  (y / n)"));
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

fn draw_schema(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    area: Rect,
    focused: bool,
    tick: u32,
    search: &SchemaSearch,
) -> (Rect, usize, usize, bool, Option<(u16, u16)>) {
    let searching = search.focused;
    let search_cursor = search.cursor;
    const SPINNER: [&str; 4] = ["⠋", "⠙", "⠹", "⠸"];
    let status = if state.schema_loading {
        SPINNER[(tick as usize) % SPINNER.len()]
    } else if !state.schema_nodes.is_empty() {
        "✓"
    } else {
        ""
    };
    let title = if state.schema_loading || state.schema_nodes.is_empty() {
        format!("Explorer {status}").trim_end().to_string()
    } else {
        let count = state.schema_nodes.len();
        format!("Explorer ({count})")
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

    let list_items: Vec<ListItem> = items
        .iter()
        .map(|item| ListItem::new(item.label.as_str()))
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

    let list = List::new(list_items).highlight_style(highlight_style);
    let mut list_state = ListState::default().with_selected(selected);
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
    f.render_widget(&editor.textarea, chunks[1]);

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
    tick: u32,
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

            let build_header = || -> Line<'static> {
                let mut spans: Vec<Span<'static>> = Vec::with_capacity(r.columns.len() * 2);
                for (i, c) in r.columns.iter().enumerate() {
                    let w = r.col_widths.get(i).copied().unwrap_or(0) as usize;
                    let inner = w.saturating_sub(1);
                    let mut st = header_style;
                    if cursor == Some(ResultsCursor::Header(i)) {
                        st = st.bg(cursor_bg);
                    } else if active_col == Some(i) {
                        st = st.bg(col_bg);
                    }
                    spans.push(Span::styled(format!(" {:<inner$}", c, inner = inner), st));
                    if i + 1 < r.columns.len() {
                        spans.push(Span::styled("│".to_string(), sep_style));
                    }
                }
                Line::from(spans)
            };

            let build_row = |row_idx: usize, row: &Vec<String>| -> Line<'static> {
                let mut spans: Vec<Span<'static>> = Vec::with_capacity(r.columns.len() * 2);
                for i in 0..r.columns.len() {
                    let w = r.col_widths.get(i).copied().unwrap_or(0) as usize;
                    let inner = w.saturating_sub(1);
                    let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
                    let text = format!(" {:<inner$}", cell, inner = inner);
                    let is_cursor = cursor_row == Some(row_idx) && active_col == Some(i);
                    let bg_style = if is_cursor {
                        Some(cursor_bg)
                    } else if active_col == Some(i) {
                        Some(col_bg)
                    } else {
                        None
                    };
                    if let Some(bg) = bg_style {
                        spans.push(Span::styled(text, Style::default().bg(bg)));
                    } else {
                        spans.push(Span::raw(text));
                    }
                    if i + 1 < r.columns.len() {
                        spans.push(Span::styled("│".to_string(), sep_style));
                    }
                }
                Line::from(spans)
            };

            let body_lines: Vec<Line<'static>> = r
                .rows
                .iter()
                .enumerate()
                .skip(state.results_scroll())
                .map(|(i, row)| build_row(i, row))
                .collect();

            let query_text = state
                .active_result()
                .map(|t| t.query.clone())
                .unwrap_or_default();
            let mut query_line = highlight_query_line(&query_text);
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
                Paragraph::new(build_header()).scroll((0, char_offset)),
                chunks[5],
            );
            f.render_widget(Paragraph::new(hr).style(sep_style), chunks[6]);
            let body_area = chunks[7];
            state
                .results_body_rows
                .store(body_area.height, std::sync::atomic::Ordering::Relaxed);
            state
                .results_body_width
                .store(body_area.width, std::sync::atomic::Ordering::Relaxed);
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
            const SPINNER: [&str; 4] = ["⠋", "⠙", "⠹", "⠸"];
            let frame = SPINNER[(tick as usize) % SPINNER.len()];
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
        let mut query_line = highlight_query_line(&query_text);
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
fn highlight_query_line(query: &str) -> Line<'static> {
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
            .map(|h| h.highlight(query))
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

/// Convert tree-sitter spans (row+col are byte offsets within the row) into
/// the per-row `(start_byte, end_byte, style)` shape tui-textarea expects.
/// Multi-row spans are split per row. Spans with no styling are skipped.
fn syntax_spans_by_row(
    spans: &[HighlightSpan],
    row_count: usize,
) -> Vec<Vec<(usize, usize, Style)>> {
    let mut by_row: Vec<Vec<(usize, usize, Style)>> = vec![Vec::new(); row_count];
    for s in spans {
        let Some(style) = token_kind_style(s.kind) else {
            continue;
        };
        if s.start_row >= row_count {
            continue;
        }
        if s.start_row == s.end_row {
            if s.end_col > s.start_col {
                by_row[s.start_row].push((s.start_col, s.end_col, style));
            }
        } else {
            // Multi-row span: emit a segment per row. Use usize::MAX as "to
            // end of line" — clamped in line_spans against actual line length.
            by_row[s.start_row].push((s.start_col, usize::MAX, style));
            for row_spans in by_row
                .iter_mut()
                .take(s.end_row.min(row_count))
                .skip(s.start_row + 1)
            {
                row_spans.push((0, usize::MAX, style));
            }
            if s.end_row < row_count && s.end_col > 0 {
                by_row[s.end_row].push((0, s.end_col, style));
            }
        }
    }
    // tree-sitter visits parents before children; child spans for an
    // identifier/keyword often nest inside larger ones. Sort and de-overlap by
    // taking the last (innermost) style for any byte range.
    for row_spans in &mut by_row {
        row_spans.sort_by_key(|&(s, _, _)| s);
    }
    by_row
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
        .enumerate()
        .map(|(i, s)| {
            let style = if i == cursor {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            ListItem::new(s.as_str()).style(style)
        })
        .collect();

    let popup_w = 30u16.min(editor_area.width.saturating_sub(2));
    let popup_h = (items.len() as u16 + 2).min(10);

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

    f.render_widget(Clear, popup);
    f.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ui().completion_border)),
        ),
        popup,
    );
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

fn draw_help(f: &mut ratatui::Frame<'_>, area: Rect, scroll: u16) {
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
                ("j / k", "Scroll down / up"),
                ("h / l", "Scroll left / right"),
                ("H / L", "Prev / next result tab"),
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

    let width = 62.min(area.width.saturating_sub(4));
    let total_rows: u16 = SECTIONS
        .iter()
        .map(|(_, items)| items.len() as u16 + 2)
        .sum::<u16>()
        + 1;
    let height = total_rows.min(area.height.saturating_sub(4));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Help  (Esc / q / ? to close) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ui().dialog_border));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let scroll = scroll.min(total_rows.saturating_sub(inner.height));

    let mut lines: Vec<ratatui::text::Line<'static>> = vec![];
    for (section, items) in SECTIONS {
        lines.push(ratatui::text::Line::from(Span::styled(
            format!(" {section}"),
            Style::default()
                .fg(ui().dialog_border)
                .add_modifier(Modifier::BOLD),
        )));
        for (key, desc) in *items {
            let pad = 20usize.saturating_sub(key.len());
            lines.push(ratatui::text::Line::from(vec![
                Span::styled(format!("  {key}"), Style::default().fg(ui().completion_key)),
                Span::raw(" ".repeat(pad)),
                Span::raw(*desc),
            ]));
        }
        lines.push(ratatui::text::Line::raw(""));
    }

    f.render_widget(
        Paragraph::new(lines)
            .style(Style::default())
            .scroll((scroll, 0)),
        inner,
    );
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
}
