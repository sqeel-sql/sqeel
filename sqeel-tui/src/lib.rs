pub mod editor;

use std::sync::{Arc, Mutex};
use std::io;

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};
use sqeel_core::{AppState, UiProvider, state::{Focus, KeybindingMode, ResultsPane, VimMode}};
use editor::Editor;

pub struct TuiProvider;

impl UiProvider for TuiProvider {
    fn run(state: Arc<Mutex<AppState>>) -> anyhow::Result<()> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let keybinding_mode = state.lock().unwrap().keybinding_mode;
        let result = run_loop(&mut terminal, state, keybinding_mode);

        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;

        result
    }
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: Arc<Mutex<AppState>>,
    keybinding_mode: KeybindingMode,
) -> anyhow::Result<()> {
    let mut editor = Editor::new(keybinding_mode);

    loop {
        // Sync editor content into shared state
        {
            let mut s = state.lock().unwrap();
            s.editor_content = editor.content();
            s.vim_mode = editor.vim_mode;
        }

        terminal.draw(|f| {
            let s = state.lock().unwrap();
            draw(f, &s, &editor);
        })?;

        if let Event::Key(key) = event::read()? {
            let s = state.lock().unwrap();
            let focus = s.focus;
            let vim_mode = s.vim_mode;
            drop(s);

            match (key.modifiers, key.code) {
                // Global quit in vim normal mode
                (KeyModifiers::NONE, KeyCode::Char('q'))
                    if focus != Focus::Editor || vim_mode == VimMode::Normal =>
                {
                    break;
                }
                // Dismiss results pane
                (KeyModifiers::CONTROL, KeyCode::Char('c'))
                | (KeyModifiers::NONE, KeyCode::Char('q'))
                    if focus == Focus::Results =>
                {
                    state.lock().unwrap().dismiss_results();
                }
                // Execute query: Ctrl+Enter
                (KeyModifiers::CONTROL, KeyCode::Enter) => {
                    let content = editor.content();
                    // M3: actual query execution goes here
                    state.lock().unwrap().set_error(format!("No DB connected. Query: {content}"));
                }
                // Pane focus — Ctrl+h/l for schema/editor, Ctrl+j/k for editor/results
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
                // Editor keys — only when editor focused
                _ if focus == Focus::Editor => {
                    editor.handle_key(key);
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn mode_label(state: &'_ AppState) -> Span<'_> {
    match state.keybinding_mode {
        KeybindingMode::Vim => match state.vim_mode {
            VimMode::Normal => Span::styled(" NORMAL ", Style::default().fg(Color::Blue)),
            VimMode::Insert => Span::styled(" INSERT ", Style::default().fg(Color::Green)),
            VimMode::Visual => Span::styled(" VISUAL ", Style::default().fg(Color::Magenta)),
        },
        KeybindingMode::Emacs => Span::styled(" EMACS ", Style::default().fg(Color::Cyan)),
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
    let schema_block = Block::default()
        .title("Schema")
        .borders(Borders::ALL)
        .border_style(if schema_focused {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        });
    f.render_widget(schema_block, outer[0]);

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

    // Editor panel — use tui-textarea widget
    let mode = mode_label(state);
    let editor_block = Block::default()
        .title(Line::from(vec![
            Span::raw("Editor "),
            mode,
        ]))
        .borders(Borders::ALL)
        .border_style(if editor_focused {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        });
    let mut textarea = editor.textarea.clone();
    textarea.set_block(editor_block);
    f.render_widget(&textarea, right_chunks[0]);

    // Results panel
    if show_results {
        let (title, content, color) = match &state.results {
            ResultsPane::Results(r) => {
                let text = format!(
                    "{}\n{}",
                    r.columns.join(" | "),
                    r.rows.iter().map(|row| row.join(" | ")).collect::<Vec<_>>().join("\n")
                );
                ("Results", text, Color::Green)
            }
            ResultsPane::Error(e) => ("Error", e.clone(), Color::Red),
            ResultsPane::Empty => unreachable!(),
        };

        let results_block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(if results_focused {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(color)
            });
        f.render_widget(Paragraph::new(content).block(results_block), right_chunks[1]);
    }
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
}
