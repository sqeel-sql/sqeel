use std::sync::Arc;

use clap::Parser;
use sqeel_core::{
    AppState, UiProvider,
    config::{load_connections, load_last_connection, save_last_connection},
    db::DbConnection,
    persistence::{load_schema_cache, save_schema_cache},
};
use sqeel_gui::GuiProvider;

#[derive(Parser)]
#[command(name = "sqeel-gui", about = "Fast vim-native SQL client (GUI)")]
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

    let conns = load_connections().unwrap_or_default();
    state
        .lock()
        .unwrap()
        .set_available_connections(conns.clone());

    let url = if let Some(url) = args.url {
        Some(url)
    } else if let Some(name) = args.connection {
        conns.into_iter().find(|c| c.name == name).map(|c| c.url)
    } else {
        load_last_connection()
    };

    // Build a tokio runtime for async work; iced owns the main thread and its own executor.
    let rt = tokio::runtime::Runtime::new()?;

    if let Some(url) = url {
        rt.block_on(connect_and_spawn(&state, &url));
    }

    // Reconnection watcher — lives inside the manual runtime.
    let watcher_state = state.clone();
    rt.spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let pending = watcher_state.lock().unwrap().pending_reconnect.take();
            if let Some(url) = pending {
                connect_and_spawn(&watcher_state, &url).await;
            }
        }
    });

    // iced creates its own executor on the main thread — must not be inside tokio::main.
    GuiProvider::run(state)?;
    Ok(())
}

async fn connect_and_spawn(state: &Arc<std::sync::Mutex<AppState>>, url: &str) {
    match DbConnection::connect(url).await {
        Ok(conn) => {
            {
                let mut s = state.lock().unwrap();
                s.active_connection = Some(conn.url.clone());
                s.set_status(format!("Connected: {}", conn.url));
            }
            let _ = save_last_connection(url);
            spawn_executor(state.clone(), conn);
        }
        Err(e) => {
            state
                .lock()
                .unwrap()
                .set_error(format!("Connection failed: {e}"));
        }
    }
}

fn spawn_executor(state: Arc<std::sync::Mutex<AppState>>, conn: DbConnection) {
    let conn = Arc::new(conn);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(8);
    state.lock().unwrap().query_tx = Some(tx);

    // Show cached schema immediately, then refresh in background
    if let Some(cached) = load_schema_cache(&conn.url) {
        state.lock().unwrap().set_schema_nodes(cached);
    }

    let schema_conn = conn.clone();
    let schema_state = state.clone();
    let schema_url = conn.url.clone();
    tokio::spawn(async move {
        match schema_conn.load_schema().await {
            Ok(nodes) => {
                let _ = save_schema_cache(&schema_url, &nodes);
                schema_state.lock().unwrap().set_schema_nodes(nodes);
            }
            Err(e) => schema_state
                .lock()
                .unwrap()
                .set_status(format!("Schema load failed: {e}")),
        }
    });

    tokio::spawn(async move {
        while let Some(query) = rx.recv().await {
            let result = conn.execute(&query).await;
            let mut s = state.lock().unwrap();
            match result {
                Ok(r) => {
                    s.persist_result(&query, &r);
                    s.push_history(&query);
                    s.set_results(r);
                }
                Err(e) => s.set_error(e.to_string()),
            }
        }
    });
}
