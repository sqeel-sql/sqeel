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
    /// (request_id, markdown/plain text) — hover payload for the `K`
    /// binding. Caller drops if id doesn't match the latest request.
    Hover(i64, String),
    /// (request_id, target uri, 0-based line, 0-based char column) —
    /// resolved location for a `gd` goto-definition. Caller jumps
    /// the editor cursor to the target (or surfaces the uri in a
    /// toast if it's outside the active buffer).
    Definition(i64, String, u32, u32),
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
                    // sqls gates `textDocument/hover` on the client
                    // advertising the capability; without this the
                    // server silently drops every hover request.
                    hover: Some(HoverClientCapabilities {
                        content_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
                        ..Default::default()
                    }),
                    definition: Some(GotoCapability {
                        dynamic_registration: Some(false),
                        link_support: Some(true),
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

    /// Clonable write-side handle. Use it to fire-and-forget
    /// notifications (e.g. `didChange`) from a spawned task so the
    /// render loop doesn't block on the JSON serialization + channel
    /// send — which for multi-MB buffers would otherwise block per
    /// keystroke.
    pub fn writer(&self) -> LspWriter {
        LspWriter {
            tx: self.write_tx.clone(),
        }
    }
}

/// Cheap cloneable write-side of an [`LspClient`]. Doesn't hold any
/// `&mut` reference so the caller can move it into a spawned task and
/// fire notifications off the render loop. Sends drop silently if the
/// owning client has been dropped (e.g. LSP restart SIGKILLed the
/// previous child) — the caller doesn't need to observe the error.
#[derive(Clone)]
pub struct LspWriter {
    tx: mpsc::Sender<String>,
}

impl LspWriter {
    pub async fn change_document(&self, uri: Uri, version: i32, text: &str) -> anyhow::Result<()> {
        let params = json!({
            "textDocument": { "uri": uri, "version": version },
            "contentChanges": [{ "text": text }]
        });
        let msg = RpcNotification {
            jsonrpc: "2.0",
            method: "textDocument/didChange",
            params,
        };
        let body = serde_json::to_string(&msg)?;
        let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        self.tx.send(framed).await?;
        Ok(())
    }

    /// Fire-and-forget completion request. Returns the request ID
    /// synchronously (derived from the shared counter) so the caller
    /// can use it to dedupe late responses. The serialize + mpsc send
    /// run on a spawned task, so a slow writer channel doesn't stall
    /// the render loop.
    pub fn request_completion(&self, uri: Uri, line: u32, col: u32) -> i64 {
        let id = next_id();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let params = json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": col }
            });
            let msg = RpcRequest {
                jsonrpc: "2.0",
                id,
                method: "textDocument/completion",
                params,
            };
            if let Ok(body) = serde_json::to_string(&msg) {
                let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
                let _ = tx.send(framed).await;
            }
        });
        id
    }

    /// Fire-and-forget goto-definition request. Response surfaces as
    /// `LspEvent::Definition(id, uri, line, col)`; caller dedupes by id.
    pub fn request_definition(&self, uri: Uri, line: u32, col: u32) -> i64 {
        let id = next_id();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let params = json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": col }
            });
            let msg = RpcRequest {
                jsonrpc: "2.0",
                id,
                method: "textDocument/definition",
                params,
            };
            if let Ok(body) = serde_json::to_string(&msg) {
                let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
                let _ = tx.send(framed).await;
            }
        });
        id
    }

    /// Fire-and-forget hover request. Response surfaces as
    /// `LspEvent::Hover(id, text)`; caller dedupes by id.
    pub fn request_hover(&self, uri: Uri, line: u32, col: u32) -> i64 {
        let id = next_id();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let params = json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": col }
            });
            let msg = RpcRequest {
                jsonrpc: "2.0",
                id,
                method: "textDocument/hover",
                params,
            };
            if let Ok(body) = serde_json::to_string(&msg) {
                let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
                let _ = tx.send(framed).await;
            }
        });
        id
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
        {
            let id = match &id_val {
                Value::Number(n) => n.as_i64().unwrap_or(0),
                _ => 0,
            };
            let debug = std::env::var("SQEEL_DEBUG_HL_DUMP").ok();
            // Try the Hover shape first — it carries a `contents`
            // field whose type is distinctive enough that false
            // positives don't sneak through. Completion responses are
            // either a raw array or an object with `isIncomplete` +
            // `items`, neither of which matches. Probing Hover first
            // keeps hover replies from accidentally being routed
            // through the Completion branch on servers that return
            // odd shapes.
            // GotoDefinition can arrive as Location | [Location] |
            // [LocationLink] | null. Probe that shape before hover /
            // completion since its discriminant (`uri` field) is
            // distinct from either.
            if let Ok(def) = serde_json::from_value::<GotoDefinitionResponse>(result.clone()) {
                let loc_opt = match def {
                    GotoDefinitionResponse::Scalar(loc) => Some((loc.uri, loc.range.start)),
                    GotoDefinitionResponse::Array(mut locs) => {
                        locs.pop().map(|loc| (loc.uri, loc.range.start))
                    }
                    GotoDefinitionResponse::Link(mut links) => links
                        .pop()
                        .map(|l| (l.target_uri, l.target_selection_range.start)),
                };
                if let Some((uri, pos)) = loc_opt {
                    let _ = tx
                        .send(LspEvent::Definition(
                            id,
                            uri.to_string(),
                            pos.line,
                            pos.character,
                        ))
                        .await;
                }
            } else if let Ok(hover) = serde_json::from_value::<Hover>(result.clone()) {
                if let Some(path) = &debug {
                    use std::io::Write;
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                    {
                        let _ = writeln!(f, "### lsp hover response id={id}");
                    }
                }
                if let Some(text) = hover_text_from_contents(&hover.contents) {
                    let _ = tx.send(LspEvent::Hover(id, text)).await;
                }
            } else if let Ok(list) = serde_json::from_value::<CompletionResponse>(result) {
                let items: Vec<String> = match list {
                    CompletionResponse::Array(items) => {
                        items.into_iter().map(|i| i.label).collect()
                    }
                    CompletionResponse::List(l) => l.items.into_iter().map(|i| i.label).collect(),
                };
                let _ = tx.send(LspEvent::Completion(id, items)).await;
            } else if let Some(path) = &debug {
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let _ = writeln!(f, "### lsp unroutable response id={id}");
                }
            }
        }
    }
}

/// Flatten `HoverContents` into a plain display string. sqls returns
/// markdown — we strip the fences so the popup renders as raw text;
/// the TUI doesn't carry a markdown renderer. Returns `None` when the
/// server sent an empty response.
fn hover_text_from_contents(contents: &HoverContents) -> Option<String> {
    fn marked_string_text(m: &MarkedString) -> String {
        match m {
            MarkedString::String(s) => s.clone(),
            MarkedString::LanguageString(ls) => ls.value.clone(),
        }
    }
    let text = match contents {
        HoverContents::Scalar(s) => marked_string_text(s),
        HoverContents::Array(items) => items
            .iter()
            .map(marked_string_text)
            .collect::<Vec<_>>()
            .join("\n"),
        HoverContents::Markup(m) => m.value.clone(),
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
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

    #[test]
    fn hover_text_scalar_string_extracted() {
        let contents = HoverContents::Scalar(MarkedString::String("hello".into()));
        assert_eq!(
            super::hover_text_from_contents(&contents),
            Some("hello".into())
        );
    }

    #[test]
    fn hover_text_scalar_language_string_extracted() {
        let contents = HoverContents::Scalar(MarkedString::LanguageString(LanguageString {
            language: "sql".into(),
            value: "SELECT 1".into(),
        }));
        assert_eq!(
            super::hover_text_from_contents(&contents),
            Some("SELECT 1".into())
        );
    }

    #[test]
    fn hover_text_array_joins_with_newlines() {
        let contents = HoverContents::Array(vec![
            MarkedString::String("line1".into()),
            MarkedString::String("line2".into()),
        ]);
        assert_eq!(
            super::hover_text_from_contents(&contents),
            Some("line1\nline2".into())
        );
    }

    #[test]
    fn hover_text_markup_extracted() {
        let contents = HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: "## schema.table".into(),
        });
        assert_eq!(
            super::hover_text_from_contents(&contents),
            Some("## schema.table".into())
        );
    }

    #[test]
    fn hover_text_empty_returns_none() {
        let contents = HoverContents::Scalar(MarkedString::String("   ".into()));
        assert_eq!(super::hover_text_from_contents(&contents), None);
    }
}
