# SQEEL — Rust SQL Client

Fast, vim-native SQL client. No Electron. No JVM. No bullshit.

## Differentiators

- Native Rust — starts instantly
- Vim or Emacs bindings — first class, not a plugin
- Mouse always works regardless of keybinding mode
- Clean layout — not weird
- Two UI modes: terminal (ratatui) or native GUI (iced)

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

- Results hidden → editor fills full right pane
- Query runs → results expands to 50% right, editor shrinks to 50%
- All splits user-adjustable

## Keybinding Modes

Configured in `config.toml`. Mouse always enabled in both modes.

### Vim mode (default)

- Normal/insert/visual modes in editor
- `Ctrl+h/l` — left/right pane focus (click also works)
- `Ctrl+j/k` — editor/results focus (click also works)
- `+/-` or mouse drag — resize splits
- `<leader>r` or `Ctrl+Enter` — execute query

### Emacs mode

- No modal editing — always in insert mode
- `Ctrl+b/f` — move cursor left/right
- `Ctrl+p/n` — move cursor up/down
- `Ctrl+a/e` — start/end of line
- `Ctrl+x Ctrl+e` — execute query
- `Ctrl+x o` — cycle pane focus
- Mouse drag — resize splits

## Editor Features

### Syntax Highlighting

- `tree-sitter` with SQL grammar — dialect-aware (MySQL, SQLite, PostgreSQL)
- Runs in both TUI and GUI providers

### LSP Integration

- Connects to an external SQL LSP (`sqls` recommended)
- Diagnostics (syntax errors, unknown tables/columns) shown inline in editor
- Autocomplete triggered on keypress (`Ctrl+Space` in emacs mode, native in vim
  insert mode)
- LSP process spawned on startup, connection string passed for schema-aware
  completions
- Configurable LSP binary path in `config.toml`

## Architecture — UI Abstraction

Core logic (state, DB, query execution) lives in `sqeel-core`. UI providers
implement a trait and render from shared state.

```
sqeel-core/        ← state machine, DB, query runner, schema model
sqeel-tui/         ← ratatui provider (terminal)
sqeel-gui/         ← iced provider (native window)
sqeel/             ← two binaries: `sqeel` (TUI) and `sqeel-gui` (GUI)
```

### `UiProvider` trait (rough shape)

```rust
pub trait UiProvider {
    fn run(core: Arc<Core>) -> anyhow::Result<()>;
}
```

### Error Handling

Query errors and connection failures display in the results pane, styled
distinctly (e.g. red). Same dismiss keys apply (`Ctrl+c` / `q` in results pane).

### State shared across providers

- Active connection + DB selection
- Editor buffer + cursor + keybinding mode (vim or emacs)
- Query results (columns + rows) or error message
- Schema tree (databases → tables → columns)
- Focus (which pane is active)
- Split ratios

## Persistence

### SQL Files

Every buffer is backed by a file on disk. New buffers get a generated name (e.g.
`scratch_001.sql`). Saving is automatic on every edit — no explicit save command
needed. Files live in `~/.local/share/sqeel/queries/` (platform-resolved via
`dirs::data_dir()`). Named tabs allow switching between open files.

### Result History

The last 10 successful query results are persisted to disk as JSON in
`~/.local/share/sqeel/results/`. Errors are never stored. Each file is named by
timestamp + query hash (e.g. `2026-04-21T14:05:00Z_a3f9c2.json`). When there are
more than 10, the oldest is deleted. Results can be browsed from a history panel
or recalled by query history.

Events flow: user input → provider translates → `Core` action → state update →
provider re-renders.

## Testing

Full coverage is required for all features. Every milestone ships with tests.

### Strategy

- **`sqeel-core`** — unit + integration tests for all state transitions,
  actions, query execution, schema parsing, config loading. No mocks for DB —
  use real sqlx connections against a test DB (Docker or env var).
- **`sqeel-tui`** — unit tests for input → action translation, layout split
  logic, vim mode state machine.
- **`sqeel-gui`** — unit tests for message → state update logic (iced is
  elm-style, easy to unit test without rendering).
- **Integration** — end-to-end tests that boot a provider with a real DB and
  assert state after actions.

### Rules

- No feature merged without tests covering it
- DB tests require a real connection — no mocks
- CI runs full test suite against MySQL + SQLite

## Platform Support

- **Linux** — primary target, all milestones build and test here first
- **Windows** — required before M7 (Polish)
- **macOS** — required before M7 (Polish)

CI matrix expands to all three once core is stable (post-M3).

## Repository

- GitHub org: `sqeel-sql` (under mxaddict's account)
- Repo: `sqeel-sql/sqeel` → `https://github.com/sqeel-sql/sqeel`
- Create org + repo before M0 scaffold

## CI/CD

GitHub Actions runs on every push and PR to `sqeel/sqeel`.

### Workflow

- `cargo test --workspace` — full test suite
- DB tests spin up MySQL + SQLite via GitHub Actions service containers
- `cargo clippy -- -D warnings` — no warnings allowed
- `cargo fmt --check` — enforced formatting
- Both binaries (`sqeel`, `sqeel-gui`) must build cleanly
- Matrix: Linux (always), Windows + macOS (post-M3)

### Release Pipeline

- Triggered on new git tag (`v*`)
- Builds `sqeel` + `sqeel-gui` for all three platforms (Linux, Windows, macOS)
- Artifacts uploaded to GitHub Release for that tag
- Naming: `sqeel-v1.0.0-x86_64-linux`, `sqeel-gui-v1.0.0-x86_64-windows.exe`,
  etc.

## Stack

### Core

- `sqlx` — async multi-DB (MySQL first, SQLite/PostgreSQL later)
- `tokio` — async runtime
- `tree-sitter` + `tree-sitter-sql` — dialect-aware syntax highlighting
- `tower-lsp` or raw LSP client — LSP communication with `sqls`
- `dirs` — cross-platform config dir resolution

### TUI provider (`sqeel` binary)

- `ratatui` — layout + widgets
- `tui-textarea` — vim bindings in editor

### GUI provider (`sqeel-gui` binary)

- `iced` — native GUI, elm-style
- Custom vim-mode editor widget
- Native OS window, font rendering, mouse support

## Milestones

### M0–M7 ✅ Complete

All milestones shipped. Core abstraction, TUI/GUI providers, editor with vim/emacs modes, tree-sitter highlighting, LSP integration, DB connections (MySQL/SQLite/PostgreSQL), results pane, schema browser, config system, persistence, and polish.

## DB Support Priority

1. MySQL/MariaDB
2. SQLite
3. PostgreSQL

## Config (future)

All paths resolved via `dirs::config_dir()` — platform-appropriate on Linux,
macOS, and Windows.

### Main config — `~/.config/sqeel/config.toml`

```toml
[editor]
keybindings = "vim"  # or "emacs"
lsp_binary = "sqls"  # path to LSP binary
```

### Per-connection config — `~/.config/sqeel/conns/<name>.toml`

Each connection is its own file. Filename becomes the connection name shown in
UI.

```toml
# ~/.config/sqeel/conns/local.toml
url = "mysql://localhost/mydb"
```

```toml
# ~/.config/sqeel/conns/staging.toml
url = "mysql://user:pass@staging-host/mydb"

[tls]
ca_cert = "/path/to/ca.pem"
```

sqeel scans `conns/` on startup and loads all `.toml` files found.

### Data directory — `~/.local/share/sqeel/` (platform-resolved via `dirs::data_dir()`)

```
~/.local/share/sqeel/
  queries/          ← auto-saved SQL buffers (*.sql)
  results/          ← last 10 successful query results (*.json, errors excluded)
```

## Name

SQEEL — because it makes other SQL clients cry.
