use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use sqeel_core::{
    AppState, UiProvider,
    config::{load_connections, load_main_config, load_session_data, save_session},
    db::DbConnection,
    ddl::parse_ddl,
    persistence::{evict_old_results, load_result_for, sanitize_conn_slug},
    schema::SchemaNode,
    state::{QueryRequest, ResultsPane, ResultsTab, SchemaLoadRequest},
};
use sqeel_tui::TuiProvider;

const SAMPLE_QUERY: &str = "CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY,
    email TEXT NOT NULL,
    display_name TEXT NOT NULL
);

INSERT INTO users (email, display_name) VALUES ('alice@example.com', 'Alice');
INSERT INTO users (email, display_name) VALUES ('bob@example.com', 'Bob');

SELECT * FROM users;
";

/// Repoint sqeel's config + data dirs at a fresh tempdir, then seed
/// a `sample` SQLite connection + a sample `.sql` buffer so the
/// user lands in a working state without touching their real
/// `~/.config/sqeel` or `~/.local/share/sqeel`.
///
/// Returns the sandbox root so `main()` can prompt for cleanup on
/// exit. The tempdir uses [`tempfile::Builder::keep`] so the dir
/// survives the process unless the user opts in to deletion.
fn bootstrap_sandbox(empty: bool) -> anyhow::Result<PathBuf> {
    let tmp = tempfile::Builder::new()
        .prefix("sqeel-sandbox-")
        .tempdir()?;
    let root: PathBuf = tmp.keep();
    let config = root.join("config");
    let data = root.join("data");
    let queries = data.join("queries");
    std::fs::create_dir_all(config.join("conns"))?;
    std::fs::create_dir_all(&queries)?;
    std::fs::create_dir_all(data.join("results"))?;

    if !empty {
        // Seed a sample SQLite connection pointing at a fresh DB file.
        let db_path = data.join("sample.db");
        let conn_toml = format!(
            "name = \"sample\"\nurl = \"sqlite://{}\"\n",
            db_path.display()
        );
        std::fs::write(config.join("conns").join("sample.toml"), conn_toml)?;

        // Seed a sample SQL buffer with a CREATE TABLE for users.
        std::fs::write(queries.join("sample_users.sql"), SAMPLE_QUERY)?;

        // Seed a session pointing at the sample connection so the TUI
        // opens it on launch instead of dropping the user into "no
        // connections selected".
        std::fs::write(
            config.join("session.toml"),
            "connection = \"sample\"\n\
             schema_cursor = 0\n\
             schema_expanded_paths = []\n\
             focus = \"Editor\"\n\
             tab_cursors = []\n\
             active_tab = 0\n\
             result_tabs = []\n\
             active_result_tab = 0\n",
        )?;
    }

    sqeel_core::config::set_config_dir_override(config);
    sqeel_core::persistence::set_data_dir_override(data);
    let label = if empty { "sandbox (empty)" } else { "sandbox" };
    eprintln!("sqeel {label}: {}", root.display());
    Ok(root)
}

/// Print a stderr y/N prompt and read a single line of stdin.
/// Returns true iff the user typed `y` or `Y` (with optional
/// whitespace). Default is "no" — Enter or any other key keeps
/// the dir intact so a misclick can't nuke the user's work.
fn prompt_yes_no(question: &str) -> bool {
    use std::io::{BufRead, Write};
    eprint!("{question} [y/N]: ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y")
}

/// Best-effort cleanup of the sandbox dir. Logged failures don't
/// surface as a process error — the user's work in the parent shell
/// shouldn't die because `/tmp` is wedged.
fn cleanup_sandbox(root: &std::path::Path) {
    if let Err(e) = std::fs::remove_dir_all(root) {
        eprintln!("sqeel sandbox: cleanup failed for {}: {e}", root.display());
    } else {
        eprintln!("sqeel sandbox: removed {}", root.display());
    }
}

#[derive(Parser)]
#[command(name = "sqeel", about = "Fast vim-native SQL client")]
struct Args {
    /// Connection URL (e.g. mysql://user:pass@host/db)
    #[arg(short = 'u', long)]
    url: Option<String>,

    /// Named connection from config (e.g. local)
    #[arg(short = 'c', long)]
    connection: Option<String>,

    /// Show debug panel at the bottom
    #[arg(long)]
    debug: bool,

    /// Start in a fresh sandbox: a temp dir replaces `~/.config/sqeel`
    /// and `~/.local/share/sqeel`, pre-seeded with a `sample` SQLite
    /// connection backed by a temp DB and a sample SQL buffer that
    /// creates a `users` table. Useful for trying sqeel without
    /// touching your real config or for end-to-end tests.
    #[arg(long)]
    sandbox: bool,

    /// Combine with `--sandbox` to skip the seeded sample connection
    /// and SQL buffer. The sandbox dirs exist but are empty, so the
    /// launch matches a fresh install — connection picker is empty
    /// and the add-connection form opens automatically.
    #[arg(long, requires = "sandbox")]
    empty: bool,
}

fn main() -> anyhow::Result<()> {
    let mut args = Args::parse();
    let sandbox_root: Option<PathBuf> = if args.sandbox {
        let root = bootstrap_sandbox(args.empty)?;
        // Auto-select the seeded connection so first launch lands on
        // a working pane instead of the empty connection picker.
        // `--empty` skips the seed, so leave `connection` unset and
        // let the first-run UX (open add-connection form) kick in.
        if !args.empty && args.connection.is_none() && args.url.is_none() {
            args.connection = Some("sample".into());
        }
        Some(root)
    } else {
        None
    };
    let state = AppState::new();
    state.lock().unwrap().debug_mode = args.debug;

    let main_config = load_main_config().unwrap_or_default();
    let conns = load_connections().unwrap_or_default();
    {
        let mut s = state.lock().unwrap();
        s.apply_editor_config(&main_config.editor);
        s.set_available_connections(conns.clone());
        // First-run UX: with no connections on disk and none passed
        // on the CLI, drop the user straight into the add-connection
        // form so the launch isn't a blank TUI.
        if conns.is_empty() && args.url.is_none() && args.connection.is_none() {
            s.open_add_connection();
        }
    }

    let session = load_session_data();
    {
        let mut s = state.lock().unwrap();
        s.focus = session.focus;
        s.schema_search_query = session.schema_search.clone();
        let conn_for_results = args
            .connection
            .clone()
            .or_else(|| session.connection.clone());
        if let Some(name) = conn_for_results {
            let slug = sanitize_conn_slug(&name);
            s.result_tabs = session
                .result_tabs
                .iter()
                .filter_map(|r| {
                    let kind = if r.cancelled {
                        ResultsPane::Cancelled
                    } else if let Some(ref err) = r.error {
                        ResultsPane::Error(err.clone())
                    } else if let Some(ref filename) = r.filename {
                        ResultsPane::Results(load_result_for(&slug, filename).ok()?)
                    } else {
                        return None;
                    };
                    Some(ResultsTab {
                        query: r.query.clone(),
                        kind,
                        scroll: r.scroll,
                        col_scroll: r.col_scroll,
                        saved_filename: r.filename.clone(),
                        cursor: sqeel_core::state::ResultsCursor::default(),
                        selection: None,
                    })
                })
                .collect();
        }
        s.active_result_tab = session
            .active_result_tab
            .min(s.result_tabs.len().saturating_sub(1));
        if !s.result_tabs.is_empty() {
            s.editor_ratio = 0.5;
        }
    }
    let (url, conn_name) = if let Some(url) = args.url.clone() {
        let url_ref = url.clone();
        let name = args.connection.clone().or_else(|| {
            conns
                .iter()
                .find(|c| c.url == url_ref)
                .map(|c| c.name.clone())
        });
        (Some(url), name)
    } else {
        let name = args.connection.clone().or(session.connection.clone());
        let url = name
            .as_ref()
            .and_then(|n| conns.iter().find(|c| c.name == *n).map(|c| c.url.clone()));
        (url, name)
    };
    let session_schema_cursor = session.schema_cursor;
    let session_schema_cursor_path = session.schema_cursor_path;
    let session_schema_expanded_paths = session.schema_expanded_paths;
    let session_active_tab = session.active_tab;

    // Load scratch tabs from disk immediately — doesn't need the DB
    // handshake, so the user sees their last query as soon as the TUI
    // comes up even if the connection is slow or fails entirely.
    // Scratches are connection-agnostic so they load regardless of
    // whether a connection name was resolved.
    {
        let mut s = state.lock().unwrap();
        if let Some(name) = &conn_name {
            s.active_connection = Some(name.clone());
        }
        s.load_tabs();
        if session_active_tab < s.tabs.len() {
            s.switch_to_tab(session_active_tab);
        }
    }

    // Runtime for async setup (initial connect + reconnection watcher).
    // TuiProvider::run creates its own runtime; must not be called from inside one.
    let rt = tokio::runtime::Runtime::new()?;

    if let Some(url) = url {
        // Spawn — don't block. Slow DB handshakes must not freeze the TUI.
        {
            let mut s = state.lock().unwrap();
            s.schema_connecting = true;
            s.set_status(format!("Connecting to {url}…"));
        }
        let connect_state = state.clone();
        rt.spawn(async move {
            connect_and_spawn(
                &connect_state,
                &url,
                session_schema_cursor,
                session_schema_cursor_path,
                session_schema_expanded_paths,
                session_active_tab,
            )
            .await;
        });
    }

    let watcher_state = state.clone();
    rt.spawn(async move {
        let mut last_written_conn: Option<String> = None;
        let mut last_written_cursor: usize = 0;
        let mut last_written_cursor_path: Option<String> = None;
        let mut last_written_expanded_paths: Vec<String> = Vec::new();
        let mut last_written_focus = sqeel_core::state::Focus::default();
        let mut last_written_search: Option<String> = None;
        let mut last_written_tab_cursors: Vec<sqeel_core::config::TabCursor> = Vec::new();
        let mut last_written_active_tab: usize = 0;
        let mut last_written_result_tabs: Vec<sqeel_core::config::SavedResultRef> = Vec::new();
        let mut last_written_active_result_tab: usize = 0;
        let mut dirty = false;
        let mut pending_conn: Option<String> = None;
        let mut pending_cursor: usize = 0;
        let mut pending_cursor_path: Option<String> = None;
        let mut pending_expanded_paths: Vec<String> = Vec::new();
        let mut pending_focus = sqeel_core::state::Focus::default();
        let mut pending_search: Option<String> = None;
        let mut pending_tab_cursors: Vec<sqeel_core::config::TabCursor> = Vec::new();
        let mut pending_active_tab: usize = 0;
        let mut pending_result_tabs: Vec<sqeel_core::config::SavedResultRef> = Vec::new();
        let mut pending_active_result_tab: usize = 0;
        let mut last_write = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(2))
            .unwrap_or_else(std::time::Instant::now);

        loop {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let reconnect = watcher_state.lock().unwrap().pending_reconnect.take();
            if let Some(url) = reconnect {
                connect_and_spawn(&watcher_state, &url, 0, None, Vec::new(), 0).await;
            }

            let s = watcher_state.lock().unwrap();
            let conn = s.active_connection.clone();
            let cursor = s.schema_cursor;
            let cursor_path = s.schema_cursor_path_string();
            let expanded_paths = s.schema_expanded_paths();
            let focus = s.focus;
            let search = s.schema_search_query.clone();
            let loading = s.schema_loading;
            let tab_cursors: Vec<sqeel_core::config::TabCursor> = s
                .tab_cursor_snapshot()
                .into_iter()
                .map(|(name, row, col)| sqeel_core::config::TabCursor { name, row, col })
                .collect();
            let active_tab = s.active_tab;
            let result_tabs: Vec<sqeel_core::config::SavedResultRef> = s
                .result_tabs
                .iter()
                .filter_map(saved_ref_from_tab)
                .collect();
            let active_result_tab = s.active_result_tab;
            drop(s);

            // Skip while the schema is still loading: schema_nodes is partial,
            // so schema_expanded_paths() would write a truncated set and clobber
            // the user's saved expansion.
            if loading {
                continue;
            }

            if conn.is_some()
                && (conn != last_written_conn
                    || cursor != last_written_cursor
                    || cursor_path != last_written_cursor_path
                    || expanded_paths != last_written_expanded_paths
                    || focus != last_written_focus
                    || search != last_written_search
                    || tab_cursors != last_written_tab_cursors
                    || active_tab != last_written_active_tab
                    || result_tabs != last_written_result_tabs
                    || active_result_tab != last_written_active_result_tab)
            {
                pending_conn = conn;
                pending_cursor = cursor;
                pending_cursor_path = cursor_path;
                pending_expanded_paths = expanded_paths;
                pending_focus = focus;
                pending_search = search;
                pending_tab_cursors = tab_cursors;
                pending_active_tab = active_tab;
                pending_result_tabs = result_tabs;
                pending_active_result_tab = active_result_tab;
                dirty = true;
            }

            if dirty && last_write.elapsed() >= std::time::Duration::from_millis(1000) {
                if let Some(ref name) = pending_conn {
                    let _ = save_session(
                        name,
                        pending_cursor,
                        pending_cursor_path.clone(),
                        pending_expanded_paths.clone(),
                        pending_focus,
                        pending_search.clone(),
                        pending_tab_cursors.clone(),
                        pending_active_tab,
                        pending_result_tabs.clone(),
                        pending_active_result_tab,
                    );
                }
                last_written_conn = pending_conn.clone();
                last_written_cursor = pending_cursor;
                last_written_cursor_path = pending_cursor_path.clone();
                last_written_expanded_paths = pending_expanded_paths.clone();
                last_written_focus = pending_focus;
                last_written_search = pending_search.clone();
                last_written_tab_cursors = pending_tab_cursors.clone();
                last_written_active_tab = pending_active_tab;
                last_written_result_tabs = pending_result_tabs.clone();
                last_written_active_result_tab = pending_active_result_tab;
                dirty = false;
                last_write = std::time::Instant::now();
            }
        }
    });

    TuiProvider::run(state.clone())?;

    // Sandbox-cleanup prompt. The TUI has surrendered the terminal,
    // so a plain stderr prompt + stdin read is fine — the user is
    // back at their shell-style cursor.
    if let Some(root) = sandbox_root {
        eprintln!("\nsqeel sandbox: {}", root.display());
        if prompt_yes_no("Delete sandbox dir?") {
            cleanup_sandbox(&root);
        } else {
            eprintln!(
                "sqeel sandbox: kept at {} — `rm -rf` it when done.",
                root.display()
            );
        }
    }
    Ok(())
}

fn saved_ref_from_tab(t: &ResultsTab) -> Option<sqeel_core::config::SavedResultRef> {
    use sqeel_core::config::SavedResultRef;
    use sqeel_core::state::ResultsPane as P;
    match &t.kind {
        P::Results(_) => {
            let filename = t.saved_filename.clone()?;
            Some(SavedResultRef {
                filename: Some(filename),
                query: t.query.clone(),
                scroll: t.scroll,
                col_scroll: t.col_scroll,
                error: None,
                cancelled: false,
            })
        }
        P::Error(msg) => Some(SavedResultRef {
            filename: None,
            query: t.query.clone(),
            scroll: t.scroll,
            col_scroll: t.col_scroll,
            error: Some(msg.clone()),
            cancelled: false,
        }),
        P::Cancelled => Some(SavedResultRef {
            filename: None,
            query: t.query.clone(),
            scroll: t.scroll,
            col_scroll: t.col_scroll,
            error: None,
            cancelled: true,
        }),
        // NonQuery results aren't worth restoring across launches —
        // they describe a one-shot statement (CREATE TABLE / INSERT /
        // …) whose summary loses meaning once the user reopens the
        // app and the rows_affected count no longer matches reality.
        P::Loading | P::Empty | P::NonQuery { .. } => None,
    }
}

async fn connect_and_spawn(
    state: &Arc<std::sync::Mutex<AppState>>,
    url: &str,
    session_schema_cursor: usize,
    session_schema_cursor_path: Option<String>,
    session_schema_expanded_paths: Vec<String>,
    session_active_tab: usize,
) {
    {
        let mut s = state.lock().unwrap();
        s.schema_connecting = true;
    }
    match DbConnection::connect(url).await {
        Ok(conn) => {
            {
                let mut s = state.lock().unwrap();
                let conn_name = s
                    .available_connections
                    .iter()
                    .find(|c| c.url == url)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| conn.url.clone());
                // Wipe any previous failure now that we're back online.
                s.schema_connecting = false;
                s.schema_connect_error = None;
                s.schema_connect_url = None;
                s.show_connect_error_popup = false;
                s.set_status(format!("Connected: {conn_name}"));
                // Tabs are connection-agnostic now — main() already
                // populated them on startup. Don't reload here; that
                // would clobber any edits the user made while waiting
                // for the handshake.
                let already_loaded = !s.tabs.is_empty();
                s.active_connection = Some(conn_name.clone());
                s.active_dialect = sqeel_core::highlight::Dialect::from_url(url);
                // Generate a sqls config from the active URL so the
                // LSP can resolve schema + emit useful diagnostics.
                // Main loop picks up `pending_sqls_config` and restarts
                // the LSP with `--config=<path>`.
                if let Ok(cfg) = sqeel_core::lsp::write_sqls_config(url) {
                    s.pending_sqls_config = Some(cfg);
                }
                if !already_loaded {
                    s.load_tabs();
                    if session_active_tab < s.tabs.len() {
                        s.switch_to_tab(session_active_tab);
                    }
                }
                // Mark loading so the session watcher won't persist the
                // empty schema_expanded_paths before the loader has had a
                // chance to restore them from the session file.
                s.schema_loading = true;
            }
            spawn_executor(
                state.clone(),
                conn,
                session_schema_cursor,
                session_schema_cursor_path,
                session_schema_expanded_paths,
            );
        }
        Err(e) => {
            let msg = format!("{e}");
            let mut s = state.lock().unwrap();
            // Cancel pending "connecting"/"loading" state and surface
            // the failure in the sidebar so the user isn't stuck
            // staring at a perpetual "Connecting…" placeholder. The
            // error stays in the sidebar — no result-tab push, since
            // the user expects the failure to live next to the thing
            // that failed (the schema explorer), not in the results
            // pane.
            s.schema_connecting = false;
            s.schema_loading = false;
            s.schema_connect_error = Some(msg);
            s.schema_connect_url = Some(url.to_string());
        }
    }
}

fn spawn_executor(
    state: Arc<std::sync::Mutex<AppState>>,
    conn: DbConnection,
    session_schema_cursor: usize,
    session_schema_cursor_path: Option<String>,
    session_schema_expanded_paths: Vec<String>,
) {
    let conn = Arc::new(conn);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<QueryRequest>(8);
    state.lock().unwrap().query_tx = Some(tx);

    let conn_slug = {
        let s = state.lock().unwrap();
        let conn_name = s.active_connection.as_deref().unwrap_or("default");
        sanitize_conn_slug(conn_name)
    };

    // ── Lazy schema loader ───────────────────────────────────────────────────
    // Bounded-concurrency worker that handles one SchemaLoadRequest at a time:
    // Databases (initial) → Tables(db) when a db is expanded → Columns(db,table)
    // when a table is expanded. Nothing is fetched unless the user opens that
    // level of the sidebar.
    let (load_tx, load_rx) = tokio::sync::mpsc::unbounded_channel::<SchemaLoadRequest>();
    {
        let mut s = state.lock().unwrap();
        s.schema_load_tx = Some(load_tx.clone());
        // Kick off the initial db list; toggles after that drive further loads.
        s.request_schema_load(SchemaLoadRequest::Databases);

        // If the session restored expanded db/table nodes from cache, fire
        // lazy loads for them so the user sees fresh data for whatever was
        // open last time. Collect first so we don't hold the lock across sends.
        let pending: Vec<SchemaLoadRequest> = collect_expanded_load_requests(&s.schema_nodes);
        for req in pending {
            s.request_schema_load(req);
        }
    }

    let schema_conn = conn.clone();
    let schema_state = state.clone();
    tokio::spawn(schema_loader_task(
        schema_conn,
        schema_state,
        load_rx,
        load_tx,
        session_schema_cursor,
        session_schema_cursor_path,
        session_schema_expanded_paths,
    ));

    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            // Clear any stale cancel signal left over from a previous
            // request (e.g. user pressed Ctrl-C while idle) so the new
            // query doesn't abort before it starts.
            let cancel = state.lock().unwrap().cancel_control.clone();
            cancel.reset();
            match req {
                QueryRequest::Single(query, tab_idx) => {
                    let cleanup_slug = conn_slug.clone();
                    tokio::spawn(async move {
                        evict_old_results(&cleanup_slug);
                    });
                    let outcome = tokio::select! {
                        r = conn.execute(&query) => Some(r),
                        _ = cancel.cancelled() => None,
                    };
                    let mut s = state.lock().unwrap();
                    s.batch_in_progress = false;
                    match outcome {
                        Some(Ok(sqeel_core::db::ExecOutcome::Rows(mut r))) => {
                            let filename = s.persist_result(&query, &r);
                            s.push_history(&query);
                            r.compute_col_widths();
                            s.finish_result_tab(tab_idx, ResultsPane::Results(r));
                            if let Some(tab) = s.result_tabs.get_mut(tab_idx) {
                                tab.saved_filename = filename;
                            }
                            if let Some(effect) = parse_ddl(&query) {
                                s.invalidate_for_ddl(&effect);
                            }
                        }
                        Some(Ok(sqeel_core::db::ExecOutcome::NonQuery {
                            verb,
                            rows_affected,
                        })) => {
                            s.push_history(&query);
                            s.finish_result_tab(
                                tab_idx,
                                ResultsPane::NonQuery {
                                    verb,
                                    rows_affected,
                                },
                            );
                            if let Some(effect) = parse_ddl(&query) {
                                s.invalidate_for_ddl(&effect);
                            }
                        }
                        Some(Err(e)) => {
                            s.finish_result_tab(tab_idx, ResultsPane::Error(e.to_string()));
                        }
                        None => {
                            // Cancelled before the query completed.
                            s.finish_result_tab(tab_idx, ResultsPane::Cancelled);
                        }
                    }
                    s.results_dirty = true;
                }
                QueryRequest::Batch(queries, start_idx) => {
                    let cleanup_slug = conn_slug.clone();
                    tokio::spawn(async move {
                        evict_old_results(&cleanup_slug);
                    });
                    let (stop_on_error, batch_start) = {
                        let mut s = state.lock().unwrap();
                        (s.stop_on_error, s.start_batch())
                    };
                    let query_count = queries.len();
                    let mut cancelled = false;
                    for (i, query) in queries.into_iter().enumerate() {
                        if cancel.is_cancelled() {
                            cancelled = true;
                            break;
                        }
                        let tab_idx = start_idx + i;
                        let outcome = tokio::select! {
                            r = conn.execute(&query) => Some(r),
                            _ = cancel.cancelled() => None,
                        };
                        let (is_err, stop) = {
                            let mut s = state.lock().unwrap();
                            let (is_err, stop) = match outcome {
                                Some(Ok(sqeel_core::db::ExecOutcome::Rows(mut r))) => {
                                    let filename = s.persist_result(&query, &r);
                                    s.push_history(&query);
                                    r.compute_col_widths();
                                    s.finish_result_tab(tab_idx, ResultsPane::Results(r));
                                    if let Some(tab) = s.result_tabs.get_mut(tab_idx) {
                                        tab.saved_filename = filename;
                                    }
                                    if let Some(effect) = parse_ddl(&query) {
                                        s.invalidate_for_ddl(&effect);
                                    }
                                    (false, false)
                                }
                                Some(Ok(sqeel_core::db::ExecOutcome::NonQuery {
                                    verb,
                                    rows_affected,
                                })) => {
                                    s.push_history(&query);
                                    s.finish_result_tab(
                                        tab_idx,
                                        ResultsPane::NonQuery {
                                            verb,
                                            rows_affected,
                                        },
                                    );
                                    if let Some(effect) = parse_ddl(&query) {
                                        s.invalidate_for_ddl(&effect);
                                    }
                                    (false, false)
                                }
                                Some(Err(e)) => {
                                    s.finish_result_tab(tab_idx, ResultsPane::Error(e.to_string()));
                                    (true, stop_on_error)
                                }
                                None => {
                                    // User cancelled this query — mark it and
                                    // break out so the rest of the batch doesn't
                                    // run.
                                    s.finish_result_tab(tab_idx, ResultsPane::Cancelled);
                                    cancelled = true;
                                    (false, true)
                                }
                            };
                            s.results_dirty = true;
                            (is_err, stop)
                        };
                        let _ = is_err;
                        if stop {
                            // Mark remaining loading tabs as cancelled so the
                            // UI stops showing them as pending.
                            let mut s = state.lock().unwrap();
                            for j in (i + 1)..query_count {
                                let remaining_idx = start_idx + j;
                                s.finish_result_tab(remaining_idx, ResultsPane::Cancelled);
                            }
                            s.results_dirty = true;
                            break;
                        }
                    }
                    let mut s = state.lock().unwrap();
                    s.end_batch(batch_start);
                    let _ = cancelled;
                }
            }
            // Reset again so a cancel-late (fired between breakout and
            // here) doesn't leak into the next request.
            cancel.reset();
        }
    });
}

/// Retry an async DB call once after a short delay on failure. Covers transient
/// network blips and connection-pool flakes without masking real schema errors.
async fn retry_once<T, Fut, F>(mut f: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    match f().await {
        Ok(v) => Ok(v),
        Err(_) => {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            f().await
        }
    }
}

/// Walk the tree and emit a load request for every expanded node whose
/// children haven't been fetched this session. Called at startup so cached
/// nodes the user had open last time refresh automatically.
fn collect_expanded_load_requests(nodes: &[SchemaNode]) -> Vec<SchemaLoadRequest> {
    let mut out = Vec::new();
    for db in nodes {
        if let SchemaNode::Database {
            name: db_name,
            expanded: true,
            tables,
            tables_loaded_at: None,
            ..
        } = db
        {
            out.push(SchemaLoadRequest::Tables {
                db: db_name.clone(),
            });
            // Tables aren't loaded yet, so we can't queue Columns requests
            // here — they fire when set_db_tables completes and the user's
            // saved expansion is re-applied.
            let _ = tables;
            continue;
        }
        if let SchemaNode::Database {
            name: db_name,
            tables,
            ..
        } = db
        {
            for table in tables {
                if let SchemaNode::Table {
                    name: tname,
                    expanded: true,
                    columns_loaded_at: None,
                    ..
                } = table
                {
                    out.push(SchemaLoadRequest::Columns {
                        db: db_name.clone(),
                        table: tname.clone(),
                    });
                }
            }
        }
    }
    out
}

/// Long-lived task that services lazy-load requests from the sidebar. Runs
/// each request inside a Semaphore-gated spawn so opening many nodes in a
/// row doesn't hammer the connection pool.
#[allow(clippy::too_many_arguments)]
async fn schema_loader_task(
    conn: Arc<DbConnection>,
    state: Arc<std::sync::Mutex<AppState>>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<SchemaLoadRequest>,
    tx: tokio::sync::mpsc::UnboundedSender<SchemaLoadRequest>,
    session_schema_cursor: usize,
    session_schema_cursor_path: Option<String>,
    session_schema_expanded_paths: Vec<String>,
) {
    const LOAD_CONCURRENCY: usize = 8;
    let sem = Arc::new(tokio::sync::Semaphore::new(LOAD_CONCURRENCY));
    // On the first Databases response we also re-apply the user's saved
    // expansion (if any) so lazy fetches fire for nodes they had open.
    let mut databases_loaded = false;

    while let Some(req) = rx.recv().await {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let conn = conn.clone();
        let state = state.clone();
        let req_tx = tx.clone();
        let apply_saved_expansion =
            !databases_loaded && matches!(req, SchemaLoadRequest::Databases);
        if matches!(req, SchemaLoadRequest::Databases) {
            databases_loaded = true;
        }
        let session_expanded = session_schema_expanded_paths.clone();
        let session_cursor_path = session_schema_cursor_path.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let finish_req = req.clone();
            match req {
                SchemaLoadRequest::Databases => {
                    match retry_once(|| conn.load_schema_databases()).await {
                        Ok(db_shells) => {
                            let names: Vec<String> = db_shells
                                .iter()
                                .filter_map(|n| match n {
                                    SchemaNode::Database { name, .. } => Some(name.clone()),
                                    _ => None,
                                })
                                .collect();
                            {
                                let mut s = state.lock().unwrap();
                                s.merge_db_list(&names);
                                if apply_saved_expansion {
                                    s.restore_schema_expanded_paths(&session_expanded);
                                    let restored = session_cursor_path
                                        .as_deref()
                                        .map(|p| s.restore_schema_cursor_by_path(p))
                                        .unwrap_or(false);
                                    if !restored {
                                        s.rebuild_schema_cache_if_dirty();
                                        let max = s.visible_schema_items().len().saturating_sub(1);
                                        s.schema_cursor = session_schema_cursor.min(max);
                                    }
                                    // Queue follow-up Tables requests for any
                                    // db that's currently expanded.
                                    let follow_ups =
                                        collect_expanded_load_requests(&s.schema_nodes);
                                    for f in follow_ups {
                                        let _ = req_tx.send(f);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            state
                                .lock()
                                .unwrap()
                                .set_status(format!("Schema load failed: {e}"));
                        }
                    }
                }
                SchemaLoadRequest::Tables { db } => {
                    match retry_once(|| conn.list_tables(&db)).await {
                        Ok(table_names) => {
                            // set_db_tables may reuse existing Table nodes; we
                            // then need to queue Columns for any of those that
                            // the user already had expanded.
                            let follow_ups: Vec<SchemaLoadRequest> = {
                                let mut s = state.lock().unwrap();
                                s.set_db_tables(&db, &table_names);
                                collect_expanded_load_requests(&s.schema_nodes)
                                    .into_iter()
                                    .filter(|r| match r {
                                        SchemaLoadRequest::Columns { db: rd, .. } => rd == &db,
                                        _ => false,
                                    })
                                    .collect()
                            };
                            for f in follow_ups {
                                let _ = req_tx.send(f);
                            }
                        }
                        Err(e) => {
                            state
                                .lock()
                                .unwrap()
                                .set_status(format!("Tables load failed ({db}): {e}"));
                        }
                    }
                }
                SchemaLoadRequest::Columns { db, table } => {
                    let col_nodes = match retry_once(|| conn.list_columns(&db, &table)).await {
                        Ok(cols) => cols
                            .into_iter()
                            .map(|c| SchemaNode::Column {
                                name: c.name,
                                type_name: c.type_name,
                                nullable: c.nullable,
                                is_pk: c.is_pk,
                            })
                            .collect(),
                        Err(e) => {
                            state
                                .lock()
                                .unwrap()
                                .set_status(format!("Columns load failed ({db}.{table}): {e}"));
                            vec![]
                        }
                    };
                    state
                        .lock()
                        .unwrap()
                        .set_table_columns(&db, &table, col_nodes);
                }
            }
            state.lock().unwrap().finish_schema_load(&finish_req);
        });
    }
}
