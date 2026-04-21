# SQEEL

[![CI](https://github.com/sqeel-sql/sqeel/actions/workflows/ci.yml/badge.svg)](https://github.com/sqeel-sql/sqeel/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/sqeel-sql/sqeel)](https://github.com/sqeel-sql/sqeel/releases/latest)

Fast, vim-native SQL client. No Electron. No JVM.

## Features

- Native Rust — instant startup
- Vim or Emacs bindings — first class
- Mouse support in all modes
- Two UIs: terminal (`sqeel`) or native GUI (`sqeel-gui`)
- MySQL, SQLite, PostgreSQL via sqlx
- tree-sitter SQL syntax highlighting (dialect-aware)
- LSP integration (`sqls`) — completions + diagnostics
- Schema browser — databases → tables → columns
- Auto-save SQL buffers, result history, query history

## Layout

```
┌──────────┬─────────────────────────────┐
│          │                             │
│  Schema  │         Editor              │
│  (15%)   │         (85%)               │
│          │                             │
│          ├─────────────────────────────┤
│          │         Results             │
│          │      (shows on query)       │
└──────────┴─────────────────────────────┘
```

Results hidden → editor fills right pane. Query runs → results expand to 50%.

## Install

```sh
cargo install --git https://github.com/sqeel-sql/sqeel --bin sqeel
cargo install --git https://github.com/sqeel-sql/sqeel --bin sqeel-gui
```

Or build from source:

```sh
git clone https://github.com/sqeel-sql/sqeel
cd sqeel
cargo build --release
```

Binaries land in `target/release/sqeel` and `target/release/sqeel-gui`.

## Config

### Main — `~/.config/sqeel/config.toml`

```toml
[editor]
keybindings = "vim"   # or "emacs"
lsp_binary = "sqls"
```

### Connections — `~/.config/sqeel/conns/<name>.toml`

Each file is one connection. Filename = display name in UI.

```toml
url = "mysql://localhost/mydb"
```

```toml
url = "postgres://user:pass@host/db"

[tls]
ca_cert = "/path/to/ca.pem"
```

sqeel scans `conns/` on startup and loads all `.toml` files.

## Keybindings

### Vim (default)

| Key | Action |
|-----|--------|
| `<leader>r` / `Ctrl+Enter` | Execute query |
| `Ctrl+h/l` | Left/right pane |
| `Ctrl+j/k` | Editor/results pane |
| `+/-` or mouse drag | Resize splits |
| `Ctrl+W` | Connection switcher |
| `Ctrl+P/N` | Query history |
| `?` | Help overlay |

### Emacs

| Key | Action |
|-----|--------|
| `Ctrl+x Ctrl+e` | Execute query |
| `Ctrl+x o` | Cycle pane focus |
| `Ctrl+Space` | Autocomplete |
| `Ctrl+P/N` | Query history |

## Data

```
~/.local/share/sqeel/
  queries/    # auto-saved SQL buffers
  results/    # last 10 successful results (JSON)
```

## Workspace

```
sqeel-core/   # state, DB, query runner, schema, config
sqeel-tui/    # ratatui terminal provider
sqeel-gui/    # iced native GUI provider
sqeel/        # binaries: sqeel + sqeel-gui
```

## License

[MIT](LICENSE)
