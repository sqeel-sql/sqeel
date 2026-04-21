# TODO

## Performance Optimizations

### High Priority

- [x] **Dirty-tracking for render loop** (`sqeel-tui/src/lib.rs:252`) Only call
      `terminal.draw` when state-dirty flag set or tick requires it. Polling
      wakes every 50ms and triggers full redraw regardless of changes —
      amplifies every other issue below.

- [ ] **Cache flattened schema tree** (`sqeel-core/src/state.rs:281`)
      `visible_schema_items()` calls `flatten_tree` on every frame and every key
      event. Cache flattened list on `AppState`, invalidate only on schema
      change (toggle/refresh/append). Mouse wheel should call
      `schema_cursor_advance(n)` once rather than spin `schema_cursor_down` N
      times.

- [ ] **Cache column widths on QueryResult** (`sqeel-tui/src/lib.rs:1540-1554`)
      Column-width scan is O(rows×cols) on every frame. Compute widths once when
      `QueryResult` is assigned, store on struct.

- [ ] **Arc<String> for editor content** (`sqeel-tui/src/lib.rs:141-152`)
      Triple-clone of full editor content under Mutex lock on every keystroke.
      Use `Arc<String>` so store/submit are cheap clones; move
      `highlight_thread.submit` outside the lock.

- [ ] **Async autosave** (`sqeel-core/src/state.rs:653-670`) `std::fs::write`
      runs synchronously while holding the Mutex in the render loop. Spawn
      dedicated autosave thread with channel, or tokio task.

- [ ] **Fix O(n²) completion merge** (`sqeel-tui/src/lib.rs:218-225`)
      `Vec::contains` is O(n) inside a loop over completions. Build `HashSet`
      from `merged` first before iterating `last_schema_completions`.

- [ ] **Cache lowercased schema identifiers**
      (`sqeel-core/src/state.rs:245-257`,
      `sqeel-tui/src/completion_thread.rs:35-39`) `schema_identifier_names`
      allocates `name.to_owned()` for every node on every keystroke. Completion
      thread then lowercases every identifier per request. Cache
      `Arc<Vec<String>>` of names and lowercased names, rebuild only on schema
      change.

- [ ] **HashMap in merge_expansion** (`sqeel-core/src/schema.rs:146-184`) Linear
      `.find()` over old siblings per new sibling = O(new×old) per level. For
      10k tables this is 10⁸ ops. Build `HashMap<&str, &SchemaNode>` once per
      recursion level.

- [ ] **Avoid clone in refresh_schema_nodes**
      (`sqeel-core/src/state.rs:377-382`) Deep clones entire tree for
      `merge_expansion`. Use `std::mem::take` to avoid the clone; rework
      `merge_expansion` to borrow `&self.schema_nodes`.

- [ ] **Bulk schema cursor movement** (`sqeel-tui/src/lib.rs:377-413`) Mouse
      scroll spins single-step calls, each triggering flatten/lookups. Add
      `schema_cursor_advance(n)` and `scroll_results_down_by(n)` bulk variants.

### Medium Priority

- [ ] **VecDeque for undo stack** (`sqeel-tui/src/editor.rs:156-167`)
      `undo_stack.remove(0)` is O(n) Vec shift when >200 entries. Use `VecDeque`
      for O(1) `pop_front`. Consider storing diffs instead of full snapshots.

- [ ] **Cache SQL keyword regex** (`sqeel-tui/src/lib.rs:1462-1464`)
      `set_search_pattern` called with full keyword regex every frame when no
      user search active. If tui-textarea recompiles each time, this is
      per-frame regex compile. Compile once at init, only update on user search
      state change.

- [ ] **Byte-slice approach for extract_inserted**
      (`sqeel-tui/src/editor.rs:967-989`) Collects `before`/`after` into
      `Vec<char>` (multi-MB on large files) just to find prefix/suffix. Work
      directly on byte slices with char-boundary awareness.

- [ ] **Cache schema search filtering** (`sqeel-tui/src/lib.rs:1319-1332`)
      Per-frame: `visible_schema_items()` clone into filtered Vec, then
      `label.to_lowercase()` per item. Cache lowercased labels; recompute
      filtered list only when query or nodes change.

- [ ] **Stream query results** (`sqeel-core/src/db.rs:49-78`) `fetch_all` pulls
      every row into RAM before display. Stream with `fetch` into channel,
      paginate, or LIMIT by default. Cache column count outside loop; dispatch
      `decode_cell_any` on type once instead of trying 5 types sequentially.

- [ ] **Arc<String> for tab content**
      (`sqeel-core/src/state.rs:580,624,666-668`) Tab switch clones full editor
      content twice (memory + disk). Use `Arc<String>` sharing between
      `editor_content` and `tab.content`.

- [ ] **Optimize find_cursor_by_path / restore_expanded_paths**
      (`sqeel-core/src/schema.rs:256-320`) `find_cursor_by_path` calls
      `path_to_string` for every item rebuilding strings.
      `restore_expanded_paths` runs `splitn` + nested linear scan per path.
      Cache path strings; use maps.

- [ ] **Move evict_cold_tabs off render loop** (`sqeel-tui/src/lib.rs:137`)
      Called every frame (~20Hz) adding lock pressure just to check timestamps.
      Run on 1-second timer instead.

- [ ] **CursorMove::Jump for scroll** (`sqeel-tui/src/editor.rs:120-130`)
      `scroll_down`/`scroll_up` iterate N times each calling `CursorMove::Down`.
      Use `CursorMove::Jump` once.

- [ ] **Remove or verify dead schema_identifier_completions**
      (`sqeel-core/src/state.rs:261-279`) Likely superseded by background
      completion thread (commit b0c707a). If dead code, remove.

### Low Priority

- [ ] **Cache help text Lines** (`sqeel-tui/src/lib.rs:1808-1825`) Static help
      content rebuilt into `Vec<Line>` on every frame. Compute once.

- [ ] **Fix word_prefix_at double-reverse** (`sqeel-tui/src/lib.rs:1590-1603`)
      Collects reversed chars then re-reverses. Use `rfind` on byte slice with
      ASCII fast path.

- [ ] **Tab bar click width rebuild** (`sqeel-tui/src/lib.rs:298-308`) Rebuilds
      tab widths on every click. Fine for small N but takes lock per iteration.

- [ ] **diag_label single-pass** (`sqeel-tui/src/lib.rs:860-883`) Iterates
      diagnostics twice (once per severity). Single-pass count.

- [ ] **LSP parse before id match** (`sqeel-core/src/lsp.rs:261-274`)
      `CompletionResponse` parsed before id matching. Minor: match id first,
      then parse.

- [ ] **highlight_spans dead storage** (`sqeel-core/src/state.rs:95,194-196`)
      Set on every edit but TUI render path uses tui-textarea's own
      highlighting. Verify unused and remove.

- [ ] **build_tab_title per-frame format!** (`sqeel-tui/src/lib.rs:1490-1508`)
      `format!(" {} ", name)` allocated per tab per frame. Cache on TabEntry.
