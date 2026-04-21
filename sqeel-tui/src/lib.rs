pub mod editor;

use arboard::Clipboard;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers,
        KeyboardEnhancementFlags, MouseButton, MouseEventKind, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use editor::Editor;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table},
};
use sqeel_core::{
    AppState, UiProvider,
    config::load_main_config,
    highlight::Highlighter,
    lsp::{LspClient, LspEvent},
    state::{AddConnectionField, Focus, KeybindingMode, ResultsPane, VimMode},
};

pub struct TuiProvider;

impl UiProvider for TuiProvider {
    fn run(state: Arc<Mutex<AppState>>) -> anyhow::Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async_run(state))
    }
}

async fn async_run(state: Arc<Mutex<AppState>>) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let keybinding_mode = state.lock().unwrap().keybinding_mode;
    let result = run_loop(&mut terminal, state, keybinding_mode).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: Arc<Mutex<AppState>>,
    keybinding_mode: KeybindingMode,
) -> anyhow::Result<()> {
    let mut editor = Editor::new(keybinding_mode);
    let mut highlighter = Highlighter::new()?;

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
    let lsp_binary = load_main_config()
        .ok()
        .map(|c| c.editor.lsp_binary)
        .unwrap_or_else(|| "sqls".into());
    let mut lsp: Option<LspClient> = LspClient::start(&lsp_binary, None).await.ok();
    if let Some(ref mut client) = lsp {
        let _ = client.open_document(scratch_uri.clone(), "").await;
    }
    {
        let mut s = state.lock().unwrap();
        s.lsp_available = lsp.is_some();
        s.lsp_binary = lsp_binary.clone();
    }

    let mut last_saved_content = String::new();
    let mut last_lsp_content = String::new();
    let mut doc_version: i32 = 0;
    let mut last_completion_id: Option<i64> = None;
    let mut tick: u32 = 0;
    let mut command_input: Option<String> = None;

    // Clipboard held alive for the session so the OS clipboard manager sees the content.
    let mut clipboard = Clipboard::new().ok();
    // Mouse drag tracking
    let mut last_draw_areas = DrawAreas::default();
    let mut mouse_select_start: Option<(u16, u16)> = None;
    let mut mouse_did_drag = false;

    loop {
        // Drain pending tab content (set when connection loads or tab switches)
        {
            let pending = state.lock().unwrap().tab_content_pending.take();
            if let Some(content) = pending {
                editor.set_content(&content);
                last_saved_content = content.clone();
                last_lsp_content = content;
            }
        }

        // Evict cold tabs (content not accessed for 5 min released from RAM)
        state.lock().unwrap().evict_cold_tabs();

        // Sync editor content + re-highlight
        let content = editor.content();
        {
            let spans = highlighter.highlight(&content);
            let mut s = state.lock().unwrap();
            s.editor_content = content.clone();
            s.vim_mode = editor.vim_mode();
            s.set_highlights(spans);
            if content != last_saved_content {
                s.autosave();
                last_saved_content = content.clone();
            }
        }

        // Auto-complete: on every content change, notify LSP and request fresh completions
        if content != last_lsp_content {
            last_lsp_content = content.clone();
            doc_version += 1;
            if let Some(ref mut client) = lsp {
                let (row, col) = editor.textarea.cursor();
                let _ = client
                    .change_document(scratch_uri.clone(), doc_version, &content)
                    .await;

                // Suppress completions when the character immediately left of the
                // cursor is whitespace, a newline, or `;`.
                let char_left = editor.textarea.lines().get(row).and_then(|line| {
                    let before = &line[..col.min(line.len())];
                    before.chars().next_back()
                });
                let suppress = matches!(char_left, Some(c) if c.is_whitespace() || c == ';')
                    || char_left.is_none(); // cursor at start of line (after newline)

                if suppress {
                    state.lock().unwrap().dismiss_completions();
                } else if let Ok(id) = client
                    .request_completion(scratch_uri.clone(), row as u32, col as u32)
                    .await
                {
                    last_completion_id = Some(id);
                }
            }
        }

        // Drain LSP events
        if let Some(ref mut client) = lsp {
            while let Ok(event) = client.events.try_recv() {
                match event {
                    LspEvent::Diagnostics(diags) => {
                        state.lock().unwrap().set_diagnostics(diags);
                    }
                    LspEvent::Completion(id, items) => {
                        if Some(id) == last_completion_id {
                            state.lock().unwrap().set_completions(items);
                        }
                    }
                }
            }
        }

        tick = tick.wrapping_add(1);
        let tick_snap = tick;
        let cmd_snap = command_input.clone();
        terminal.draw(|f| {
            let s = state.lock().unwrap();
            last_draw_areas = draw(f, &s, &mut editor, tick_snap, cmd_snap.as_deref());
        })?;

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }

        match event::read()? {
            Event::Mouse(mouse) => {
                let area = terminal.size()?;
                let schema_width = (area.width * 15 / 100).max(30);
                let show_results = !matches!(
                    state.lock().unwrap().results,
                    sqeel_core::state::ResultsPane::Empty
                );
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
                        let pos = Position { x: mouse.column, y: mouse.row };
                        if last_draw_areas.tab_bar.contains(pos) {
                            // Click on tab bar — determine which tab
                            let rel_x = mouse.column.saturating_sub(last_draw_areas.tab_bar.x) as usize;
                            let clicked = {
                                let s = state.lock().unwrap();
                                let mut offset = 0usize;
                                let mut found = None;
                                for (i, tab) in s.tabs.iter().enumerate() {
                                    let w = tab.name.len() + 2
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
                                    s.switch_to_tab(idx);
                                    s.tab_content_pending.take()
                                };
                                if let Some(c) = content {
                                    editor.set_content(&c);
                                    last_saved_content = c.clone();
                                    last_lsp_content = c;
                                }
                            } else {
                                state.lock().unwrap().focus = Focus::Editor;
                            }
                            mouse_select_start = None;
                            mouse_did_drag = false;
                        } else {
                            state.lock().unwrap().focus = pane;
                            mouse_select_start = Some((mouse.column, mouse.row));
                            mouse_did_drag = false;
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        mouse_did_drag = true;
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if mouse_did_drag {
                            if let Some(start) = mouse_select_start {
                                let end = (mouse.column, mouse.row);
                                let s = state.lock().unwrap();
                                if let Some(text) = extract_mouse_selection(
                                    start,
                                    end,
                                    &last_draw_areas,
                                    &editor,
                                    &s,
                                ) {
                                    drop(s);
                                    if let Some(ref mut cb) = clipboard {
                                        let _ = cb.set_text(text);
                                    }
                                }
                            }
                        }
                        mouse_select_start = None;
                        mouse_did_drag = false;
                    }
                    MouseEventKind::ScrollDown => match pane {
                        Focus::Schema => state.lock().unwrap().schema_cursor_down(),
                        Focus::Results => state.lock().unwrap().scroll_results_down(),
                        Focus::Editor => editor.scroll_down(3),
                    },
                    MouseEventKind::ScrollUp => match pane {
                        Focus::Schema => state.lock().unwrap().schema_cursor_up(),
                        Focus::Results => state.lock().unwrap().scroll_results_up(),
                        Focus::Editor => editor.scroll_up(3),
                    },
                    _ => {}
                }
            }
            Event::Key(key) => {
                let s = state.lock().unwrap();
                let focus = s.focus;
                let vim_mode = s.vim_mode;
                let show_completions = s.show_completions;
                let show_switcher = s.show_connection_switcher;
                let show_add = s.show_add_connection;
                let show_help = s.show_help;
                let show_results = !matches!(s.results, sqeel_core::state::ResultsPane::Empty);
                drop(s);

                // ── Command input mode ───────────────────────────────────────────
                if command_input.is_some() {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            command_input = None;
                        }
                        (KeyModifiers::NONE, KeyCode::Backspace) => {
                            if let Some(ref mut cmd) = command_input {
                                cmd.pop();
                            }
                        }
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            let cmd_str = command_input.take().unwrap_or_default();
                            match cmd_str.trim() {
                                "q" | "q!" => break,
                                _ => {}
                            }
                        }
                        (KeyModifiers::NONE, KeyCode::Char(c)) => {
                            if let Some(ref mut cmd) = command_input {
                                cmd.push(c);
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // ── Help overlay ─────────────────────────────────────────────────
                if show_help {
                    if let (
                        KeyModifiers::NONE,
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Char('.'),
                    ) = (key.modifiers, key.code)
                    {
                        state.lock().unwrap().close_help();
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
                        (KeyModifiers::NONE, KeyCode::Char(ch)) => {
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
                                state.lock().unwrap().set_error(format!("Delete failed: {e}"));
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
                                // Prevent immediate re-trigger: content changed but we don't
                                // want completions to pop up again until the user types more.
                                last_lsp_content = editor.content();
                            }
                            continue;
                        }
                        _ => {}
                    }
                }

                match (key.modifiers, key.code) {
                    // Tab navigation (global, any mode)
                    (KeyModifiers::CONTROL, KeyCode::Right) => {
                        let content = {
                            let mut s = state.lock().unwrap();
                            s.next_tab();
                            s.tab_content_pending.take()
                        };
                        if let Some(c) = content {
                            editor.set_content(&c);
                            last_saved_content = c.clone();
                            last_lsp_content = c;
                        }
                    }
                    (KeyModifiers::CONTROL, KeyCode::Left) => {
                        let content = {
                            let mut s = state.lock().unwrap();
                            s.prev_tab();
                            s.tab_content_pending.take()
                        };
                        if let Some(c) = content {
                            editor.set_content(&c);
                            last_saved_content = c.clone();
                            last_lsp_content = c;
                        }
                    }
                    (KeyModifiers::CONTROL, KeyCode::Char('t')) => {
                        let content = {
                            let mut s = state.lock().unwrap();
                            s.new_tab();
                            s.tab_content_pending.take()
                        };
                        if let Some(c) = content {
                            editor.set_content(&c);
                            last_saved_content = c.clone();
                            last_lsp_content = c;
                        }
                    }
                    // Command mode
                    (KeyModifiers::NONE, KeyCode::Char(':'))
                        if focus != Focus::Editor || vim_mode == VimMode::Normal =>
                    {
                        command_input = Some(String::new());
                    }
                    // Global quit in vim normal mode or schema/results pane
                    (KeyModifiers::NONE, KeyCode::Char('q'))
                        if focus != Focus::Editor || vim_mode == VimMode::Normal =>
                    {
                        break;
                    }
                    // Help: ? or .
                    (KeyModifiers::NONE, KeyCode::Char('?' | '.'))
                        if focus != Focus::Editor || vim_mode == VimMode::Normal =>
                    {
                        state.lock().unwrap().open_help();
                    }
                    // Open connection switcher: Ctrl+W
                    (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
                        state.lock().unwrap().open_connection_switcher();
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
                    // Results pane navigation
                    (KeyModifiers::NONE, KeyCode::Char('j')) if focus == Focus::Results => {
                        state.lock().unwrap().scroll_results_down();
                    }
                    (KeyModifiers::NONE, KeyCode::Char('k')) if focus == Focus::Results => {
                        state.lock().unwrap().scroll_results_up();
                    }
                    (KeyModifiers::NONE, KeyCode::Char('l')) if focus == Focus::Results => {
                        state.lock().unwrap().scroll_results_right();
                    }
                    (KeyModifiers::NONE, KeyCode::Char('h')) if focus == Focus::Results => {
                        state.lock().unwrap().scroll_results_left();
                    }
                    // Dismiss results
                    (KeyModifiers::CONTROL, KeyCode::Char('c'))
                    | (KeyModifiers::NONE, KeyCode::Char('q'))
                        if focus == Focus::Results =>
                    {
                        state.lock().unwrap().dismiss_results();
                    }
                    // Execute query: Ctrl+Enter
                    (KeyModifiers::CONTROL, KeyCode::Enter) => {
                        let content = editor.content();
                        let mut s = state.lock().unwrap();
                        s.dismiss_completions();
                        let sent = s.send_query(content.clone());
                        if !sent {
                            s.push_history(&content);
                            s.set_error(
                                "No DB connected. Use --url / --connection or Ctrl+W to switch."
                                    .into(),
                            );
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
                    _ if focus == Focus::Editor => {
                        editor.handle_key(key);
                        if let Some(text) = editor.last_yank.take() {
                            if let Some(ref mut cb) = clipboard {
                                let _ = cb.set_text(text);
                            }
                        }
                    }
                    _ => {}
                }
            } // Event::Key
            _ => {} // FocusGained, FocusLost, Paste, Resize — ignore
        } // match event
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
    match state.keybinding_mode {
        KeybindingMode::Vim => match state.vim_mode {
            VimMode::Normal => Span::styled(" NORMAL ", Style::default().fg(Color::Blue)),
            VimMode::Insert => Span::styled(" INSERT ", Style::default().fg(Color::Green)),
            VimMode::Visual => Span::styled(" VISUAL ", Style::default().fg(Color::Magenta)),
        },
        KeybindingMode::Emacs => Span::styled(" EMACS  ", Style::default().fg(Color::Cyan)),
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
            Style::default().fg(Color::Red),
        ))
    } else if warnings > 0 {
        Some(Span::styled(
            format!(" ⚠ {warnings}W "),
            Style::default().fg(Color::Yellow),
        ))
    } else {
        None
    }
}

#[derive(Default, Clone, Copy)]
struct DrawAreas {
    schema: Rect,
    editor: Rect,
    tab_bar: Rect,
    results: Option<Rect>,
}

fn draw(f: &mut ratatui::Frame<'_>, state: &AppState, editor: &mut Editor, tick: u32, command_input: Option<&str>) -> DrawAreas {
    let area = f.area();

    let lsp_warn = !state.lsp_available;

    // Always reserve 1 row for the status bar; optionally 1 more for LSP warning above it.
    let (main_area, lsp_warn_area, status_area) = if lsp_warn {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)])
            .split(area);
        (chunks[0], Some(chunks[1]), chunks[2])
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);
        (chunks[0], None, chunks[1])
    };

    let outer = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(30), Constraint::Percentage(85)])
        .split(main_area);

    let schema_focused = state.focus == Focus::Schema;
    let editor_focused = state.focus == Focus::Editor;
    let results_focused = state.focus == Focus::Results;

    // Schema panel
    draw_schema(f, state, outer[0], schema_focused, tick);

    let show_results = !matches!(state.results, ResultsPane::Empty);
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

    // Tab bar is the first content row inside the editor block (border=1 on each side)
    let tab_bar = Rect {
        x: right_chunks[0].x + 1,
        y: right_chunks[0].y + 1,
        width: right_chunks[0].width.saturating_sub(2),
        height: 1,
    };
    let areas = DrawAreas {
        schema: outer[0],
        editor: right_chunks[0],
        tab_bar,
        results: if show_results { Some(right_chunks[1]) } else { None },
    };

    draw_editor(f, state, editor, right_chunks[0], editor_focused);

    if show_results {
        draw_results(f, state, right_chunks[1], results_focused);
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
    if state.show_add_connection {
        draw_add_connection(f, state, area);
    }

    // Help overlay (topmost)
    if state.show_help {
        draw_help(f, area);
    }

    // LSP warning bar (above status bar)
    if let Some(warn_area) = lsp_warn_area {
        let msg = Paragraph::new(Span::styled(
            format!(" ⚠ LSP not available ({})", state.lsp_binary),
            Style::default()
                .fg(ratatui::style::Color::Yellow)
                .bg(ratatui::style::Color::DarkGray),
        ));
        f.render_widget(msg, warn_area);
    }

    // Status bar (always at bottom)
    draw_status_bar(f, state, editor, status_area);

    // Command bar overlays the status row when active
    if let Some(cmd) = command_input {
        f.render_widget(Clear, status_area);
        f.render_widget(
            Paragraph::new(format!(":{cmd}_"))
                .style(Style::default().fg(Color::White).bg(Color::Black)),
            status_area,
        );
    }

    areas
}

fn extract_mouse_selection(
    start: (u16, u16),
    end: (u16, u16),
    areas: &DrawAreas,
    editor: &Editor,
    state: &AppState,
) -> Option<String> {
    use ratatui::layout::Position;
    let start_pos = Position { x: start.0, y: start.1 };

    let (r1, r2) = if start.1 <= end.1 {
        (start.1, end.1)
    } else {
        (end.1, start.1)
    };
    let (c1, c2) = if start.0 <= end.0 {
        (start.0, end.0)
    } else {
        (end.0, start.0)
    };

    if areas.schema.contains(start_pos) {
        let inner_top = areas.schema.y + 1;
        let row_start = r1.saturating_sub(inner_top) as usize;
        let row_end = r2.saturating_sub(inner_top) as usize;
        let items = state.visible_schema_items();
        if row_start >= items.len() {
            return None;
        }
        let row_end = row_end.min(items.len() - 1);
        let text = items[row_start..=row_end]
            .iter()
            .map(|i| i.label.trim())
            .collect::<Vec<_>>()
            .join("\n");
        return if text.is_empty() { None } else { Some(text) };
    }

    if let Some(results_area) = areas.results {
        if results_area.contains(start_pos) {
            if let sqeel_core::state::ResultsPane::Results(ref r) = state.results {
                // border (1) + header row (1) = 2 rows offset
                let inner_top = results_area.y + 2;
                let row_start = (r1.saturating_sub(inner_top) as usize)
                    .saturating_add(state.results_scroll);
                let row_end = (r2.saturating_sub(inner_top) as usize)
                    .saturating_add(state.results_scroll);
                if row_start >= r.rows.len() {
                    return None;
                }
                let row_end = row_end.min(r.rows.len() - 1);
                let col_start = state.results_col_scroll;
                let text = r.rows[row_start..=row_end]
                    .iter()
                    .map(|row| {
                        row.iter()
                            .skip(col_start)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join("\t")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                return if text.is_empty() { None } else { Some(text) };
            }
            return None;
        }
    }

    // Editor pane
    let lines = editor.textarea.lines();
    // line number width: digits in line count + 1 space
    let lnum_width = lines.len().to_string().len() as u16 + 1;
    let inner_top = areas.editor.y + 1;
    let row_start = r1.saturating_sub(inner_top) as usize;
    let row_end = r2.saturating_sub(inner_top) as usize;
    if row_start >= lines.len() {
        return None;
    }
    let row_end = row_end.min(lines.len() - 1);

    let text = if row_start == row_end {
        // Single row: extract column range, accounting for border + line numbers
        let line = &lines[row_start];
        let content_x = areas.editor.x + 1 + lnum_width;
        let col_start = c1.saturating_sub(content_x) as usize;
        let col_end = (c2.saturating_sub(content_x) as usize + 1).min(line.len());
        if col_start >= line.len() {
            line.clone()
        } else {
            line[col_start..col_end].to_string()
        }
    } else {
        lines[row_start..=row_end].join("\n")
    };

    if text.is_empty() { None } else { Some(text) }
}

fn draw_status_bar(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    editor: &Editor,
    area: Rect,
) {
    let mode = mode_label(state);
    let mode_width = mode.content.len() as u16;

    let conn = state.active_connection.as_deref().unwrap_or("no connection");
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

    let right_width = cursor_width + diag_width;
    let center_width = area.width.saturating_sub(mode_width + right_width);

    // Mode block (left)
    let mode_area = Rect { x: area.x, y: area.y, width: mode_width.min(area.width), height: 1 };
    // Center info
    let center_area = Rect {
        x: area.x + mode_width,
        y: area.y,
        width: center_width.min(area.width.saturating_sub(mode_width)),
        height: 1,
    };
    // Right side (diag + cursor)
    let right_x = area.x + mode_width + center_width;
    let diag_area = Rect { x: right_x, y: area.y, width: diag_width, height: 1 };
    let cursor_area = Rect {
        x: right_x + diag_width,
        y: area.y,
        width: cursor_width.min(area.width.saturating_sub(mode_width + center_width + diag_width)),
        height: 1,
    };

    let bar_bg = Style::default().bg(Color::DarkGray).fg(Color::White);

    // Mode label (colored fg, same bg as status bar)
    let mode_style = Style::default()
        .bg(mode.style.fg.unwrap_or(Color::Blue))
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);
    f.render_widget(Paragraph::new(Span::styled(mode.content.to_string(), mode_style)), mode_area);

    // Center: connection > tab
    f.render_widget(Paragraph::new(center_text).style(bar_bg), center_area);

    // Diagnostics
    if let Some(d) = diag {
        let diag_style = Style::default()
            .bg(d.style.fg.unwrap_or(Color::Yellow))
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD);
        f.render_widget(Paragraph::new(Span::styled(d.content.to_string(), diag_style)), diag_area);
    }

    // Cursor position (right-aligned, highlighted)
    let cursor_style = Style::default()
        .bg(Color::Blue)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);
    f.render_widget(Paragraph::new(Span::styled(cursor_str, cursor_style)), cursor_area);
}

fn draw_schema(f: &mut ratatui::Frame<'_>, state: &AppState, area: Rect, focused: bool, tick: u32) {
    const SPINNER: [&str; 4] = ["⠋", "⠙", "⠹", "⠸"];
    let status = if state.schema_loading {
        SPINNER[(tick as usize) % SPINNER.len()]
    } else if !state.schema_nodes.is_empty() {
        "✓"
    } else {
        ""
    };
    let title = if status.is_empty() {
        "Schema".to_string()
    } else {
        format!("Schema {status}")
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::TOP | Borders::RIGHT)
        .border_style(if focused {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        });

    let items = state.visible_schema_items();

    if items.is_empty() {
        f.render_widget(
            Paragraph::new(if state.active_connection.is_some() {
                "Loading..."
            } else {
                "No connection"
            })
            .block(block),
            area,
        );
        return;
    }

    let list_items: Vec<ListItem> = items
        .iter()
        .map(|item| ListItem::new(item.label.as_str()))
        .collect();

    let list = List::new(list_items)
        .block(block)
        .highlight_style(if focused {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        });

    let mut list_state = ListState::default().with_selected(Some(state.schema_cursor));
    f.render_stateful_widget(list, area, &mut list_state);
}

fn draw_editor(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    editor: &mut Editor,
    area: Rect,
    focused: bool,
) {
    let mode = mode_label(state);
    let mut title_spans = vec![Span::raw("Editor "), mode];
    if let Some(d) = diag_label(state) {
        title_spans.push(d);
    }

    // Show first diagnostic message if any
    let diag_line = state
        .lsp_diagnostics
        .first()
        .map(|d| format!(" {}:{} {}", d.line + 1, d.col + 1, d.message));

    let editor_block = Block::default()
        .title(Line::from(title_spans))
        .borders(Borders::TOP)
        .border_style(if focused {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        });

    // Render block manually so we can split the inner area ourselves
    let inner = editor_block.inner(area);
    f.render_widget(editor_block, area);

    // Split inner: tab_bar (1) + textarea + optional diag (1)
    let inner_chunks = if diag_line.is_some() {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inner)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(inner)
    };

    draw_tab_bar(f, state, inner_chunks[0]);

    // Pre-render a full-width strip at the cursor line so trailing empty cells
    // also get the highlight background (tui-textarea only styles character cells).
    let textarea_area = inner_chunks[1];
    let cursor_screen_row = editor.cursor_screen_row(textarea_area.height);
    if cursor_screen_row < textarea_area.height {
        let strip = Rect {
            x: textarea_area.x,
            y: textarea_area.y + cursor_screen_row,
            width: textarea_area.width,
            height: 1,
        };
        f.render_widget(
            Block::default().style(Style::default().bg(Color::Rgb(40, 40, 45))),
            strip,
        );
    }

    editor.textarea.set_line_number_style(Style::default().fg(Color::DarkGray));
    editor.textarea.set_cursor_line_style(Style::default().bg(Color::Rgb(40, 40, 45)));
    let _ = editor.textarea.set_search_pattern(
        r"(?i)\b(select|from|where|insert|into|values|update|set|delete|create|table|drop|alter|add|column|join|inner|outer|left|right|full|cross|on|and|or|not|null|is|in|like|between|order|by|group|having|limit|offset|union|all|distinct|as|case|when|then|else|end|if|exists|primary|foreign|key|references|unique|default|constraint|check|with|view|begin|commit|rollback|transaction|use|show|describe|explain|database|schema|index|procedure|function|returns|return|trigger|true|false)\b",
    );
    editor.textarea.set_search_style(
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    f.render_widget(&editor.textarea, inner_chunks[1]);

    if let Some(msg) = diag_line {
        f.render_widget(
            Paragraph::new(msg).style(Style::default().fg(Color::Red)),
            inner_chunks[2],
        );
    }
}

fn draw_tab_bar(f: &mut ratatui::Frame<'_>, state: &AppState, area: Rect) {
    if state.tabs.is_empty() {
        return;
    }
    let mut spans: Vec<Span> = vec![];
    for (i, tab) in state.tabs.iter().enumerate() {
        let active = i == state.active_tab;
        let style = if active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(format!(" {} ", tab.name), style));
        if i + 1 < state.tabs.len() {
            spans.push(Span::styled("│", Style::default().fg(Color::DarkGray)));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_results(f: &mut ratatui::Frame<'_>, state: &AppState, area: Rect, focused: bool) {
    match &state.results {
        ResultsPane::Results(r) => {
            let block = Block::default()
                .title(format!("Results ({} rows)", r.rows.len()))
                .borders(Borders::TOP)
                .border_style(if focused {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::Green)
                });

            let col_start = state.results_col_scroll;
            let visible_cols: Vec<&String> = r.columns.iter().skip(col_start).collect();

            let header_cells: Vec<Cell> = visible_cols
                .iter()
                .map(|c| {
                    Cell::from(c.as_str()).style(Style::default().add_modifier(Modifier::BOLD))
                })
                .collect();
            let header = Row::new(header_cells).style(Style::default().fg(Color::Cyan));

            let col_widths: Vec<Constraint> = r
                .columns
                .iter()
                .enumerate()
                .skip(col_start)
                .map(|(i, col)| {
                    let max_data = r
                        .rows
                        .iter()
                        .map(|row| row.get(i).map(|s| s.len()).unwrap_or(0))
                        .max()
                        .unwrap_or(0);
                    Constraint::Min((col.len().max(max_data) + 2) as u16)
                })
                .collect();

            let visible_rows: Vec<Row> = r
                .rows
                .iter()
                .skip(state.results_scroll)
                .map(|row| {
                    Row::new(
                        row.iter()
                            .skip(col_start)
                            .map(|c| Cell::from(c.as_str()))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect();

            let table = Table::new(visible_rows, col_widths)
                .header(header)
                .block(block);

            f.render_widget(table, area);
        }
        ResultsPane::Error(e) => {
            let block = Block::default()
                .title("Error")
                .borders(Borders::TOP)
                .border_style(if focused {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::Red)
                });
            f.render_widget(
                Paragraph::new(e.as_str())
                    .style(Style::default().fg(Color::Red))
                    .block(block),
                area,
            );
        }
        ResultsPane::Empty => unreachable!(),
    }
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
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        popup,
    );
}

fn draw_connection_switcher(f: &mut ratatui::Frame<'_>, state: &AppState, area: Rect) {
    let conns = &state.available_connections;
    let cursor = state.connection_switcher_cursor;

    let items: Vec<ListItem> = if conns.is_empty() {
        vec![ListItem::new("No connections configured")]
    } else {
        conns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let style = if i == cursor {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                ListItem::new(format!("{} — {}", c.name, c.url)).style(style)
            })
            .collect()
    };

    let width = 60.min(area.width.saturating_sub(4));
    let height = (items.len() as u16 + 2)
        .min(20)
        .min(area.height.saturating_sub(4));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(Clear, popup);
    f.render_widget(
        List::new(items).block(
            Block::default()
                .title(" Connections (j/k  Enter connect  n new  e edit  d delete  Esc close) ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        ),
        popup,
    );
}

fn draw_help(f: &mut ratatui::Frame<'_>, area: Rect) {
    const SECTIONS: &[(&str, &[(&str, &str)])] = &[
        (
            "Global",
            &[
                ("?  /  .", "Open this help (normal mode)"),
                ("Ctrl+Enter", "Execute query"),
                ("Ctrl+W", "Connection switcher"),
                ("q", "Quit (normal mode / schema / results)"),
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
            "Schema Pane",
            &[
                ("j / k", "Navigate up / down"),
                ("Enter / l", "Expand / collapse node"),
            ],
        ),
        (
            "Results Pane",
            &[
                ("j / k", "Scroll down / up"),
                ("q / Ctrl+C", "Dismiss results"),
            ],
        ),
        (
            "Tabs",
            &[
                ("Ctrl+T", "New scratch tab"),
                ("Ctrl+Right", "Next tab"),
                ("Ctrl+Left", "Prev tab"),
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
            "Editor — Emacs",
            &[
                ("Ctrl+B / F", "Cursor left / right"),
                ("Ctrl+P / N", "Cursor / history up / down"),
                ("Ctrl+A / E", "Start / end of line"),
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
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let mut lines: Vec<ratatui::text::Line<'static>> = vec![];
    for (section, items) in SECTIONS {
        lines.push(ratatui::text::Line::from(Span::styled(
            format!(" {section}"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for (key, desc) in *items {
            let pad = 20usize.saturating_sub(key.len());
            lines.push(ratatui::text::Line::from(vec![
                Span::styled(format!("  {key}"), Style::default().fg(Color::Yellow)),
                Span::raw(" ".repeat(pad)),
                Span::raw(*desc),
            ]));
        }
        lines.push(ratatui::text::Line::raw(""));
    }

    f.render_widget(Paragraph::new(lines).style(Style::default()), inner);
}

fn draw_add_connection(f: &mut ratatui::Frame<'_>, state: &AppState, area: Rect) {
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
        .border_style(Style::default().fg(Color::Green));
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
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let url_style = if state.add_connection_field == AddConnectionField::Url {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    f.render_widget(
        Paragraph::new(format!("Name : {}_", state.add_connection_name)).style(name_style),
        rows[0],
    );
    f.render_widget(
        Paragraph::new(format!("URL  : {}_", state.add_connection_url)).style(url_style),
        rows[1],
    );
    f.render_widget(
        Paragraph::new("Only letters, digits, - and _ allowed in name")
            .style(Style::default().fg(Color::DarkGray)),
        rows[2],
    );
}

#[cfg(test)]
mod tests {
    use crate::editor::Editor;
    use sqeel_core::{
        AppState,
        state::{Focus, KeybindingMode, QueryResult},
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
    fn emacs_editor_mode() {
        let editor = Editor::new(KeybindingMode::Emacs);
        assert_eq!(editor.keybinding_mode, KeybindingMode::Emacs);
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
