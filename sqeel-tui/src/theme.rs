//! Named theme loaded from TOML.
//!
//! Load order: `$XDG_CONFIG_HOME/sqeel/theme.toml` → bundled `tokyonight.toml`.
//! If the user config is broken we surface the error to the caller (run-loop
//! turns it into a toast) and fall back to the bundle — the binary always has
//! a working theme.

use ratatui::style::Color;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

const BUNDLED: &str = include_str!("../themes/tokyonight.toml");

static THEME: OnceLock<Theme> = OnceLock::new();

pub fn theme() -> &'static Theme {
    THEME.get_or_init(|| Theme::from_toml(BUNDLED).expect("bundled theme must parse"))
}

/// Shorthand for `&theme().ui` — call sites just read named slots.
pub fn ui() -> &'static UiColors {
    &theme().ui
}

/// Load the theme; prefers the user config, falls back to the bundle.
/// Returns `Some(error_message)` if the user config existed but failed to
/// parse, so the caller can surface it as a toast.
pub fn load() -> Option<String> {
    let user_path = dirs::config_dir().map(|d| d.join("sqeel").join("theme.toml"));
    let mut parse_error: Option<String> = None;
    let theme = user_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| match Theme::from_toml(&s) {
            Ok(t) => Some(t),
            Err(e) => {
                parse_error = Some(format!("theme.toml: {e} — falling back to bundled theme"));
                None
            }
        })
        .unwrap_or_else(|| Theme::from_toml(BUNDLED).expect("bundled theme must parse"));
    let _ = THEME.set(theme);
    parse_error
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct Theme {
    pub name: String,
    pub ui: UiColors,
}

impl Theme {
    fn from_toml(src: &str) -> Result<Self, String> {
        let raw: RawTheme = toml::from_str(src).map_err(|e| e.to_string())?;
        let resolved = raw.resolve()?;
        Ok(resolved)
    }
}

#[derive(Deserialize)]
struct RawTheme {
    name: String,
    palette: HashMap<String, String>,
    ui: HashMap<String, String>,
}

impl RawTheme {
    fn resolve(self) -> Result<Theme, String> {
        let palette: HashMap<String, Color> = self
            .palette
            .iter()
            .map(|(k, v)| parse_color(v).map(|c| (k.clone(), c)))
            .collect::<Result<_, _>>()?;
        let lookup = |key: &str| -> Result<Color, String> {
            let raw = self
                .ui
                .get(key)
                .ok_or_else(|| format!("missing ui slot `{key}`"))?;
            if let Some(c) = palette.get(raw) {
                return Ok(*c);
            }
            parse_color(raw)
        };
        let ui = UiColors::load(&lookup)?;
        Ok(Theme {
            name: self.name,
            ui,
        })
    }
}

fn parse_color(s: &str) -> Result<Color, String> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).map_err(|e| e.to_string())?;
            let g = u8::from_str_radix(&hex[2..4], 16).map_err(|e| e.to_string())?;
            let b = u8::from_str_radix(&hex[4..6], 16).map_err(|e| e.to_string())?;
            return Ok(Color::Rgb(r, g, b));
        }
        return Err(format!("bad hex color `{s}`"));
    }
    Err(format!("unresolved color reference `{s}`"))
}

macro_rules! ui_colors {
    ($($field:ident),+ $(,)?) => {
        #[derive(Debug)]
        #[allow(dead_code)]
        pub struct UiColors {
            $(pub $field: Color,)+
        }
        impl UiColors {
            fn load(lookup: &impl Fn(&str) -> Result<Color, String>) -> Result<Self, String> {
                Ok(Self {
                    $($field: lookup(stringify!($field))?,)+
                })
            }
        }
    };
}

ui_colors!(
    schema_pane_bg,
    pane_sep,
    editor_pane_bg,
    editor_tab_bar_bg,
    results_pane_bg,
    editor_cursor_line_active,
    editor_cursor_line_inactive,
    editor_line_num,
    editor_search_bg,
    editor_search_fg,
    editor_error_fg,
    schema_sel_active_bg,
    schema_sel_inactive_bg,
    schema_border_focus,
    schema_border_filter,
    schema_icon_db,
    schema_icon_table,
    schema_icon_column,
    schema_icon_pk,
    schema_type_fg,
    schema_placeholder_fg,
    results_col_active_bg,
    results_col_inactive_bg,
    results_cursor_active_bg,
    results_cursor_inactive_bg,
    results_sep,
    results_header_active,
    results_row_num,
    results_null,
    results_title_active,
    results_title_inactive,
    results_error,
    results_loading,
    results_cancelled,
    tab_active_fg,
    tab_active_bg,
    tab_inactive_fg,
    tab_sep_fg,
    tab_err_fg,
    tab_err_bg,
    tab_loading_fg,
    tab_loading_bg,
    tab_cancel_fg,
    tab_cancel_bg,
    status_bar_bg,
    status_bar_fg,
    status_mode_fg,
    status_mode_normal,
    status_mode_insert,
    status_mode_visual,
    status_diag_error,
    status_diag_warning,
    status_search_bg,
    status_search_fg,
    status_hint_bg,
    status_hint_fg,
    lsp_warn_fg,
    lsp_warn_bg,
    toast_info_bg,
    toast_info_fg,
    toast_error_bg,
    toast_error_fg,
    dialog_fg,
    dialog_bg,
    dialog_error_bg,
    dialog_error_fg,
    dialog_border,
    confirm_border,
    completion_border,
    completion_bg,
    completion_select,
    completion_key,
    sql_keyword,
    sql_string,
    sql_number,
    sql_comment,
    sql_operator,
    sql_ident,
    sql_plain,
    sql_marker_fg,
    sql_marker_todo,
    sql_marker_fixme,
    sql_marker_note,
    sql_marker_warn,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_parses() {
        let theme = Theme::from_toml(BUNDLED).unwrap();
        assert_eq!(theme.name, "Tokyo Night");
        assert!(matches!(
            theme.ui.editor_cursor_line_active,
            Color::Rgb(_, _, _)
        ));
    }

    #[test]
    fn palette_reference_resolves() {
        let src = r##"
            name = "t"
            [palette]
            bg = "#112233"
            [ui]
            editor_bg_active = "bg"
        "##;
        // Missing slots fail — confirm reference resolution works in isolation.
        let raw: RawTheme = toml::from_str(src).unwrap();
        let lookup = |key: &str| -> Result<Color, String> {
            let raw_val = raw.ui.get(key).ok_or_else(|| format!("missing `{key}`"))?;
            let palette = raw
                .palette
                .iter()
                .map(|(k, v)| parse_color(v).map(|c| (k.clone(), c)))
                .collect::<Result<HashMap<_, _>, _>>()
                .unwrap();
            if let Some(c) = palette.get(raw_val) {
                return Ok(*c);
            }
            parse_color(raw_val)
        };
        assert_eq!(
            lookup("editor_bg_active").unwrap(),
            Color::Rgb(0x11, 0x22, 0x33)
        );
    }

    #[test]
    fn hex_literal_works() {
        assert_eq!(
            parse_color("#ff9e64").unwrap(),
            Color::Rgb(0xff, 0x9e, 0x64)
        );
    }
}
