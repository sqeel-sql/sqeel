pub mod editor;

use std::sync::{Arc, Mutex};
use std::io;
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table},
    Terminal,
};
use sqeel_core::{
    AppState, UiProvider,
    highlight::Highlighter,
    state::{Focus, KeybindingMode, ResultsPane, VimMode},
};
use editor::Editor;

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
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let keybinding_mode = state.lock().unwrap().keybinding_mode;
    let result = run_loop(&mut terminal, state, keybinding_mode).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
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

    loop {
        // Sync editor content + re-highlight
        {
            let content = editor.content();
            let spans = highlighter.highlight(&content);
            let mut s = state.lock().unwrap();
            s.editor_content = content;
            s.vim_mode = editor.vim_mode;
            s.set_highlights(spans);
        }

        terminal.draw(|f| {
            let s = state.lock().unwrap();
            draw(f, &s, &editor);
        })?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                let s = state.lock().unwrap();
                let focus = s.focus;
                let vim_mode = s.vim_mode;
                let show_completions = s.show_completions;
                drop(s);

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
                        // M3: actual query execution here
                        state.lock().unwrap().set_error(
                            format!("No DB connected. Query was:\n{content}"),
                        );
                    }
                    // Trigger completions: Ctrl+Space (both modes)
                    (KeyModifiers::CONTROL, KeyCode::Char(' ')) => {
                        // M2.5: LSP completion request goes here
                        // For now show placeholder
                        state.lock().unwrap().set_completions(vec![
                            "SELECT".into(),
                            "FROM".into(),
                            "WHERE".into(),
                            "JOIN".into(),
                            "GROUP BY".into(),
                        ]);
                    }
                    // Pane focus
                    (KeyModifiers::CONTROL, KeyCode::Char('h')) => {
                        state.lock().unwrap().focus = Focus::Schema;
                    }
                    (KeyModifiers::CONTROL, KeyCode::Char('l')) => {
                        state.lock().unwrap().focus = Focus::Editor;
                    }
                    (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
                        state.lock().unwrap().focus = Focus::Results;
                    }
                    (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
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
            }
        }
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
    let errors = state.lsp_diagnostics.iter().filter(|d| {
        d.severity == lsp_types::DiagnosticSeverity::ERROR
    }).count();
    let warnings = state.lsp_diagnostics.iter().filter(|d| {
        d.severity == lsp_types::DiagnosticSeverity::WARNING
    }).count();
    if errors > 0 {
        Some(Span::styled(format!(" ✖ {errors}E "), Style::default().fg(Color::Red)))
    } else if warnings > 0 {
        Some(Span::styled(format!(" ⚠ {warnings}W "), Style::default().fg(Color::Yellow)))
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

    if state.schema_tree.is_empty() {
        f.render_widget(
            Paragraph::new("No connection").block(block),
            area,
        );
    } else {
        let items: Vec<ListItem> = state
            .schema_tree
            .iter()
            .map(|s| ListItem::new(s.as_str()))
            .collect();
        f.render_widget(List::new(items).block(block), area);
    }
}

fn draw_editor(f: &mut ratatui::Frame<'_>, state: &AppState, editor: &Editor, area: Rect, focused: bool) {
    let mode = mode_label(state);
    let mut title_spans = vec![Span::raw("Editor "), mode];
    if let Some(d) = diag_label(state) {
        title_spans.push(d);
    }

    // Show first diagnostic message if any
    let diag_line = state.lsp_diagnostics.first().map(|d| {
        format!(" {}:{} {}", d.line + 1, d.col + 1, d.message)
    });

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
                .map(|c| Cell::from(c.as_str()).style(Style::default().add_modifier(Modifier::BOLD)))
                .collect();
            let header = Row::new(header_cells).style(Style::default().fg(Color::Cyan));

            let col_widths: Vec<Constraint> = r
                .columns
                .iter()
                .enumerate()
                .map(|(i, col)| {
                    let max_data = r.rows.iter()
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
                .map(|row| Row::new(row.iter().map(|c| Cell::from(c.as_str())).collect::<Vec<_>>()))
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

#[cfg(test)]
mod tests {
    use sqeel_core::{AppState, state::{Focus, KeybindingMode, QueryResult}};
    use crate::editor::Editor;

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
        s.set_results(QueryResult { columns: vec!["col".into()], rows: vec![vec!["val".into()]] });
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
        use sqeel_core::lsp::Diagnostic;
        use lsp_types::DiagnosticSeverity;
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_diagnostics(vec![Diagnostic {
            line: 0, col: 5,
            message: "unexpected token".into(),
            severity: DiagnosticSeverity::ERROR,
        }]);
        assert!(s.has_errors());
    }
}
