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

- **Delete confirmation in the switcher (S).** `d` in the connection switcher
  drops the file immediately (`sqeel-core/src/state.rs:2875-2888`). Add a
  two-step prompt: first `d` arms a pending-delete on the highlighted entry,
  second `d` (or `Enter`) commits; any other key cancels. Show a one-line toast
  "Delete `{name}`? d/Enter to confirm." while armed. Match vim's
  `:bdelete!`-style "must press twice" feel rather than a modal — modal would
  break the muscle memory of the picker.
- **Password warning on save (S).** Adding or editing a connection whose URL has
  a `password` userinfo segment (`mysql://user:pass@…`) writes plaintext to
  `~/.config/sqeel/conns/<name>.toml` with zero feedback
  (`sqeel-core/src/config.rs:177-202`). After a successful save in
  `apply_connection_form` (`sqeel-core/src/state.rs:2843-2862`), parse the URL
  and if `password` is non-empty show a toast: "Password stored in plaintext at
  `~/.config/sqeel/conns/{name}.toml`. Set file mode 0600 or use
  `mysql://user@host` to be prompted." Don't block the save — just inform.
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
- **First-run inline prompt (S–M).** Empty config dir → blank TUI with
  `(no connections configured)` in a hidden switcher
  (`sqeel-tui/src/lib.rs:6256`). Detect the empty case in `App::run`/main draw
  and show a centred panel: "No connections yet. Press `<leader>c` then `n` to
  add one — or hit `?` for the help overlay." Stays until the user opens the
  switcher or adds a connection. Pure UI; no new state.
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

## Out of scope

- Bracket auto-pairing — leave for an opt-in plugin layer if one ever exists.
- `:earlier` / `:later` — time-tree undo; the current undo stack is flat.
- Multi-cursor.
- Window splits / `Ctrl-W` chord.
- Bidirectional text.
- `:terminal`.
- LSP-driven rename / code action chords (separate axis from vim parity).
