use crate::state::{Focus, KeybindingMode};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct MainConfig {
    #[serde(default)]
    pub editor: EditorConfig,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct EditorConfig {
    pub keybindings: KeybindingMode,
    pub lsp_binary: String,
    #[serde(default = "default_mouse_scroll_lines")]
    pub mouse_scroll_lines: usize,
    #[serde(default = "default_leader_key")]
    pub leader_key: String,
    /// Whether `Ctrl+Shift+Enter` (run-all) stops on the first query error.
    #[serde(default = "default_stop_on_error")]
    pub stop_on_error: bool,
    /// Seconds before cached schema data (databases / tables / columns) is
    /// considered stale and re-fetched in the background. `0` disables TTL.
    #[serde(default = "default_schema_ttl_secs")]
    pub schema_ttl_secs: u64,
}

fn default_mouse_scroll_lines() -> usize {
    3
}

fn default_leader_key() -> String {
    " ".to_string()
}

fn default_stop_on_error() -> bool {
    true
}

fn default_schema_ttl_secs() -> u64 {
    300
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            keybindings: KeybindingMode::Vim,
            lsp_binary: "sqls".into(),
            mouse_scroll_lines: default_mouse_scroll_lines(),
            leader_key: default_leader_key(),
            stop_on_error: default_stop_on_error(),
            schema_ttl_secs: default_schema_ttl_secs(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ConnectionConfig {
    pub url: String,
    // Derived from filename at load time; not present in the .toml file itself.
    #[serde(default, skip_serializing)]
    pub name: String,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct TabCursor {
    pub name: String,
    pub row: usize,
    pub col: usize,
}

/// Lightweight pointer persisted in session.toml for a single results tab.
/// Success rows live in a separate JSON under
/// `~/.local/share/sqeel/results/<conn>/<filename>.json`. Error + cancelled
/// outcomes are stored inline.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq, Default)]
pub struct SavedResultRef {
    /// Present only for success tabs — on-disk JSON payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub scroll: usize,
    #[serde(default)]
    pub col_scroll: usize,
    /// Error text captured when the query failed. `None` for success /
    /// cancelled tabs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// True for tabs whose batch slot was skipped after an earlier error.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct Session {
    connection: String,
    #[serde(default)]
    schema_cursor: usize,
    #[serde(default)]
    schema_cursor_path: Option<String>,
    #[serde(default)]
    schema_expanded_paths: Vec<String>,
    #[serde(default)]
    focus: Focus,
    #[serde(default)]
    schema_search: Option<String>,
    #[serde(default)]
    tab_cursors: Vec<TabCursor>,
    #[serde(default)]
    active_tab: usize,
    #[serde(default)]
    result_tabs: Vec<SavedResultRef>,
    #[serde(default)]
    active_result_tab: usize,
}

/// Data restored from session.toml.
#[derive(Debug, Default)]
pub struct SessionData {
    pub connection: Option<String>,
    /// Numeric fallback cursor — used only when `schema_cursor_path` lookup fails.
    pub schema_cursor: usize,
    /// Preferred cursor: `"db/table/col"` path string for stable restore across schema changes.
    pub schema_cursor_path: Option<String>,
    /// Expanded node paths, e.g. `["mydb", "mydb/users"]`.
    pub schema_expanded_paths: Vec<String>,
    pub focus: Focus,
    pub schema_search: Option<String>,
    /// Per-tab editor cursor positions, keyed by tab name.
    pub tab_cursors: Vec<TabCursor>,
    pub active_tab: usize,
    pub result_tabs: Vec<SavedResultRef>,
    pub active_result_tab: usize,
}

/// Process-wide override for the config dir, set by `--sandbox` so
/// dev-mode runs don't touch the user's real `~/.config/sqeel/`.
/// `None` (the default) falls back to `dirs::config_dir()`.
static CONFIG_DIR_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Install a sandbox config dir. Idempotent — first call wins.
/// Subsequent calls are silently ignored so a misconfigured caller
/// can't surprise the user mid-run by repointing the dir.
pub fn set_config_dir_override(path: PathBuf) {
    let _ = CONFIG_DIR_OVERRIDE.set(path);
}

pub fn config_dir() -> Option<PathBuf> {
    if let Some(p) = CONFIG_DIR_OVERRIDE.get() {
        return Some(p.clone());
    }
    dirs::config_dir().map(|d| d.join("sqeel"))
}

const DEFAULT_CONFIG: &str = r#"[editor]
keybindings = "vim"

# Path to the SQL LSP binary (sqls recommended: https://github.com/sqls-server/sqls)
lsp_binary = "sqls"

# Number of lines to scroll per mouse wheel tick (applies to all panes)
mouse_scroll_lines = 3

# Leader key for chord shortcuts (e.g. <leader>c opens the connection switcher).
# Use a single character; " " for Space.
leader_key = " "

# Stop running a Ctrl+Shift+Enter batch on the first query error.
stop_on_error = true

# Seconds before cached schema (databases / tables / columns) is considered
# stale and silently re-fetched. 0 disables TTL.
schema_ttl_secs = 300
"#;

pub fn load_main_config() -> anyhow::Result<MainConfig> {
    let dir = config_dir().ok_or_else(|| anyhow::anyhow!("cannot determine config dir"))?;
    let path = dir.join("config.toml");

    if !path.exists() {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&path, DEFAULT_CONFIG)?;
        return Ok(MainConfig::default());
    }

    let content = std::fs::read_to_string(&path)?;
    Ok(toml::from_str(&content)?)
}

pub fn load_connections() -> anyhow::Result<Vec<ConnectionConfig>> {
    let conns_dir = config_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine config dir"))?
        .join("conns");

    if !conns_dir.exists() {
        return Ok(vec![]);
    }

    let mut conns = Vec::new();
    for entry in std::fs::read_dir(&conns_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();
            let content = std::fs::read_to_string(&path)?;
            let mut conn: ConnectionConfig = toml::from_str(&content)?;
            conn.name = name;
            conns.push(conn);
        }
    }
    Ok(conns)
}

/// Save session state to session.toml.
#[allow(clippy::too_many_arguments)]
pub fn save_session(
    name: &str,
    schema_cursor: usize,
    schema_cursor_path: Option<String>,
    schema_expanded_paths: Vec<String>,
    focus: Focus,
    schema_search: Option<String>,
    tab_cursors: Vec<TabCursor>,
    active_tab: usize,
    result_tabs: Vec<SavedResultRef>,
    active_result_tab: usize,
) -> anyhow::Result<()> {
    let dir = config_dir().ok_or_else(|| anyhow::anyhow!("cannot determine config dir"))?;
    std::fs::create_dir_all(&dir)?;
    let content = toml::to_string(&Session {
        connection: name.to_string(),
        schema_cursor,
        schema_cursor_path,
        schema_expanded_paths,
        focus,
        schema_search,
        tab_cursors,
        active_tab,
        result_tabs,
        active_result_tab,
    })?;
    std::fs::write(dir.join("session.toml"), content)?;
    Ok(())
}

fn load_session_inner() -> Option<Session> {
    let path = config_dir()?.join("session.toml");
    let content = std::fs::read_to_string(path).ok()?;
    toml::from_str(&content).ok()
}

/// Load full session data (connection name + schema cursor).
pub fn load_session_data() -> SessionData {
    let Some(s) = load_session_inner() else {
        return SessionData::default();
    };
    SessionData {
        connection: if s.connection.is_empty() {
            None
        } else {
            Some(s.connection)
        },
        schema_cursor: s.schema_cursor,
        schema_cursor_path: s.schema_cursor_path,
        schema_expanded_paths: s.schema_expanded_paths,
        focus: s.focus,
        schema_search: s.schema_search,
        tab_cursors: s.tab_cursors,
        active_tab: s.active_tab,
        result_tabs: s.result_tabs,
        active_result_tab: s.active_result_tab,
    }
}

/// Load only the last-used connection name from session.toml.
pub fn load_session() -> Option<String> {
    load_session_inner().and_then(|s| {
        if s.connection.is_empty() {
            None
        } else {
            Some(s.connection)
        }
    })
}

pub fn delete_connection(name: &str) -> anyhow::Result<()> {
    let path = config_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine config dir"))?
        .join("conns")
        .join(format!("{name}.toml"));
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

pub fn save_connection(name: &str, url: &str) -> anyhow::Result<()> {
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!("Connection name may only contain letters, digits, - and _");
    }
    let conns_dir = config_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot determine config dir"))?
        .join("conns");
    std::fs::create_dir_all(&conns_dir)?;
    let conn = ConnectionConfig {
        url: url.to_string(),
        name: String::new(),
    };
    let content = toml::to_string(&conn)?;
    std::fs::write(conns_dir.join(format!("{name}.toml")), content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_vim_bindings() {
        let config = MainConfig::default();
        assert_eq!(config.editor.keybindings, KeybindingMode::Vim);
    }

    #[test]
    fn default_config_has_sqls_lsp() {
        let config = MainConfig::default();
        assert_eq!(config.editor.lsp_binary, "sqls");
    }

    #[test]
    fn keybinding_mode_deserialize_vim() {
        let config: MainConfig = toml::from_str(
            r#"
[editor]
keybindings = "vim"
lsp_binary = "sqls"
"#,
        )
        .unwrap();
        assert_eq!(config.editor.keybindings, KeybindingMode::Vim);
    }

    #[test]
    fn connection_config_parse() {
        let conn: ConnectionConfig = toml::from_str(
            r#"
url = "mysql://user:pass@localhost/mydb"
name = "local"
"#,
        )
        .unwrap();
        assert_eq!(conn.url, "mysql://user:pass@localhost/mydb");
    }
}
