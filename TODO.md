# sqeel — backlog

The vim engine is feature-complete for sqeel's purposes — buffer migration,
folding, registers, macros, marks, text objects, motions, operators, ex
commands, search, visual modes, and soft-wrap have all shipped. Git history
holds the per-feature commits. The remaining backlog is connection-management UX
rough edges flagged by the audit; vim-feature additions live in git history,
this file in `## Out of scope`, or — in a pinch — a fresh issue.

---

## Connection management UX (S–L)

Quick wins first, then the larger structural fixes. Audit pointed at concrete
file:line targets; each item carries them so the work is mechanical.

- ~~**Delete confirmation in the switcher (S).**~~ Done. New
  `connection_delete_armed: Option<String>` on `AppState`. First `d` arms the
  delete on the highlighted entry and surfaces a status hint ("Delete `{name}`?
  d/Enter to confirm…"); second `d` (or `Enter`) commits. Esc, j/k movement,
  opening another modal, or any unbound key disarms via
  `disarm_connection_delete`. `confirm_connection_switch` short-circuits to the
  commit path when armed so the muscle-memory picker flow still works.
- ~~**Password warning on save (S).**~~ Done. New `url_has_plaintext_password`
  parses the userinfo segment of a saved URL; on a positive hit
  `save_new_connection` raises a status-bar warning pointing the user at the
  toml path with a `chmod 0600` / passwordless-userinfo hint. Doesn't block the
  save — just informs. Query-string passwords aren't detected (rare in practice;
  would need a real URL parser to handle).
- ~~**URL sanity check at save time (S).**~~ Done. New `validate_connection_url`
  runs in `save_new_connection` before the toml write: rejects unknown schemes
  (allowed: `mysql`, `mariadb`, `postgres`, `postgresql`, `sqlite`),
  `mysql:host` (single-colon), and bodyless URLs like `mysql://`.
  `sqlite::memory:` is the one form that's allowed without `://`. Form keeps the
  typed values on rejection so the user can fix in place.
- **Retry on connection failure (S).** When the async handshake errors
  (`sqeel/src/bin/sqeel.rs:358-367`), the only recovery is re-opening the
  switcher and pressing Enter again. Add a `r` keybinding (or `Ctrl-r`) on the
  schema pane's "Connection failed" placeholder that re-runs
  `pending_reconnect = Some(active_slug)`. Show the connection name in the retry
  hint so the user knows what they're retrying.
- **Connection state badge in switcher (S).** Switcher only marks the active
  connection with `*` (`sqeel-tui/src/lib.rs:6210-6281`). Plumb the live state
  from `AppState` (`connection_status: Connected | Connecting | Error`) into the
  rendered list — e.g. `● mysql-prod` (green) for connected, `◌ mysql-staging`
  (yellow) for connecting, `✗ broken-conn` (red) for the last-tried connection
  that failed. State already lives in core; switcher just doesn't read it yet.
- **Better connection error messages (S).** The placeholder shows the raw `{e}`
  from sqlx (`sqeel-tui/src/lib.rs:4025-4031`). Wrap the error in a
  `ConnectError { kind: Network | Auth | Dns | Tls | Other, detail: String }` in
  `sqeel/src/bin/sqeel.rs` so the placeholder can render
  `Auth failed: bad password` vs `Network: connection refused` vs
  `DNS: host not found`. Pattern-match on sqlx's `Error::Database`,
  `Error::Io(io::ErrorKind::ConnectionRefused | NotFound)`, etc. Falls back to
  `Other` for anything unrecognised.
- **Manual schema refresh (S).** Schema TTL is 300s
  (`sqeel-core/src/config.rs:24`); after a `CREATE TABLE` the browser shows
  stale state until expiry or reconnect. Add ex command `:refreshschema` /
  `:refresh` and a default mapping (e.g. `<leader>R`) that bumps schema
  invalidation regardless of TTL. Should reuse the same code path the reconnect
  flow uses for fetching schema.
- ~~**First-run inline prompt (S–M).**~~ Done. Different UX from the original
  spec: empty config dir + no `-c`/`-u` flag → `bin/sqeel.rs` calls
  `open_add_connection()` right after `load_connections()` so the user lands
  directly in the add-connection form instead of a blank TUI. Esc still bails
  out of the form, leaving the connection switcher one keystroke away
  (`<leader>c`).
- **Don't clobber unsaved tabs on connection switch (M).**
  `confirm_connection_switch` → `load_tabs_for_connection` replaces the entire
  tabs list (`sqeel-core/src/state.rs:2909-2948`); any unsaved scratch buffers
  from connection A vanish when the user picks B. Two-part fix: (1) before
  switching, walk the current tabs and write any dirty buffers to their backing
  files (the auto-save path already exists for normal edits); (2) if any tab is
  _unbacked_ (no path yet), prompt
  `"N unsaved tabs will close — Switch anyway? (y/n)"` and only proceed on `y`.
  Cancelling restores the picker's previous selection.
- **TLS / SSH tunnel form fields (M–L).** Connection form is name + URL only
  (`sqeel-tui/src/lib.rs:6453-6525`); TLS requires hand-editing the toml's
  `[tls]` section, SSH tunneling isn't supported at all. Phase 1 (M): add
  optional TLS rows to the form (`ca_cert`, `client_cert`, `client_key`,
  `verify_mode: full|skip`); on save serialise to the existing `[tls]` toml
  block parsed by `sqeel-core/src/config.rs:177-202`. Phase 2 (L): `[tunnel]`
  section + form fields for SSH host / user / key, with a side car
  `russh`-driven port-forward spawned alongside the sqlx pool. Phase 2 needs a
  thread-safety pass on the connection lifecycle and is the bigger spend.
- **Keyring-backed secrets (L).** Make plaintext password storage opt-out. Use
  `keyring` crate (Linux secret-service / macOS Keychain / Windows Credential
  Manager). Connection form gains a separate `Password:` field that writes the
  URL as `mysql://user@host` (no password segment) to toml and stashes the
  secret under `keyring::Entry::new("sqeel", &slug)`. On load,
  `sqeel-core/src/config.rs` rewrites the URL with the keyring password before
  handing it to sqlx. Migration: existing `mysql://user:pass@…` URLs keep
  working (no behaviour change); the password warning toast (S item above) gains
  an extra "Run `:migrate-secrets` to move them to your OS keychain" hint. CI:
  `keyring` needs a fake backend for headless runs
  (`keyring::set_default_credential_builder(mock)`). Platform-touchy — Linux
  without `dbus` (containers, sshfs) silently falls back to plaintext; document
  and warn.

---

## Theming (L)

Today's `theme.toml` uses sqeel-internal slot names (`schema_pane_bg`,
`results_col_active_bg`, …) and a flat `fg`-only palette per slot. Vim
colorschemes ship with a different shape: highlight groups by name (`Normal`,
`Comment`, `Search`, `CursorLine`, …) each carrying `guifg`, `guibg`, and `gui`
attributes (`bold`, `italic`, `underline`, `reverse`, `undercurl`, `none`).
Re-shape sqeel's theme system so a port of an existing vim colorscheme is a 1:1
copy of its `:hi` lines instead of a custom mapping exercise.

- **Phase 1 — TOML schema redesign (M).** Replace `ui.<slot> = "<color>"` with
  `[hl.<group>]` tables that take `fg`, `bg`, `attrs`. Standard vim group names
  (`Normal`, `NonText`, `Cursor`, `CursorLine`, `CursorLineNr`, `LineNr`,
  `Comment`, `Constant`, `String`, `Number`, `Boolean`, `Identifier`,
  `Function`, `Statement`, `Keyword`, `Type`, `Special`, `Operator`, `PreProc`,
  `Underlined`, `Error`, `Todo`, `Visual`, `Search`, `IncSearch`, `MatchParen`,
  `Pmenu`, `PmenuSel`, `TabLine`, `TabLineFill`, `TabLineSel`, `StatusLine`,
  `StatusLineNC`, `Folded`, `FoldColumn`, `SignColumn`,
  `DiffAdd`/`Change`/`Delete`/`Text`, `ErrorMsg`, `WarningMsg`, `ModeMsg`,
  `Title`, `Directory`, `DiagnosticError`/`Warn`/`Info`/`Hint`). `attrs` is a
  comma list matching vim's `gui=` syntax. Missing groups inherit from `Normal`.
  Plus a `[palette]` table for symbolic refs (already supported).
- **Phase 2 — UiColors → HighlightGroups (M).** Drop the macro-generated flat
  `UiColors` struct; replace with
  `HighlightGroups { groups: HashMap<&'static str, Style> }` keyed by lowercase
  vim group name. Render call sites switch from `ui().schema_pane_bg` to
  `hl("Normal").bg`/`hl("StatusLine")` etc. Hand-write the slot ↔ group mapping
  table once; downstream consumers go through the new accessor.
- **Phase 3 — Bundled themes (S).** Port `tokyonight.toml` to the new schema.
  Add at least one more bundled theme (gruvbox or catppuccin) so the system is
  exercised by more than one source. Bundled themes ship inline via
  `include_str!` — no extra crate dep.
- **Phase 4 — `:colorscheme` ex command (S).** `:colorscheme name` loads
  `~/.config/sqeel/colors/<name>.toml`, falling back to bundled names.
  `:colorscheme` bare lists the choices via `ExEffect::Info`. Re-renders without
  a restart.
- **Phase 5 — Vim `.vim` colorscheme importer (L, optional).** A shell-side
  `sqeel-theme-import path/to/scheme.vim` helper that scrapes `:hi` lines via a
  tiny regex parser, emits the equivalent `colors/<name>.toml`. Punts on
  `:hi link` chains (resolve transitively) and on `cterm`-only colorschemes
  (`guifg` required). Probably a separate binary in the workspace so the main
  `sqeel` binary doesn't pull in the extra parsing surface. Optional because
  once Phase 1–4 ship, hand-writing one toml is small enough that auto-import is
  nice-to-have rather than necessary.

Total span: M + M + S + S + L. Phases 1–4 are the full feature; Phase 5 is the
convenience layer. Migration risk: every render call site that reads `ui()`
needs a touch (~120 sites in `sqeel-tui/src/lib.rs`); a shim that maps old slot
names to new groups during the transition keeps each commit reviewable.

---

## Out of scope

- Bracket auto-pairing — leave for an opt-in plugin layer if one ever exists.
- `:earlier` / `:later` — time-tree undo; the current undo stack is flat.
- Multi-cursor.
- Window splits / `Ctrl-W` chord.
- Bidirectional text.
- `:terminal`.
- LSP-driven rename / code action chords (separate axis from vim parity).
