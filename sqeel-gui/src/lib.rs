use std::sync::{Arc, Mutex};

use iced::{
    Color, Element, Length, Task, Theme,
    widget::{button, column, container, row, scrollable, text, text_editor, text_input},
};
use sqeel_core::{
    AppState, UiProvider,
    state::{Focus, KeybindingMode, ResultsPane},
};

pub struct GuiProvider;

impl UiProvider for GuiProvider {
    fn run(state: Arc<Mutex<AppState>>) -> anyhow::Result<()> {
        let flags = state;
        iced::application(
            move || {
                let initial = flags.lock().unwrap().editor_content.clone();
                (
                    SqeelApp {
                        state: flags.clone(),
                        editor_content: text_editor::Content::with_text(&initial),
                    },
                    Task::none(),
                )
            },
            SqeelApp::update,
            SqeelApp::view,
        )
        .title("SQEEL")
        .theme(SqeelApp::theme)
        .run()?;
        Ok(())
    }
}

// ── Messages ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Message {
    EditorAction(text_editor::Action),
    ExecuteQuery,
    DismissResults,
    FocusSchema,
    FocusEditor,
    FocusResults,
    SchemaToggle(usize),
    SchemaUp,
    SchemaDown,
    CompletionTrigger,
    DismissCompletions,
    OpenConnectionSwitcher,
    CloseConnectionSwitcher,
    SwitcherUp,
    SwitcherDown,
    ConfirmConnectionSwitch,
    OpenAddConnection,
    CloseAddConnection,
    AddNameChanged(String),
    AddUrlChanged(String),
    AddConnectionTabField,
    SaveNewConnection,
}

// ── Application ───────────────────────────────────────────────────────────────

struct SqeelApp {
    state: Arc<Mutex<AppState>>,
    editor_content: text_editor::Content,
}

impl SqeelApp {
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::EditorAction(action) => {
                self.editor_content.perform(action);
                let content = self.editor_content.text();
                let mut s = self.state.lock().unwrap();
                s.editor_content = content;
                s.autosave();
            }
            Message::ExecuteQuery => {
                let content = self.editor_content.text();
                let sent = self.state.lock().unwrap().send_query(content.clone());
                if !sent {
                    self.state.lock().unwrap().set_error(
                        "No DB connected. Use --url / --connection or the Connections button."
                            .into(),
                    );
                }
            }
            Message::DismissResults => {
                self.state.lock().unwrap().dismiss_results();
            }
            Message::FocusSchema => {
                self.state.lock().unwrap().focus = Focus::Schema;
            }
            Message::FocusEditor => {
                self.state.lock().unwrap().focus = Focus::Editor;
            }
            Message::FocusResults => {
                self.state.lock().unwrap().focus = Focus::Results;
            }
            Message::SchemaToggle(cursor) => {
                let mut s = self.state.lock().unwrap();
                s.schema_cursor = cursor;
                s.schema_toggle_current();
            }
            Message::SchemaUp => {
                self.state.lock().unwrap().schema_cursor_up();
            }
            Message::SchemaDown => {
                self.state.lock().unwrap().schema_cursor_down();
            }
            Message::CompletionTrigger => {
                self.state.lock().unwrap().set_completions(vec![
                    "SELECT".into(),
                    "FROM".into(),
                    "WHERE".into(),
                    "JOIN".into(),
                    "GROUP BY".into(),
                ]);
            }
            Message::DismissCompletions => {
                self.state.lock().unwrap().dismiss_completions();
            }
            Message::OpenConnectionSwitcher => {
                self.state.lock().unwrap().open_connection_switcher();
            }
            Message::CloseConnectionSwitcher => {
                self.state.lock().unwrap().close_connection_switcher();
            }
            Message::SwitcherUp => {
                self.state.lock().unwrap().switcher_up();
            }
            Message::SwitcherDown => {
                self.state.lock().unwrap().switcher_down();
            }
            Message::ConfirmConnectionSwitch => {
                self.state.lock().unwrap().confirm_connection_switch();
            }
            Message::OpenAddConnection => {
                self.state.lock().unwrap().open_add_connection();
            }
            Message::CloseAddConnection => {
                self.state.lock().unwrap().close_add_connection();
            }
            Message::AddNameChanged(s) => {
                self.state.lock().unwrap().add_connection_name = s;
            }
            Message::AddUrlChanged(s) => {
                self.state.lock().unwrap().add_connection_url = s;
            }
            Message::AddConnectionTabField => {
                self.state.lock().unwrap().add_connection_tab();
            }
            Message::SaveNewConnection => {
                let result = self.state.lock().unwrap().save_new_connection();
                if let Err(e) = result {
                    self.state
                        .lock()
                        .unwrap()
                        .set_error(format!("Save failed: {e}"));
                }
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let s = self.state.lock().unwrap();
        let keybinding_mode = s.keybinding_mode;
        let show_results = !matches!(s.results, ResultsPane::Empty);
        let schema_labels: Vec<String> = s
            .visible_schema_items()
            .into_iter()
            .map(|item| item.label)
            .collect();
        let schema_cursor = s.schema_cursor;
        let results = s.results.clone();
        let diag_msg: Option<String> = s
            .lsp_diagnostics
            .first()
            .map(|d| format!("{}:{} {}", d.line + 1, d.col + 1, d.message));
        let completions: Vec<String> = if s.show_completions {
            s.completions.clone()
        } else {
            vec![]
        };
        let active_connection = s.active_connection.clone();
        let show_switcher = s.show_connection_switcher;
        let switcher_cursor = s.connection_switcher_cursor;
        let conn_list: Vec<(String, String)> = s
            .available_connections
            .iter()
            .map(|c| (c.name.clone(), c.url.clone()))
            .collect();
        let show_add = s.show_add_connection;
        let add_name = s.add_connection_name.clone();
        let add_url = s.add_connection_url.clone();
        drop(s);

        // Schema panel
        let schema_content: Element<Message> = if schema_labels.is_empty() {
            text(if active_connection.is_some() {
                "Loading..."
            } else {
                "No connection"
            })
            .into()
        } else {
            let mut col = column![].spacing(2);
            for (i, label) in schema_labels.into_iter().enumerate() {
                let is_selected = i == schema_cursor;
                let btn = button(text(label).size(13))
                    .on_press(Message::SchemaToggle(i))
                    .style(move |theme, status| {
                        if is_selected {
                            iced::widget::button::primary(theme, status)
                        } else {
                            iced::widget::button::text(theme, status)
                        }
                    })
                    .width(Length::Fill);
                col = col.push(btn);
            }
            scrollable(col).into()
        };

        let schema_panel = container(
            column![
                text("Schema")
                    .size(14)
                    .color(Color::from_rgb(0.6, 0.8, 1.0)),
                schema_content,
            ]
            .spacing(4),
        )
        .width(Length::FillPortion(15))
        .height(Length::Fill)
        .padding(8);

        // Mode label
        let mode_str = match keybinding_mode {
            KeybindingMode::Vim => "VIM",
            KeybindingMode::Emacs => "EMACS",
        };

        // Editor section
        let mut editor_col = column![
            row![
                text(format!("Editor [{mode_str}]"))
                    .size(14)
                    .color(Color::from_rgb(0.6, 0.8, 1.0)),
                button(text("Run ▶").size(12))
                    .on_press(Message::ExecuteQuery)
                    .style(iced::widget::button::primary),
                button(text("⚡ Connections").size(12))
                    .on_press(Message::OpenConnectionSwitcher)
                    .style(iced::widget::button::secondary),
            ]
            .spacing(8)
            .align_y(iced::alignment::Vertical::Center),
            text_editor(&self.editor_content)
                .on_action(Message::EditorAction)
                .height(Length::Fill),
        ]
        .spacing(4);

        // Diagnostic line
        if let Some(msg) = diag_msg {
            editor_col = editor_col.push(text(msg).size(12).color(Color::from_rgb(1.0, 0.3, 0.3)));
        }

        // Completions
        if !completions.is_empty() {
            let mut comp_col = column![text("Completions").size(12)].spacing(2);
            for item in completions {
                comp_col = comp_col.push(
                    button(text(item).size(12))
                        .on_press(Message::DismissCompletions)
                        .style(iced::widget::button::secondary),
                );
            }
            editor_col = editor_col.push(comp_col);
        }

        // Results panel
        let right: Element<Message> = if show_results {
            let results_widget = build_results_widget(&results);
            column![
                container(editor_col).height(Length::FillPortion(1)),
                container(
                    column![
                        row![
                            text("Results")
                                .size(14)
                                .color(Color::from_rgb(0.4, 1.0, 0.4)),
                            button(text("✕").size(12))
                                .on_press(Message::DismissResults)
                                .style(iced::widget::button::danger),
                        ]
                        .spacing(8),
                        results_widget,
                    ]
                    .spacing(4),
                )
                .height(Length::FillPortion(1)),
            ]
            .spacing(4)
            .into()
        } else {
            column![editor_col].into()
        };

        let editor_panel = container(right)
            .width(Length::FillPortion(85))
            .height(Length::Fill)
            .padding(8);

        let base: Element<Message> = container(row![schema_panel, editor_panel].spacing(0))
            .width(Length::Fill)
            .height(Length::Fill)
            .into();

        if show_switcher {
            let mut switcher_col = column![
                text("Switch Connection")
                    .size(14)
                    .color(Color::from_rgb(0.6, 0.8, 1.0)),
            ]
            .spacing(4);

            for (i, (name, url)) in conn_list.into_iter().enumerate() {
                let is_selected = i == switcher_cursor;
                let label = format!("{name}  {url}");
                let btn = button(text(label).size(13))
                    .on_press(Message::ConfirmConnectionSwitch)
                    .style(move |theme, status| {
                        if is_selected {
                            iced::widget::button::primary(theme, status)
                        } else {
                            iced::widget::button::text(theme, status)
                        }
                    })
                    .width(Length::Fill);
                switcher_col = switcher_col.push(btn);
            }

            switcher_col = switcher_col.push(
                row![
                    button(text("+ New").size(12))
                        .on_press(Message::OpenAddConnection)
                        .style(iced::widget::button::secondary),
                    button(text("Close").size(12))
                        .on_press(Message::CloseConnectionSwitcher)
                        .style(iced::widget::button::danger),
                ]
                .spacing(8),
            );

            let switcher_overlay = container(switcher_col)
                .width(Length::Fixed(480.0))
                .padding(16)
                .style(|theme: &Theme| {
                    let palette = theme.extended_palette();
                    iced::widget::container::Style {
                        background: Some(palette.background.strong.color.into()),
                        border: iced::Border {
                            color: palette.primary.strong.color,
                            width: 1.0,
                            radius: 6.0.into(),
                        },
                        ..Default::default()
                    }
                });

            let stacked = iced::widget::stack![
                base,
                container(switcher_overlay)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(iced::alignment::Horizontal::Center)
                    .align_y(iced::alignment::Vertical::Center),
            ];

            if show_add {
                let add_col = column![
                    text("Add Connection")
                        .size(14)
                        .color(Color::from_rgb(0.4, 1.0, 0.4)),
                    text("Name").size(12),
                    text_input("my-db", &add_name)
                        .on_input(Message::AddNameChanged)
                        .padding(6),
                    text("URL").size(12),
                    text_input("mysql://user:pass@host/db", &add_url)
                        .on_input(Message::AddUrlChanged)
                        .padding(6),
                    row![
                        button(text("Save").size(12))
                            .on_press(Message::SaveNewConnection)
                            .style(iced::widget::button::primary),
                        button(text("Cancel").size(12))
                            .on_press(Message::CloseAddConnection)
                            .style(iced::widget::button::danger),
                    ]
                    .spacing(8),
                ]
                .spacing(6);

                let add_overlay = container(add_col)
                    .width(Length::Fixed(400.0))
                    .padding(16)
                    .style(|theme: &Theme| {
                        let palette = theme.extended_palette();
                        iced::widget::container::Style {
                            background: Some(palette.background.strong.color.into()),
                            border: iced::Border {
                                color: palette.success.strong.color,
                                width: 1.0,
                                radius: 6.0.into(),
                            },
                            ..Default::default()
                        }
                    });

                iced::widget::stack![
                    stacked,
                    container(add_overlay)
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .align_x(iced::alignment::Horizontal::Center)
                        .align_y(iced::alignment::Vertical::Center),
                ]
                .into()
            } else {
                stacked.into()
            }
        } else if show_add {
            // add dialog without switcher (shouldn't normally happen, but handle gracefully)
            let add_col = column![
                text("Add Connection")
                    .size(14)
                    .color(Color::from_rgb(0.4, 1.0, 0.4)),
                text("Name").size(12),
                text_input("my-db", &add_name)
                    .on_input(Message::AddNameChanged)
                    .padding(6),
                text("URL").size(12),
                text_input("mysql://user:pass@host/db", &add_url)
                    .on_input(Message::AddUrlChanged)
                    .padding(6),
                row![
                    button(text("Save").size(12))
                        .on_press(Message::SaveNewConnection)
                        .style(iced::widget::button::primary),
                    button(text("Cancel").size(12))
                        .on_press(Message::CloseAddConnection)
                        .style(iced::widget::button::danger),
                ]
                .spacing(8),
            ]
            .spacing(6);

            let add_overlay = container(add_col)
                .width(Length::Fixed(400.0))
                .padding(16)
                .style(|theme: &Theme| {
                    let palette = theme.extended_palette();
                    iced::widget::container::Style {
                        background: Some(palette.background.strong.color.into()),
                        border: iced::Border {
                            color: palette.success.strong.color,
                            width: 1.0,
                            radius: 6.0.into(),
                        },
                        ..Default::default()
                    }
                });

            iced::widget::stack![
                base,
                container(add_overlay)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .align_x(iced::alignment::Horizontal::Center)
                    .align_y(iced::alignment::Vertical::Center),
            ]
            .into()
        } else {
            base
        }
    }

    fn theme(&self) -> Theme {
        Theme::Dark
    }
}

fn build_results_widget(results: &ResultsPane) -> Element<'static, Message> {
    match results {
        ResultsPane::Results(r) => {
            let header = text(r.columns.join(" │ "))
                .size(13)
                .color(Color::from_rgb(0.4, 0.8, 1.0));
            let mut col = column![header].spacing(2);
            for row_data in r.rows.iter().take(200) {
                col = col.push(text(row_data.join(" │ ")).size(12));
            }
            scrollable(col).into()
        }
        ResultsPane::Error(e) => text(e.clone())
            .size(13)
            .color(Color::from_rgb(1.0, 0.3, 0.3))
            .into(),
        ResultsPane::Empty => text("").into(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use sqeel_core::{
        AppState,
        state::{Focus, QueryResult, ResultsPane},
    };

    #[test]
    fn execute_with_no_db_sets_error() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_error("No DB connected. Use --url or --connection to connect.".into());
        assert!(matches!(s.results, ResultsPane::Error(_)));
        assert_eq!(s.editor_ratio, 0.5);
    }

    #[test]
    fn dismiss_results_clears_pane() {
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_results(QueryResult {
            columns: vec!["id".into()],
            rows: vec![],
        });
        s.dismiss_results();
        assert!(matches!(s.results, ResultsPane::Empty));
        assert_eq!(s.editor_ratio, 1.0);
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
    fn schema_toggle_expands_node() {
        use sqeel_core::schema::SchemaNode;
        let state = AppState::new();
        let mut s = state.lock().unwrap();
        s.set_schema_nodes(vec![SchemaNode::Database {
            name: "mydb".into(),
            expanded: false,
            tables: vec![SchemaNode::Table {
                name: "users".into(),
                expanded: false,
                columns: vec![],
            }],
        }]);
        assert_eq!(s.visible_schema_items().len(), 1);
        s.schema_toggle_current();
        assert_eq!(s.visible_schema_items().len(), 2);
    }

    #[test]
    fn vim_mode_initial_state() {
        use sqeel_core::state::VimMode;
        let state = AppState::new();
        let s = state.lock().unwrap();
        assert_eq!(s.vim_mode, VimMode::Normal);
    }
}
