# SQEEL

[![CI](https://github.com/sqeel-sql/sqeel/actions/workflows/ci.yml/badge.svg)](https://github.com/sqeel-sql/sqeel/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/sqeel-sql/sqeel)](https://github.com/sqeel-sql/sqeel/releases/latest)

Fast, vim-native SQL client. No Electron. No JVM.

## Features

- Native Rust — instant startup
- Vim bindings — first class (operators, text objects, visual modes, marks,
  jumplist, page scroll, H/M/L, case/indent ops, dot-repeat)
- Mouse support in all panes
- Two UIs: terminal (`sqeel`) or native GUI (`sqeel-gui`)
- MySQL, SQLite, PostgreSQL via sqlx
- tree-sitter SQL syntax highlighting (dialect-aware)
- LSP integration (`sqls`) — completions + inline diagnostic underlines + gutter
  signs, tree-sitter fallback for parse errors, auto-generated `sqls` config
  from the active connection
- Schema browser — click or keyboard to expand/collapse
- Editor tabs with lazy loading and 5-min RAM eviction
- Auto-save SQL buffers, result history, query history
- tmux-aware pane navigation
- Vim-style status bar + command mode (`:`)
- Vim-style results pane — cell cursor, visual-line / visual-block selection
  with TSV yank, `/` search, count prefix nav

## Layout

```
┌──────────┬─────────────────────────────┐
│          │  [tab1] [tab2]              │
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
keybindings = "vim"

# Path to the SQL LSP binary (sqls recommended: https://github.com/sqls-server/sqls)
lsp_binary = "sqls"

# Lines scrolled per mouse wheel tick (all panes)
mouse_scroll_lines = 3

# Leader key for chord shortcuts (e.g. <leader>c opens the connection switcher).
# Single character; " " for Space.
leader_key = " "

# Stop a Ctrl+Shift+Enter batch on the first query error.
stop_on_error = true
```

### Connections — `~/.config/sqeel/conns/<name>.toml`

Each file is one connection. Filename = display name in UI.

```toml
url = "mysql://localhost/mydb"
```

```toml
url = "postgres://user:pass@host/db"
```

sqeel scans `conns/` on startup and loads all `.toml` files.

## Keybindings

Press `?` in normal mode to open the help overlay.

### Global

| Key                | Action                          |
| ------------------ | ------------------------------- |
| `?`                | Open help overlay (normal mode) |
| `Ctrl+Enter`       | Run statement under cursor      |
| `Ctrl+Shift+Enter` | Run all statements in file      |
| `:q`               | Quit                            |
| `Ctrl+C`           | Cancel running query / batch    |
| `Esc Esc`          | Dismiss all toasts              |

### Leader (default `Space` — config: `editor.leader_key`)

| Key                | Action                       |
| ------------------ | ---------------------------- |
| `<leader>c`        | Connection switcher          |
| `<leader>n`        | New scratch tab              |
| `<leader>r`        | Rename current tab           |
| `<leader>d`        | Delete current tab (confirm) |
| `<leader><leader>` | Fuzzy file picker            |

### Pane Focus

| Key              | Action        |
| ---------------- | ------------- |
| `Ctrl+H` / click | Focus schema  |
| `Ctrl+L` / click | Focus editor  |
| `Ctrl+J` / click | Focus results |
| `Ctrl+K` / click | Focus editor  |

### Tabs

| Key            | Action        |
| -------------- | ------------- |
| `Shift+L`      | Next tab      |
| `Shift+H`      | Prev tab      |
| Click tab name | Switch to tab |

### Editor — Vim

Core motions, operators, text objects, and visual modes all work. The help
overlay (`?`) is the authoritative list; the table below is a cheat sheet of
features that go beyond basic vim.

| Key                     | Action                                    |
| ----------------------- | ----------------------------------------- |
| `i` / `Esc`             | Insert / Normal                           |
| `v` / `V` / `Ctrl+V`    | Visual (char / line / block)              |
| `:`                     | Command mode                              |
| `/` + `n` / `N`         | Search + next / previous                  |
| `*` / `#`               | Search word under cursor fwd / back       |
| `Ctrl+d` / `Ctrl+u`     | Half-page scroll (cursor follows)         |
| `Ctrl+f` / `Ctrl+b`     | Full-page scroll                          |
| `H` / `M` / `L`         | Cursor to viewport top / middle / bottom  |
| `gg` / `G`              | First / last line                         |
| `zz` / `zt` / `zb`      | Center / top / bottom viewport on cursor  |
| `m{a-z}`                | Set mark                                  |
| `` `{a-z} `` / `'{a-z}` | Jump to mark (charwise / linewise)        |
| `Ctrl+o` / `Ctrl+i`     | Jumplist back / forward                   |
| `gU` / `gu` / `g~`      | Uppercase / lowercase / toggle-case op    |
| `>` / `<`               | Indent / outdent op                       |
| `Ctrl+a` / `Ctrl+x`     | Increment / decrement number under cursor |
| `Ctrl+P` / `Ctrl+N`     | Query history prev / next                 |

### Explorer Pane

| Key           | Action                 |
| ------------- | ---------------------- |
| `j` / `k`     | Navigate down / up     |
| `Enter` / `l` | Expand / collapse node |
| `/`           | Search                 |

### Results Pane

Vim-native navigation over the cell grid. Arrow keys mirror `hjkl`.

| Key / Mouse           | Action                                           |
| --------------------- | ------------------------------------------------ |
| `j` / `k` / `h` / `l` | Cursor / scroll (count-prefixable, arrows alias) |
| `gg` / `G`            | First / last row                                 |
| `0` / `$`             | First / last column of current row               |
| `/` + `n` / `N`       | Search cells (case-insensitive) + next / prev    |
| `V`                   | Visual-line select rows                          |
| `v` / `Ctrl+V`        | Visual-block select rectangle                    |
| `y`                   | Yank selection / row (TSV)                       |
| `Esc`                 | Clear selection / close `/` prompt               |
| `Shift+H` / `Shift+L` | Prev / next result tab                           |
| `Enter`               | Jump editor cursor to error line:col (error tab) |
| Left click            | Copy column value                                |
| Right click           | Copy full row                                    |
| Left click (error)    | Copy query or error text                         |
| `q` / `Ctrl+C`        | Dismiss results                                  |

### Connection Switcher

| Key       | Action            |
| --------- | ----------------- |
| `j` / `k` | Navigate          |
| `Enter`   | Connect           |
| `n`       | New connection    |
| `e`       | Edit connection   |
| `d`       | Delete connection |
| `Esc`     | Close             |

### Add / Edit Connection

| Key     | Action                  |
| ------- | ----------------------- |
| `Tab`   | Switch Name / URL field |
| `Enter` | Save                    |
| `Esc`   | Cancel                  |

## Data

```
~/.local/share/sqeel/
  queries/    # auto-saved SQL buffers (grouped by connection)
  results/    # last 10 successful results (JSON, grouped by connection)
```

## Workspace

```
sqeel-core/            # state, DB, query runner, schema, config
sqeel-tui/             # ratatui terminal provider
sqeel-gui/             # iced native GUI provider
sqeel-vim/             # vim-mode editor widget (on top of tui-textarea)
sqeel/                 # binaries: sqeel + sqeel-gui
vendor/tui-textarea/   # vendored + patched text area (render cache, signs)
```

## License

[MIT](LICENSE)
