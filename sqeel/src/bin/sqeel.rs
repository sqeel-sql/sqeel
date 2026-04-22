use std::sync::Arc;

use clap::Parser;
use sqeel_core::{
    AppState, UiProvider,
    config::{load_connections, load_main_config, load_session_data, save_session},
    db::DbConnection,
    persistence::{
        evict_old_results, load_result_for, load_schema_cache, sanitize_conn_slug,
        save_schema_cache,
    },
    schema::SchemaNode,
    state::{QueryRequest, ResultsPane, ResultsTab},
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
        rt.block_on(connect_and_spawn(
            &state,
            &url,
            session_schema_cursor,
            session_schema_cursor_path,
            session_schema_expanded_paths,
            session_active_tab,
        ));
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

    // Show cached schema immediately with restored expansion + cursor, then refresh in background
    if let Some(cached) = load_schema_cache(&conn.url) {
        let mut s = state.lock().unwrap();
        s.set_schema_nodes(cached);
        s.restore_schema_expanded_paths(&session_schema_expanded_paths);
        let restored = session_schema_cursor_path
            .as_deref()
            .map(|p| s.restore_schema_cursor_by_path(p))
            .unwrap_or(false);
        if !restored {
            let max = s.visible_schema_items().len().saturating_sub(1);
            s.schema_cursor = session_schema_cursor.min(max);
        }
    }

    // Channel: table loader sends (db, table) pairs; column loader consumes them.
    let (col_tx, mut col_rx) = tokio::sync::mpsc::unbounded_channel::<(String, String)>();

    // ── Column loader task (separate thread) ─────────────────────────────────
    state.lock().unwrap().schema_loading = true;
    let col_conn = conn.clone();
    let col_state = state.clone();
    let col_schema_url = conn.url.clone();
    let col_session_path = session_schema_cursor_path.clone();
    let col_session_expanded = session_schema_expanded_paths.clone();
    tokio::spawn(async move {
        while let Some((db_name, table_name)) = col_rx.recv().await {
            let col_nodes = match col_conn.list_columns(&db_name, &table_name).await {
                Ok(cols) => cols
                    .into_iter()
                    .map(|c| SchemaNode::Column {
                        name: c.name,
                        type_name: c.type_name,
                        nullable: c.nullable,
                        is_pk: c.is_pk,
                    })
                    .collect(),
                Err(_) => vec![],
            };
            col_state
                .lock()
                .unwrap()
                .set_table_columns(&db_name, &table_name, col_nodes);
        }
        // Channel drained — full schema available. Restore expansion + cursor, then save cache.
        let mut s = col_state.lock().unwrap();
        s.schema_loading = false;
        s.restore_schema_expanded_paths(&col_session_expanded);
        let restored = col_session_path
            .as_deref()
            .map(|p| s.restore_schema_cursor_by_path(p))
            .unwrap_or(false);
        if !restored {
            let max = s.visible_schema_items().len().saturating_sub(1);
            s.schema_cursor = session_schema_cursor.min(max);
        }
        let nodes = s.schema_nodes.clone();
        let cursor = s.schema_cursor;
        let cursor_path = s.schema_cursor_path_string();
        let expanded_paths = s.schema_expanded_paths();
        let focus = s.focus;
        let search_query = s.schema_search_query.clone();
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
        let _ = save_schema_cache(&col_schema_url, &nodes);
        if let Some(ref name) = s.active_connection.clone() {
            let _ = save_session(
                name,
                cursor,
                cursor_path,
                expanded_paths,
                focus,
                search_query,
                tab_cursors,
                active_tab,
                result_tabs,
                active_result_tab,
            );
        }
    });

    // ── Table loader task ────────────────────────────────────────────────────
    let schema_conn = conn.clone();
    let schema_state = state.clone();
    tokio::spawn(async move {
        // Step 1: database shells.
        let db_shells = match schema_conn.load_schema_databases().await {
            Ok(n) => n,
            Err(e) => {
                schema_state
                    .lock()
                    .unwrap()
                    .set_status(format!("Schema load failed: {e}"));
                return;
            }
        };

        let db_names: Vec<String> = db_shells
            .iter()
            .filter_map(|n| {
                if let SchemaNode::Database { name, .. } = n {
                    Some(name.clone())
                } else {
                    None
                }
            })
            .collect();

        schema_state.lock().unwrap().refresh_schema_nodes(db_shells);

        // Phase 1: first 100 tables for every database.
        let mut overflow: Vec<(String, Vec<String>)> = Vec::new();

        for db_name in &db_names {
            let table_names = match schema_conn.list_tables(db_name).await {
                Ok(t) => t,
                Err(e) => {
                    schema_state
                        .lock()
                        .unwrap()
                        .set_status(format!("Tables load failed ({db_name}): {e}"));
                    continue;
                }
            };

            let first = &table_names[..table_names.len().min(100)];
            let rest = &table_names[first.len()..];

            add_table_shells(&schema_state, &col_tx, db_name, first);

            if !rest.is_empty() {
                overflow.push((db_name.clone(), rest.to_vec()));
            }
        }

        // Phase 2: remaining tables for databases with >100 tables.
        for (db_name, remaining) in &overflow {
            for batch in remaining.chunks(100) {
                add_table_shells(&schema_state, &col_tx, db_name, batch);
            }
        }

        // Dropping col_tx closes the channel → column loader task finishes and
        // saves the cache once all columns are loaded.
        drop(col_tx);
    });

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

/// Add table shells to the state and enqueue each for column loading.
fn add_table_shells(
    state: &Arc<std::sync::Mutex<AppState>>,
    col_tx: &tokio::sync::mpsc::UnboundedSender<(String, String)>,
    db_name: &str,
    tables: &[String],
) {
    let shells: Vec<SchemaNode> = tables
        .iter()
        .map(|t| SchemaNode::Table {
            name: t.clone(),
            expanded: false,
            columns: vec![],
        })
        .collect();
    state.lock().unwrap().append_db_tables(db_name, shells);

    for table_name in tables {
        let _ = col_tx.send((db_name.to_string(), table_name.clone()));
    }
}
