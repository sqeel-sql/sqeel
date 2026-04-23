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

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub line: u32,
    pub col: u32,
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
    pub async fn start(binary: &str, root_uri: Option<Uri>) -> anyhow::Result<Self> {
        let mut child = Command::new(binary)
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
