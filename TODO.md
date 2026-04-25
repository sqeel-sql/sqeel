# sqeel-vim — vim-feature backlog

The buffer migration (Phases 0–8) and the first round of unlocked features
(Phase 9: registers, macros, special marks, ex global, folding) are done. Git
history holds the per-phase commits.

Below: concrete plans for vim features the engine is still missing. Grouped by
area, ranked **S/M/L** by effort. Pick a chunk, work it, keep tests green.

---

## Folding follow-ups (S–M)

Folding shipped manually — selection-based `zf`, the open/close/
toggle/`zR`/`zM`/`zd` chord set, and edit-side invalidation. What's left:

- ~~**`zf{motion}` in normal mode (S).**~~ Done. Refactored as `Operator::Fold`
  so it rides the existing operator pipeline: motions, text objects (`zfip` /
  `zfap` / `zfiw`), `g`-prefix chords (`zfgg`), and inner counts (`zf3j`) all
  work for free. Visual `zf` keeps its inline path; `Operator::Fold` is
  unreachable from `apply_visual_operator`.
- ~~**Fold-aware `j` / `k` (M).**~~ Done. Added `Buffer::next_visible_row` /
  `prev_visible_row`; `move_vertical` now walks one visible row at a time so
  closed folds count as a single visual line. Cursor-on-hidden-row latent bug
  still exists but the new helpers handle it gracefully (next_visible from a
  hidden row still walks past the fold).
- **`foldmethod=indent` (M).** Auto-derive folds from leading- whitespace runs.
  Triggered manually via a new ex command (`:foldindent`?) since
  auto-fold-on-edit is expensive. Drop into `Buffer::add_fold` for each run.
- **`foldmethod=syntax` (L).** Tree-sitter already runs in sqeel- tui's
  `apply_window_spans` flow. Tap the same parser to extract block ranges (CTEs,
  subqueries, parenthesised lists) and pipe them as folds. Needs a per-row →
  fold cache that survives edits via the same dirty-gen scheme spans use.

---

## Registers + macros (S–M)

- **Macro storage in registers (M).** Macros currently live in a separate
  `HashMap<char, Vec<Input>>`. Vim stores them as text inside the matching
  register so `"ap` pastes the macro and `"ay` saves an edited macro back.
  Decide on an `Input ↔ string` encoding (probably vim's `<C-x>` notation), wire
  `record_*` / `read` to translate. Drop the separate map.
- ~~**Nested `@b` inside `qa` recording (S).**~~ Verified — `qa@bq` captures the
  literal `@`/`b` keys; replay invokes the previously recorded macro at that
  point. Test added.
- ~~**`Ctrl-R {reg}` in insert mode (S).**~~ Done. `Ctrl-R` arms an
  `insert_pending_register` flag; the next char selects the register and its
  text inserts inline (single `Edit::InsertStr`, cursor lands at end of
  payload). Stays in insert mode after.
- ~~**`:reg` / `:registers` ex command (S).**~~ Done. Returns
  `ExEffect::Info(table)` with every non-empty slot; toast renderer now expands
  vertically for multi-line `Info` payloads.
- ~~**System clipboard registers `"+` / `"*` (M).**~~ Done. `Registers` gains a
  shared `clip` slot aliased between `+` and `*`. The selector parser accepts
  both; `record_yank`/`record_delete` write to the slot, and the existing
  `last_yank` pipe lets sqeel-tui push the same text to the OS clipboard.
  Inbound paste path: sqeel-tui peeks `editor.pending_register_is_clipboard()`
  and calls `sync_clipboard_register` before handling the key, so `"+p` / `"*p`
  read the live OS clipboard.

---

## Marks (S–M)

- **File-global marks `A-Z` (M).** Vim stores `A-Z` per buffer _file_, not the
  editor session. Sqeel has tabs (one buffer per tab); store global marks on
  `AppState` keyed by `(tab_id, char)` and surface via a host accessor.
  Lowercase / special marks stay buffer-local.
- ~~**`:marks` ex command (S).**~~ Done. Prints every user mark plus the special
  `'` (last jump) and `.` (last edit), one per row; lines are 1-based to match
  vim.
- ~~**Mark migration on edit (M).**~~ Done. `mutate_edit` measures the row-count
  delta and calls `shift_marks_after_edit` to migrate user marks + jumplist
  entries: marks above the edit stay, marks past the affected band shift by
  `delta`, marks tied to deleted rows are dropped. Restore-based paths (undo,
  sort, substitute) bypass this — they replace the buffer wholesale, so marks
  may go stale across those operations. Tracked separately if it bites.
- ~~**`g;` / `g,` (M).**~~ Done. `mutate_edit` appends to a bounded
  `change_list` (cap 100, consecutive-cell dedupe). `g;` walks toward older
  entries, `g,` toward newer; counts compound (`3g;`). A new edit during a walk
  truncates the forward half (vim's branching rule).

---

## Text objects (S–M)

We support some `OpTextObj` chords. Audit + fill gaps:

- ~~**Audit existing.**~~ Done. Added tests for every text object's inner +
  around `d` form (word, big-word, quote, single-quote, backtick, paren / `b`,
  bracket, brace / `B`, angle, paragraph) plus operator-pipeline spot checks for
  `c`/`y`/`v` against word, paren, quote, paragraph. XML tag has full op
  coverage from the `it`/`at` task.
- ~~**`it` / `at` (M).**~~ Done. Added `TextObject::XmlTag` with a stack-based
  `tag_text_object` that flattens the buffer, walks `<…>` tokens, pairs opens to
  closes, and returns the innermost pair containing the cursor. Skips
  `<!--`/`<?` and self-closing `<x/>`.
- ~~**`ip` / `ap` (S).**~~ Already done — `TextObject::Paragraph` wired,
  `paragraph_text_object` walks blank-line boundaries, `ap` includes one
  trailing blank. Verified by `dap_deletes_paragraph` test.
- ~~**`is` / `as` (M).**~~ Done. `TextObject::Sentence` + a flat-char walk that
  splits on `.`/`?`/`!` followed by whitespace; consecutive terminators (`?!`)
  collapse into one boundary. `as` extends through trailing whitespace; `is`
  does not.

---

## Motions (S–M)

- ~~**`(` / `)` — sentence motions (M).**~~ Done. `Motion::SentencePrev` /
  `SentenceNext` reuse the same boundary detection as `is`/`as`; `(` walks to
  the previous sentence start, `)` to the next, counts compound.
- ~~**`gM` (S).**~~ Done. Jumps to `floor(chars / 2)` of the current line.
  (Implemented per current-line midpoint, not the longest-screen-line variant —
  matches vim's documented `gM` behaviour.)
- ~~**`*` / `#` already exist; add `g*` / `g#` (S).**~~ Done.
  `Motion::WordAtCursor` now carries a `whole_word` flag; `*` / `#` set it, `g*`
  / `g#` drop it for substring matches.

---

## Operators (S)

- ~~**`R` — Replace mode (M).**~~ Done. `InsertReason::Replace` flavour of
  insert mode; `handle_insert_key` overstrikes the cursor cell when the session
  is in Replace mode (delete one char then insert the typed char). At
  end-of-line falls through to plain insert, matching vim. Backspace does not
  restore prior content (vim has a per-char history for that — pragmatic gap).
- **`gq{motion}` — text reflow (L).** Vim wraps to `textwidth`. SQL doesn't
  usually want this; hold off unless someone asks.
- ~~**`>>` / `<<` already exist; add `>{motion}` / `<{motion}` audit (S).**~~
  Verified — `>w` / `<w` indent / outdent the line, `>ip` / `<ip` span paragraph
  rows. Tests added.

---

## Insert mode (S)

- ~~**`Ctrl-R {reg}` (S).**~~ Done — see registers section.
- ~~**`Ctrl-W` / `Ctrl-U` / `Ctrl-H` (done).**~~ Verified — `Ctrl-W` at col 0
  already joins with the previous row and deletes the word now before the cursor
  (matches vim's `backspace=indent,eol,start` default). Two regression tests
  added.
- ~~**`Ctrl-O` already exists.**~~ Verified — runs exactly one normal command,
  then drops back to insert. Test confirms a follow-up keypress lands as insert
  text rather than a second normal command.
- **Bracket auto-pairing (out of scope).** Leave for an opt-in plugin layer if
  it ever exists.

---

## Ex commands (S–M)

Today: `:q`, `:q!`, `:w`, `:wq`, `:x`, `:noh`, `:s/`, `:%s/`, `:g/`, `:v/`, `:N`
(line jump). Backlog:

- ~~**`:read file` / `:r file` (M).**~~ Done. `apply_read_file` calls
  `std::fs::read_to_string`, drops the file's trailing newline, and inserts the
  content below the current row via a single `Edit::InsertStr`. Cursor lands on
  the first inserted row. Failures surface as `ExEffect::Error`. Path resolution
  stays the user's responsibility — pass an absolute path or a path relative to
  the process CWD.
- **`:r !cmd` (L).** Insert shell command output. Powerful but needs a sandbox
  story; defer.
- ~~**`:set` (M).**~~ Done. Added
  `Settings { shiftwidth, tabstop, ignore_case }` on Editor. `:set sw=N`,
  `:set ts=N`, `:set [no]ignorecase` (or `ic`) all work; bare `:set` reports the
  current values via `ExEffect::Info`. `shiftwidth` flows through `indent_rows`
  / `outdent_rows` / `Ctrl-T`; `ignore_case` flips both `/` search and `:s`
  substitute (explicit `i` flag still wins). `tabstop` stored but not yet wired
  to render. `foldenable` accepted as a no-op so vimrc copies don't error.
- **`:earlier` / `:later` (L).** Time-tree undo. Out of scope — the current undo
  is a flat stack.
- **`:registers` / `:reg` (S).** Listed under registers.
- **`:marks` (S).** Listed under marks.
- ~~**`:sort` (M).**~~ Done — whole-buffer sort with vim flags `!` (reverse),
  `u` (unique), `n` (numeric), `i` (ignore case). Combinable (`:sort! u`).
  Pushes undo so `u` reverses. Range support deferred — comes for free once the
  range parser lands.
- **`:! cmd` (L).** Run shell, insert nothing. Same security caveats as `:r !`.
- **`:!{filter}` over a range (L).** Pipe range through external filter. Same
  caveats.
- ~~**`:undo` / `:redo` (S).**~~ Done. `:undo` / `:u` and `:redo` / `:red` drive
  the same `do_undo` / `do_redo` paths as `u` / `Ctrl-R`.
- ~~**Range support before commands (M).**~~ Done. `parse_range` strips a
  leading address pair (`N`, `N,M`, `.`, `$`, `'a`, `%`) and resolves to a
  0-based inclusive `Range`. `:[range]s/…`, `:[range]sort`, `:[range]g/`, and a
  new `:[range]d` all honour it. No range = each command's natural scope
  (current line for `:s`, whole buffer for `:sort` / `:g`). Address arithmetic
  (`+N` / `-N`) deferred.

---

## Search (S)

- ~~**Search history (M).**~~ Done. Bounded `search_history: Vec<String>` on
  `VimState` (cap 100, consecutive-dedupe). `Ctrl-P` / `Up` walks toward older
  entries, `Ctrl-N` / `Down` toward newer; typing or backspacing resets the walk
  cursor.
- ~~**`?` — backward search prompt (audit).**~~ Done. Found a real bug: `n`/`N`
  always walked forward/backward regardless of search direction. Added
  `last_search_forward` flag set on every commit and on `*`/`#`;
  `Motion::SearchNext` now flips on it so `n` repeats the prompt's direction and
  `N` inverts.
- ~~**`/<CR>` — repeat last search (S).**~~ Done. Empty `<CR>` reuses
  `last_search` in the prompt's direction; `enter_search` no longer wipes the
  pattern when opening the prompt.

---

## Visual (S)

- _(no open items — `gv` and `o`-swap shipped)_

---

## Render polish (M–L)

- **Soft-wrap render (L).** Long SQL lines often blow past terminal width. Add a
  `wrap: Wrap::None | Wrap::Char | Wrap::Word` enum on `BufferView`; the render
  walks a synthetic "screen line" stream. Affects motion (`gj`/`gk` start to
  matter), gutter (line numbers on continuation rows), cursor placement.
- **Concealed regions (M).** Render-time hide/replace of byte ranges (e.g. URL
  prettying). Buffer ignores it; `BufferView` takes a list of
  `(row, byte_range, replacement)`.
- ~~**Cursorcolumn (S).**~~ Done. `BufferView` gains `cursor_column_bg`; the
  render walker layers that bg over the cursor's visible column after row
  painting so it composes with cursorline / syntax. sqeel-tui ties it to the
  existing cursor-line bg for now (no separate theme slot yet).
- ~~**Better fold marker (S).**~~ Done. `paint_fold_marker` now reads the fold's
  start-row content and renders `▸ {trimmed prefix} ({N} lines)` so the marker
  hints at what's inside. Empty start rows fall back to `▸ {N} lines folded`.

---

## Polish / parity (S)

- ~~**Macro / register interop tests.**~~ Done. Locked in current behaviour:
  `"ay…` populates register `a` with text but `@a` is a no-op since macros and
  registers still live in separate stores. The follow-up M task ("macro storage
  in registers") will flip this; the test will fail then and force the
  unification.
- ~~**Replay still respects mode-switching mid-macro.**~~ Verified — recording
  `iX<Esc>0` and replaying lands the cursor in normal mode at col 0 with `X`
  inserted at the start of the line.
- ~~**`.` after a macro**~~ Verified — after `@a` runs a macro whose last edit
  was an insert, `.` repeats only that final change, not the whole macro key
  sequence.

---

## Out of scope (for now)

- Multi-cursor.
- Window splits / `Ctrl-W` chord.
- Bidirectional text.
- `:terminal`.
- LSP-driven rename / code action chords (separate axis from vim parity).
