use crate::state::KeybindingMode;
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
}

fn default_mouse_scroll_lines() -> usize {
    3
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            keybindings: KeybindingMode::Vim,
            lsp_binary: "sqls".into(),
            mouse_scroll_lines: default_mouse_scroll_lines(),
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

#[derive(Debug, Deserialize, Serialize)]
struct Session {
    connection: String,
    #[serde(default)]
    schema_cursor: usize,
}

/// Data restored from session.toml.
#[derive(Debug, Default)]
pub struct SessionData {
    pub connection: Option<String>,
    pub schema_cursor: usize,
}

impl serde::Serialize for KeybindingMode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("vim")
    }
}

impl<'de> serde::Deserialize<'de> for KeybindingMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let _ = String::deserialize(d)?;
        Ok(KeybindingMode::Vim)
    }
}

pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("sqeel"))
}

const DEFAULT_CONFIG: &str = r#"[editor]
keybindings = "vim"

# Path to the SQL LSP binary (sqls recommended: https://github.com/sqls-server/sqls)
lsp_binary = "sqls"

# Number of lines to scroll per mouse wheel tick (applies to all panes)
mouse_scroll_lines = 3
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

/// Save the last-used connection name and schema cursor to session.toml.
pub fn save_session(name: &str, schema_cursor: usize) -> anyhow::Result<()> {
    let dir = config_dir().ok_or_else(|| anyhow::anyhow!("cannot determine config dir"))?;
    std::fs::create_dir_all(&dir)?;
    let content = toml::to_string(&Session {
        connection: name.to_string(),
        schema_cursor,
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
