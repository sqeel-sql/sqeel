use anyhow::Context;
use lsp_types::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

static ID: AtomicI64 = AtomicI64::new(1);

fn next_id() -> i64 {
    ID.fetch_add(1, Ordering::SeqCst)
}

/// Generate a `sqls` config file from a sqlx-style connection URL and
/// write it to a per-process temp path. Returns the path so the caller
/// can pass `--config=<path>` (or similar) when spawning sqls. The
/// config contains a single connection entry — sqls picks the first
/// one by default so no further wiring is needed.
pub fn write_sqls_config(url: &str) -> anyhow::Result<std::path::PathBuf> {
    let (driver, dsn) = sqls_driver_and_dsn(url)?;
    let yaml = format!(
        "lowercaseKeywords: false\nconnections:\n  - alias: sqeel\n    driver: {driver}\n    dataSourceName: \"{dsn}\"\n"
    );
    let dir = std::env::temp_dir();
    let path = dir.join(format!("sqeel-sqls-config-{}.yml", std::process::id()));
    std::fs::write(&path, yaml)?;
    Ok(path)
}

fn sqls_driver_and_dsn(url: &str) -> anyhow::Result<(&'static str, String)> {
    use anyhow::Context as _;
    if let Some(rest) = url
        .strip_prefix("mysql://")
        .or_else(|| url.strip_prefix("mariadb://"))
    {
        // sqls expects MySQL DSN form `user[:pass]@tcp(host[:port])/db`.
        let (userpass, after) = rest
            .split_once('@')
            .context("mysql url missing `user@host`")?;
        let (hostport, db_and_rest) = after.split_once('/').unwrap_or((after, ""));
        let db = db_and_rest.split('?').next().unwrap_or("");
        Ok(("mysql", format!("{userpass}@tcp({hostport})/{db}")))
    } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        // sqls `postgresql` driver accepts libpq URIs directly.
        Ok(("postgresql", url.to_string()))
    } else if url.starts_with("sqlite:") {
        let path = url
            .strip_prefix("sqlite://")
            .or_else(|| url.strip_prefix("sqlite:"))
            .unwrap_or("");
        Ok(("sqlite3", path.to_string()))
    } else {
        anyhow::bail!("unsupported URL scheme for sqls config: {url}")
    }
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub line: u32,
    pub col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub message: String,
    pub severity: DiagnosticSeverity,
}

#[derive(Debug)]
pub enum LspEvent {
    Diagnostics(Vec<Diagnostic>),
    /// (request_id, items) — caller drops if id doesn't match the latest request
    Completion(i64, Vec<String>),
}

#[derive(Serialize)]
struct RpcRequest {
    jsonrpc: &'static str,
    id: i64,
    method: &'static str,
    params: Value,
}

#[derive(Serialize)]
struct RpcNotification {
    jsonrpc: &'static str,
    method: &'static str,
    params: Value,
}

#[derive(Deserialize, Debug)]
struct RpcMessage {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<Value>,
    method: Option<String>,
    params: Option<Value>,
    result: Option<Value>,
    #[allow(dead_code)]
    error: Option<Value>,
}

pub struct LspClient {
    write_tx: mpsc::Sender<String>,
    _child: Child,
    pub events: mpsc::Receiver<LspEvent>,
}

impl LspClient {
    pub async fn start(
        binary: &str,
        root_uri: Option<Uri>,
        args: &[String],
    ) -> anyhow::Result<Self> {
        let mut child = Command::new(binary)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // SIGKILL the server if we drop the handle without explicit
            // shutdown — prevents orphaned sqls processes on crash / kill.
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to spawn LSP: {binary}"))?;

        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;

        let (event_tx, event_rx) = mpsc::channel(64);
        let (write_tx, write_rx) = mpsc::channel::<String>(64);

        tokio::spawn(read_loop(BufReader::new(stdout), event_tx));
        tokio::spawn(write_loop(stdin, write_rx));

        let mut client = Self {
            write_tx,
            _child: child,
            events: event_rx,
        };

        client.initialize(root_uri).await?;
        Ok(client)
    }

    async fn initialize(&mut self, root_uri: Option<Uri>) -> anyhow::Result<()> {
        let params = InitializeParams {
            #[allow(deprecated)]
            root_uri,
            capabilities: ClientCapabilities {
                text_document: Some(TextDocumentClientCapabilities {
                    completion: Some(CompletionClientCapabilities {
                        completion_item: Some(CompletionItemCapability {
                            snippet_support: Some(false),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        self.request("initialize", serde_json::to_value(params)?)
            .await?;
        self.notify("initialized", json!({})).await?;
        Ok(())
    }

    pub async fn open_document(&mut self, uri: Uri, text: &str) -> anyhow::Result<()> {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "sql",
                    "version": 1,
                    "text": text
                }
            }),
        )
        .await
    }

    pub async fn change_document(
        &mut self,
        uri: Uri,
        version: i32,
        text: &str,
    ) -> anyhow::Result<()> {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }]
            }),
        )
        .await
    }

    /// Send a completion request and return the request ID.
    /// Callers should discard responses whose ID doesn't match the most recently returned ID.
    pub async fn request_completion(
        &mut self,
        uri: Uri,
        line: u32,
        col: u32,
    ) -> anyhow::Result<i64> {
        self.request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": col }
            }),
        )
        .await
    }

    /// Graceful LSP shutdown: `shutdown` request + `exit` notification.
    /// Follows the shutdown with a short wait so well-behaved servers can
    /// exit on their own before `kill_on_drop` fires.
    pub async fn shutdown(&mut self) {
        let _ = self.request("shutdown", Value::Null).await;
        let _ = self.notify("exit", Value::Null).await;
    }

    async fn request(&mut self, method: &'static str, params: Value) -> anyhow::Result<i64> {
        let id = next_id();
        let msg = RpcRequest {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        self.send(&serde_json::to_string(&msg)?).await?;
        Ok(id)
    }

    async fn notify(&mut self, method: &'static str, params: Value) -> anyhow::Result<()> {
        let msg = RpcNotification {
            jsonrpc: "2.0",
            method,
            params,
        };
        self.send(&serde_json::to_string(&msg)?).await
    }

    async fn send(&mut self, body: &str) -> anyhow::Result<()> {
        let msg = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        self.write_tx.send(msg).await?;
        Ok(())
    }
}

async fn write_loop(stdin: ChildStdin, mut rx: mpsc::Receiver<String>) {
    let mut writer = BufWriter::new(stdin);
    while let Some(msg) = rx.recv().await {
        if writer.write_all(msg.as_bytes()).await.is_err() {
            break;
        }
        if writer.flush().await.is_err() {
            break;
        }
    }
}

async fn read_loop(mut reader: BufReader<ChildStdout>, tx: mpsc::Sender<LspEvent>) {
    loop {
        let mut header = String::new();
        let mut content_length: usize = 0;

        loop {
            header.clear();
            if reader.read_line(&mut header).await.unwrap_or(0) == 0 {
                return;
            }
            let h = header.trim();
            if h.is_empty() {
                break;
            }
            if let Some(val) = h.strip_prefix("Content-Length: ") {
                content_length = val.parse().unwrap_or(0);
            }
        }

        if content_length == 0 {
            continue;
        }

        let mut body = vec![0u8; content_length];
        if reader.read_exact(&mut body).await.is_err() {
            return;
        }

        let Ok(msg) = serde_json::from_slice::<RpcMessage>(&body) else {
            continue;
        };

        if let Some(method) = &msg.method
            && method.as_str() == "textDocument/publishDiagnostics"
            && let Some(params) = msg.params
            && let Ok(p) = serde_json::from_value::<PublishDiagnosticsParams>(params)
        {
            let diags = p
                .diagnostics
                .into_iter()
                .map(|d| Diagnostic {
                    line: d.range.start.line,
                    col: d.range.start.character,
                    end_line: d.range.end.line,
                    end_col: d.range.end.character,
                    message: d.message,
                    severity: d.severity.unwrap_or(DiagnosticSeverity::ERROR),
                })
                .collect();
            let _ = tx.send(LspEvent::Diagnostics(diags)).await;
        }

        if let Some(id_val) = msg.id
            && let Some(result) = msg.result
            && let Ok(list) = serde_json::from_value::<CompletionResponse>(result)
        {
            let id = match &id_val {
                Value::Number(n) => n.as_i64().unwrap_or(0),
                _ => 0,
            };
            let items: Vec<String> = match list {
                CompletionResponse::Array(items) => items.into_iter().map(|i| i.label).collect(),
                CompletionResponse::List(l) => l.items.into_iter().map(|i| i.label).collect(),
            };
            let _ = tx.send(LspEvent::Completion(id, items)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqls_driver_and_dsn_mysql() {
        let (driver, dsn) =
            super::sqls_driver_and_dsn("mysql://root:secret@localhost:3306/mydb").unwrap();
        assert_eq!(driver, "mysql");
        assert_eq!(dsn, "root:secret@tcp(localhost:3306)/mydb");
    }

    #[test]
    fn sqls_driver_and_dsn_mariadb_aliased_to_mysql() {
        let (driver, dsn) = super::sqls_driver_and_dsn("mariadb://u:p@db.host:3307/shop").unwrap();
        assert_eq!(driver, "mysql");
        assert_eq!(dsn, "u:p@tcp(db.host:3307)/shop");
    }

    #[test]
    fn sqls_driver_and_dsn_postgres_passthrough() {
        let (driver, dsn) = super::sqls_driver_and_dsn("postgres://u:p@h:5432/db").unwrap();
        assert_eq!(driver, "postgresql");
        assert_eq!(dsn, "postgres://u:p@h:5432/db");
    }

    #[test]
    fn sqls_driver_and_dsn_sqlite_strips_scheme() {
        let (driver, dsn) = super::sqls_driver_and_dsn("sqlite:///tmp/foo.db").unwrap();
        assert_eq!(driver, "sqlite3");
        assert_eq!(dsn, "/tmp/foo.db");
    }

    #[test]
    fn sqls_driver_and_dsn_rejects_unknown_scheme() {
        assert!(super::sqls_driver_and_dsn("other://x").is_err());
    }

    #[test]
    fn write_sqls_config_writes_file() {
        let path = write_sqls_config("mysql://u:p@host/db").unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("driver: mysql"));
        assert!(body.contains("u:p@tcp(host)/db"));
        let _ = std::fs::remove_file(&path);
    }
}
