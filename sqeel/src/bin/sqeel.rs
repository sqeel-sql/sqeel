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
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let state = AppState::new();
    state.lock().unwrap().debug_mode = args.debug;

    let main_config = load_main_config().unwrap_or_default();
    let conns = load_connections().unwrap_or_default();
    {
        let mut s = state.lock().unwrap();
        s.apply_editor_config(&main_config.editor);
        s.set_available_connections(conns.clone());
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
    let url = if let Some(url) = args.url {
        Some(url)
    } else {
        let name = args.connection.or(session.connection);
        name.and_then(|n| conns.iter().find(|c| c.name == n).map(|c| c.url.clone()))
    };
    let session_schema_cursor = session.schema_cursor;
    let session_schema_cursor_path = session.schema_cursor_path;
    let session_schema_expanded_paths = session.schema_expanded_paths;
    let session_active_tab = session.active_tab;

    // Runtime for async setup (initial connect + reconnection watcher).
    // TuiProvider::run creates its own runtime; must not be called from inside one.
    let rt = tokio::runtime::Runtime::new()?;

    if let Some(url) = url {
        // Spawn — don't block. Slow DB handshakes must not freeze the TUI.
        state
            .lock()
            .unwrap()
            .set_status(format!("Connecting to {url}…"));
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
        P::Loading | P::Empty => None,
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
                s.active_connection = Some(conn_name.clone());
                s.set_status(format!("Connected: {conn_name}"));
                let slug = sanitize_conn_slug(&conn_name);
                s.load_tabs_for_connection(&slug);
                if session_active_tab < s.tabs.len() {
                    s.switch_to_tab(session_active_tab);
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
            state
                .lock()
                .unwrap()
                .set_error(format!("Connection failed: {e}"));
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
            match req {
                QueryRequest::Single(query, tab_idx) => {
                    // Run old-results cleanup concurrently with query execution
                    let cleanup_slug = conn_slug.clone();
                    tokio::spawn(async move {
                        evict_old_results(&cleanup_slug);
                    });
                    let result = conn.execute(&query).await;
                    let mut s = state.lock().unwrap();
                    s.batch_in_progress = false;
                    match result {
                        Ok(mut r) => {
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
                        Err(e) => {
                            s.finish_result_tab(tab_idx, ResultsPane::Error(e.to_string()));
                        }
                    }
                    s.results_dirty = true;
                }
                QueryRequest::Batch(queries, start_idx) => {
                    // Run old-results cleanup concurrently with batch execution
                    let cleanup_slug = conn_slug.clone();
                    tokio::spawn(async move {
                        evict_old_results(&cleanup_slug);
                    });
                    let (stop_on_error, batch_start) = {
                        let mut s = state.lock().unwrap();
                        (s.stop_on_error, s.start_batch())
                    };
                    let query_count = queries.len();
                    for (i, query) in queries.into_iter().enumerate() {
                        let tab_idx = start_idx + i;
                        let result = conn.execute(&query).await;
                        let is_err = result.is_err();
                        let stop = {
                            let mut s = state.lock().unwrap();
                            match result {
                                Ok(mut r) => {
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
                                Err(e) => {
                                    s.finish_result_tab(tab_idx, ResultsPane::Error(e.to_string()));
                                }
                            }
                            s.results_dirty = true;
                            is_err && stop_on_error
                        };
                        if stop {
                            // Mark remaining loading tabs as cancelled
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
                }
            }
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
