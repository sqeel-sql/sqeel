pub mod editor;

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseButton,
        MouseEventKind,
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
    widgets::{Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table},
};
use sqeel_core::{
    AppState, UiProvider,
    highlight::Highlighter,
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
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let keybinding_mode = state.lock().unwrap().keybinding_mode;
    let result = run_loop(&mut terminal, state, keybinding_mode).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
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

    let mut last_saved_content = String::new();

    loop {
        // Sync editor content + re-highlight
        {
            let content = editor.content();
            let spans = highlighter.highlight(&content);
            let mut s = state.lock().unwrap();
            s.editor_content = content.clone();
            s.vim_mode = editor.vim_mode;
            s.set_highlights(spans);
            // Auto-save on change
            if content != last_saved_content {
                s.autosave();
                last_saved_content = content;
            }
        }

        terminal.draw(|f| {
            let s = state.lock().unwrap();
            draw(f, &s, &editor);
        })?;

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }

        match event::read()? {
            Event::Mouse(mouse) => {
                if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                    let area = terminal.size()?;
                    let schema_width = area.width * 15 / 100;
                    let show_results = !matches!(
                        state.lock().unwrap().results,
                        sqeel_core::state::ResultsPane::Empty
                    );
                    let editor_ratio = state.lock().unwrap().editor_ratio;
                    let right_height = area.height;
                    let editor_height = if show_results {
                        (right_height as f32 * editor_ratio) as u16
                    } else {
                        right_height
                    };

                    let mut s = state.lock().unwrap();
                    if mouse.column < schema_width {
                        s.focus = Focus::Schema;
                    } else if show_results && mouse.row >= editor_height {
                        s.focus = Focus::Results;
                    } else {
                        s.focus = Focus::Editor;
                    }
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
                drop(s);

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
                        (KeyModifiers::NONE, KeyCode::Enter) => {
                            state.lock().unwrap().confirm_connection_switch();
                        }
                        _ => {}
                    }
                    continue;
                }

                // ── Normal key handling ──────────────────────────────────────────

                // Dismiss completions on Esc
                if show_completions && key.code == KeyCode::Esc {
                    state.lock().unwrap().dismiss_completions();
                    continue;
                }

                match (key.modifiers, key.code) {
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
                        let sent = state.lock().unwrap().send_query(content.clone());
                        if !sent {
                            let mut s = state.lock().unwrap();
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
                    // Trigger completions: Ctrl+Space (both modes)
                    (KeyModifiers::CONTROL, KeyCode::Char(' ')) => {
                        state.lock().unwrap().set_completions(vec![
                            "SELECT".into(),
                            "FROM".into(),
                            "WHERE".into(),
                            "JOIN".into(),
                            "GROUP BY".into(),
                        ]);
                    }
                    // Pane focus — Alt+h/j/k/l (Ctrl+H/J/K/L are swallowed by terminals)
                    (KeyModifiers::ALT, KeyCode::Char('h')) => {
                        state.lock().unwrap().focus = Focus::Schema;
                    }
                    (KeyModifiers::ALT, KeyCode::Char('l')) => {
                        state.lock().unwrap().focus = Focus::Editor;
                    }
                    (KeyModifiers::ALT, KeyCode::Char('j')) => {
                        state.lock().unwrap().focus = Focus::Results;
                    }
                    (KeyModifiers::ALT, KeyCode::Char('k')) => {
                        state.lock().unwrap().focus = Focus::Editor;
                    }
                    _ if focus == Focus::Editor => {
                        editor.handle_key(key);
                        // Dismiss completions on any text input
                        if show_completions {
                            state.lock().unwrap().dismiss_completions();
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

fn draw(f: &mut ratatui::Frame<'_>, state: &AppState, editor: &Editor) {
    let area = f.area();

    let outer = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(15), Constraint::Percentage(85)])
        .split(area);

    let schema_focused = state.focus == Focus::Schema;
    let editor_focused = state.focus == Focus::Editor;
    let results_focused = state.focus == Focus::Results;

    // Schema panel
    draw_schema(f, state, outer[0], schema_focused);

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

    draw_editor(f, state, editor, right_chunks[0], editor_focused);

    if show_results {
        draw_results(f, state, right_chunks[1], results_focused);
    }

    // Completion popup (overlay)
    if state.show_completions && !state.completions.is_empty() {
        draw_completions(f, state, right_chunks[0]);
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
}

fn draw_schema(f: &mut ratatui::Frame<'_>, state: &AppState, area: Rect, focused: bool) {
    let block = Block::default()
        .title("Schema")
        .borders(Borders::ALL)
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
        .enumerate()
        .map(|(i, item)| {
            let style = if i == state.schema_cursor && focused {
                Style::default().add_modifier(Modifier::REVERSED)
            } else if i == state.schema_cursor {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(item.label.as_str()).style(style)
        })
        .collect();

    f.render_widget(List::new(list_items).block(block), area);
}

fn draw_editor(
    f: &mut ratatui::Frame<'_>,
    state: &AppState,
    editor: &Editor,
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
        .borders(Borders::ALL)
        .border_style(if focused {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        });

    // Split editor area to leave room for diagnostic line
    let editor_chunks = if diag_line.is_some() {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(100)])
            .split(area)
    };

    let mut textarea = editor.textarea.clone();
    textarea.set_block(editor_block);
    f.render_widget(&textarea, editor_chunks[0]);

    if let Some(msg) = diag_line {
        f.render_widget(
            Paragraph::new(msg).style(Style::default().fg(Color::Red)),
            editor_chunks[1],
        );
    }
}

fn draw_results(f: &mut ratatui::Frame<'_>, state: &AppState, area: Rect, focused: bool) {
    match &state.results {
        ResultsPane::Results(r) => {
            let block = Block::default()
                .title(format!("Results ({} rows)", r.rows.len()))
                .borders(Borders::ALL)
                .border_style(if focused {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::Green)
                });

            let header_cells: Vec<Cell> = r
                .columns
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
                .borders(Borders::ALL)
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

fn draw_completions(f: &mut ratatui::Frame<'_>, state: &AppState, editor_area: Rect) {
    let items: Vec<ListItem> = state
        .completions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let style = if i == 0 {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            ListItem::new(s.as_str()).style(style)
        })
        .collect();

    let height = (items.len() as u16 + 2).min(10);
    let popup = Rect {
        x: editor_area.x + 2,
        y: editor_area.y + 2,
        width: 30.min(editor_area.width.saturating_sub(4)),
        height: height.min(editor_area.height.saturating_sub(4)),
    };

    f.render_widget(Clear, popup);
    f.render_widget(
        List::new(items).block(
            Block::default()
                .title("Completions")
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
                .title(" Connections (j/k select, Enter connect, n new, Esc close) ")
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
                ("Ctrl+Space", "Trigger completions"),
                ("q", "Quit (normal mode / schema / results)"),
            ],
        ),
        (
            "Pane Focus",
            &[
                ("Alt+H / click", "Focus schema"),
                ("Alt+L / click", "Focus editor"),
                ("Alt+J / click", "Focus results"),
                ("Alt+K / click", "Focus editor"),
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
                ("Esc", "Close"),
            ],
        ),
        (
            "Add Connection",
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
        .title(" Add Connection (Tab switch, Enter save, Esc cancel) ")
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
