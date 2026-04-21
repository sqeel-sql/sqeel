use anyhow::Context;
use lsp_types::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use std::process::Stdio;

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
    Completion(Vec<String>),
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
    stdin: ChildStdin,
    _child: Child,
    pub events: mpsc::Receiver<LspEvent>,
}

impl LspClient {
    pub async fn start(binary: &str, root_uri: Option<Uri>) -> anyhow::Result<Self> {
        let mut child = Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn LSP: {binary}"))?;

        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;

        let (tx, rx) = mpsc::channel(64);

        tokio::spawn(read_loop(BufReader::new(stdout), tx));

        let mut client = Self {
            stdin,
            _child: child,
            events: rx,
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

        self.request("initialize", serde_json::to_value(params)?).await?;
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

    pub async fn change_document(&mut self, uri: Uri, version: i32, text: &str) -> anyhow::Result<()> {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }]
            }),
        )
        .await
    }

    pub async fn request_completion(&mut self, uri: Uri, line: u32, col: u32) -> anyhow::Result<()> {
        self.request(
            "textDocument/completion",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": col }
            }),
        )
        .await?;
        Ok(())
    }

    async fn request(&mut self, method: &'static str, params: Value) -> anyhow::Result<i64> {
        let id = next_id();
        let msg = RpcRequest { jsonrpc: "2.0", id, method, params };
        self.send(&serde_json::to_string(&msg)?).await?;
        Ok(id)
    }

    async fn notify(&mut self, method: &'static str, params: Value) -> anyhow::Result<()> {
        let msg = RpcNotification { jsonrpc: "2.0", method, params };
        self.send(&serde_json::to_string(&msg)?).await
    }

    async fn send(&mut self, body: &str) -> anyhow::Result<()> {
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        self.stdin.write_all(header.as_bytes()).await?;
        self.stdin.write_all(body.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
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

        if let Some(method) = &msg.method {
            match method.as_str() {
                "textDocument/publishDiagnostics" => {
                    if let Some(params) = msg.params {
                        if let Ok(p) = serde_json::from_value::<PublishDiagnosticsParams>(params) {
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
                    }
                }
                _ => {}
            }
        }

        if msg.id.is_some() {
            if let Some(result) = msg.result {
                if let Ok(list) = serde_json::from_value::<CompletionResponse>(result) {
                    let items: Vec<String> = match list {
                        CompletionResponse::Array(items) => {
                            items.into_iter().map(|i| i.label).collect()
                        }
                        CompletionResponse::List(l) => {
                            l.items.into_iter().map(|i| i.label).collect()
                        }
                    };
                    if !items.is_empty() {
                        let _ = tx.send(LspEvent::Completion(items)).await;
                    }
                }
            }
        }
    }
}
