//! Direct LSP client core (P1.1, P1.2, P1.3).
//!
//! One child process per (language, root). JSON-RPC over stdio in both
//! directions. Every request carries a deadline: a late answer is dropped,
//! a missing answer returns a structured timeout. The client never blocks
//! the caller longer than its budget.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use lsp_types::notification::{DidOpenTextDocument, Notification, PublishDiagnostics};
use lsp_types::Uri;
use lsp_types::{
    ClientCapabilities, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, InitializeParams, InitializedParams, PublishDiagnosticsParams,
    TextDocumentClientCapabilities, TextDocumentItem, VersionedTextDocumentIdentifier,
    WorkspaceFolder,
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::util::{ErrorKind, ToolError};

/// Build a file:// URI from a path. lsp-types 0.97 parses from a string.
fn file_uri(path: &Path) -> Option<Uri> {
    let absolute = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let uri = format!("file://{}", absolute.display());
    Uri::from_str(&uri).ok()
}

/// A pending request: id plus the channel that waits for the answer.
struct Pending {
    reply: oneshot::Sender<Result<Value, String>>,
}

/// State shared between the writer task and the callers.
struct Inner {
    /// Held so the child stays alive; kill happens on drop.
    #[allow(dead_code)]
    child: Mutex<Child>,
    writer: mpsc::Sender<String>,
    pending: Mutex<HashMap<u64, Pending>>,
    next_id: AtomicU64,
    /// Diagnostics pushed by the server, per document URI.
    diagnostics: Mutex<HashMap<Uri, PublishDiagnosticsParams>>,
    /// Server capabilities from the initialize result (P2.4).
    capabilities: tokio::sync::RwLock<Value>,
    /// Set when the child exited. Later requests fail fast (P1.13).
    dead: std::sync::atomic::AtomicBool,
}

/// One live language server client.
pub struct LspClient {
    inner: Arc<Inner>,
    /// Documents already opened: didOpen must be sent exactly once.
    opened: Mutex<std::collections::HashSet<String>>,
}

impl LspClient {
    /// Spawn the server command and start the reader loop.
    pub async fn spawn(
        command: &[String],
        root: &Path,
        initialization_options: Option<Value>,
    ) -> Result<Self, ToolError> {
        let program = command.first().cloned().unwrap_or_default();
        if program.is_empty() {
            return Err(ToolError::new(
                ErrorKind::Invalid {
                    hint: "empty language server command".to_string(),
                },
                "lsp",
            ));
        }
        let mut child = Command::new(&program)
            .args(command.iter().skip(1))
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_e| {
                ToolError::new(
                    ErrorKind::Missing {
                        what: format!("language server {program}"),
                        next_step: "install it, or fix the command in the config".to_string(),
                    },
                    "lsp",
                )
            })?;

        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        // Drain child stderr: a full pipe buffer deadlocks the child.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut stderr = stderr;
                let mut buffer = [0u8; 4096];
                loop {
                    match stderr.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let text = String::from_utf8_lossy(&buffer[..n]);
                            for line in text.lines() {
                                crate::util::logging::debug(format!("lsp-stderr: {line}"));
                            }
                        }
                    }
                }
            });
        }

        let (writer_tx, writer_rx) = mpsc::channel::<String>(64);
        let inner = Arc::new(Inner {
            child: Mutex::new(child),
            writer: writer_tx,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            diagnostics: Mutex::new(HashMap::new()),
            capabilities: tokio::sync::RwLock::new(Value::Null),
            dead: std::sync::atomic::AtomicBool::new(false),
        });

        // Writer task: one line per JSON message.
        tokio::spawn(writer_task(stdin, writer_rx));
        // Reader task: parse frames, resolve pending, collect notifications.
        tokio::spawn(reader_task(inner.clone(), stdout));

        let client = Self {
            inner,
            opened: Mutex::new(std::collections::HashSet::new()),
        };
        crate::util::logging::debug("lsp: spawned, sending initialize");
        client.initialize(root, initialization_options).await?;
        crate::util::logging::debug("lsp: initialize answered");
        Ok(client)
    }

    /// The initialize/initialized handshake (P1.2).
    async fn initialize(
        &self,
        root: &Path,
        initialization_options: Option<Value>,
    ) -> Result<(), ToolError> {
        let root_uri = file_uri(root).ok_or_else(|| {
            ToolError::new(
                ErrorKind::Internal {
                    reason: format!("bad workspace path: {}", root.display()),
                },
                "lsp",
            )
        })?;
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            #[allow(deprecated)]
            root_uri: Some(root_uri.clone()),
            initialization_options,
            capabilities: ClientCapabilities {
                text_document: Some(TextDocumentClientCapabilities {
                    synchronization: None,
                    publish_diagnostics: Some(lsp_types::PublishDiagnosticsClientCapabilities {
                        related_information: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root_uri,
                name: root
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "workspace".to_string()),
            }]),
            ..Default::default()
        };
        let deadline_ms = 10_000u64;
        let reply = self
            .request(
                "initialize",
                serde_json::to_value(params).unwrap(),
                deadline_ms,
            )
            .await?;
        let capabilities = reply
            .as_object()
            .and_then(|o| o.get("capabilities"))
            .cloned()
            .unwrap_or(Value::Null);
        *self.inner.capabilities.write().await = capabilities;
        self.notify(
            "initialized",
            serde_json::to_value(InitializedParams {}).unwrap(),
        )
        .await;
        Ok(())
    }

    /// The raw server capabilities from the initialize reply (P2.4).
    pub async fn server_capabilities(&self) -> Value {
        self.inner.capabilities.read().await.clone()
    }

    /// Does this server support one LSP method? Answers from the stored
    /// capabilities; an absent flag reads as unsupported (P2.4).
    pub async fn supports(&self, method: &str) -> bool {
        let caps = self.server_capabilities().await;
        let capabilities = match caps.as_object() {
            Some(object) => object,
            None => return false,
        };
        let flag = match method {
            "textDocument/definition" => "definitionProvider",
            "textDocument/hover" => "hoverProvider",
            "textDocument/references" => "referencesProvider",
            "textDocument/documentSymbol" => "documentSymbolProvider",
            "textDocument/rename" => "renameProvider",
            "workspace/symbol" => "workspaceSymbolProvider",
            "workspace/didChangeWatchedFiles" => "didChangeWatchedFiles",
            _ => return true,
        };
        match capabilities.get(flag) {
            Some(Value::Bool(b)) => *b,
            // An object or a string (e.g. renameProvider: {prepareProvider:
            // true}) counts as support.
            Some(Value::Object(_)) | Some(Value::String(_)) => true,
            _ => false,
        }
    }

    /// True when the child exited (P1.13). Callers can check and degrade.
    pub fn is_dead(&self) -> bool {
        self.inner.dead.load(Ordering::Relaxed)
    }

    /// Send a request and wait with a deadline. A late reply is dropped.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        deadline_ms: u64,
    ) -> Result<Value, ToolError> {
        if self.is_dead() {
            return Err(ToolError::new(
                ErrorKind::Missing {
                    what: "language server (crashed)".to_string(),
                    next_step: "retry the call; the manager may restart it".to_string(),
                },
                "lsp",
            ));
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .await
            .insert(id, Pending { reply: tx });

        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let line = serde_json::to_string(&message).expect("serializable request");
        if self.inner.writer.send(line).await.is_err() {
            self.inner.pending.lock().await.remove(&id);
            return Err(ToolError::new(
                ErrorKind::Internal {
                    reason: "language server pipe closed".to_string(),
                },
                "lsp",
            ));
        }

        match tokio::time::timeout(std::time::Duration::from_millis(deadline_ms), rx).await {
            Ok(Ok(result)) => result.map_err(|error| {
                ToolError::new(
                    ErrorKind::Internal {
                        reason: format!("{method}: {error}"),
                    },
                    "lsp",
                )
            }),
            _ => {
                // Timeout or dropped sender: remove so a late reply drops.
                self.inner.pending.lock().await.remove(&id);
                Err(ToolError::new(
                    ErrorKind::Timeout {
                        layer: format!("lsp {method}"),
                        deadline_ms,
                    },
                    "lsp",
                ))
            }
        }
    }

    /// Fire a notification; no answer expected. Ordered: the write lands
    /// before any later request from the same caller.
    pub async fn notify(&self, method: &str, params: Value) {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let line = serde_json::to_string(&message).expect("serializable notification");
        let _ = self.inner.writer.send(line).await;
    }

    /// didOpen once per document; later calls are no-ops (P1.4).
    pub async fn did_open_once(&self, path: &Path, language_id: &str, text: &str) {
        let key = uri_string_of(path);
        if self.opened.lock().await.contains(&key) {
            return;
        }
        self.did_open(path, language_id, text, 1).await;
        self.opened.lock().await.insert(key);
    }

    /// didOpen a document so the server starts analyzing it (P1.4).
    pub async fn did_open(&self, path: &Path, language_id: &str, text: &str, version: i32) {
        let uri = match file_uri(path) {
            Some(uri) => uri,
            None => return,
        };
        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri,
                language_id: language_id.to_string(),
                version,
                text: text.to_string(),
            },
        };
        self.notify(
            DidOpenTextDocument::METHOD,
            serde_json::to_value(params).unwrap(),
        )
        .await;
    }

    /// didChange with full text sync (v1; incremental arrives later).
    #[allow(dead_code)]
    pub async fn did_change(&self, path: &Path, text: &str, version: i32) {
        let uri = match file_uri(path) {
            Some(uri) => uri,
            None => return,
        };
        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri, version },
            content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: text.to_string(),
            }],
        };
        self.notify(
            lsp_types::notification::DidChangeTextDocument::METHOD,
            serde_json::to_value(params).unwrap(),
        )
        .await;
    }

    /// didClose a document.
    #[allow(dead_code)]
    pub async fn did_close(&self, path: &Path) {
        let uri = match file_uri(path) {
            Some(uri) => uri,
            None => return,
        };
        let params = DidCloseTextDocumentParams {
            text_document: lsp_types::TextDocumentIdentifier { uri },
        };
        self.notify(
            lsp_types::notification::DidCloseTextDocument::METHOD,
            serde_json::to_value(params).unwrap(),
        )
        .await;
    }

    /// The latest diagnostics pushed for one document (P1.5).
    pub async fn diagnostics_for(&self, path: &Path) -> Option<PublishDiagnosticsParams> {
        let uri = file_uri(path)?;
        self.inner.diagnostics.lock().await.get(&uri).cloned()
    }

    /// Stop the server.
    #[allow(dead_code)]
    pub async fn shutdown(&self) {
        let _ = self.request("shutdown", json!({}), 2_000).await;
        self.notify("exit", json!({})).await;
    }
}

fn uri_string_of(path: &Path) -> String {
    let absolute = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    format!("file://{}", absolute.display())
}

/// Write JSON messages with LSP Content-Length framing to the child's stdin.
async fn writer_task(mut stdin: ChildStdin, mut rx: mpsc::Receiver<String>) {
    while let Some(line) = rx.recv().await {
        let header = format!("Content-Length: {}\r\n\r\n", line.len());
        if stdin.write_all(header.as_bytes()).await.is_err() {
            break;
        }
        if stdin.write_all(line.as_bytes()).await.is_err() {
            break;
        }
        if stdin.flush().await.is_err() {
            break;
        }
    }
}

/// Read length-prefixed or line-delimited JSON from the server.
async fn reader_task(inner: Arc<Inner>, stdout: tokio::process::ChildStdout) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                // Server exited: mark dead so callers fail fast (P1.13).
                inner.dead.store(true, std::sync::atomic::Ordering::Relaxed);
                crate::util::logging::info("lsp: server exited");
                break;
            }
            Ok(_) => {}
            Err(_) => {
                inner.dead.store(true, std::sync::atomic::Ordering::Relaxed);
                break;
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // LSP stdio framing: the first header line is already in `line`.
        let message: Value = if trimmed.starts_with("Content-Length") {
            let mut length = 0usize;
            if let Some(value) = trimmed.strip_prefix("Content-Length:") {
                length = value.trim().parse().unwrap_or(0);
            }
            // Consume any further headers until the blank line.
            loop {
                let mut header = String::new();
                match reader.read_line(&mut header).await {
                    Ok(0) => return,
                    Ok(_) => {}
                    Err(_) => return,
                }
                let header = header.trim();
                if header.is_empty() {
                    break;
                }
                if let Some(value) = header.strip_prefix("Content-Length:") {
                    length = value.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; length];
            if reader.read_exact(&mut body).await.is_err() {
                return;
            }
            match serde_json::from_slice(&body) {
                Ok(value) => value,
                Err(_) => continue,
            }
        } else {
            match serde_json::from_str(trimmed) {
                Ok(value) => value,
                Err(_) => continue,
            }
        };

        if let Some(id) = message.get("id").and_then(|v| v.as_u64()) {
            crate::util::logging::debug(format!("lsp: response id={id}"));
            // A response to one of our requests.
            if let Some(pending) = inner.pending.lock().await.remove(&id) {
                if let Some(error) = message.get("error") {
                    let _ = pending.reply.send(Err(error.to_string()));
                } else {
                    let _ = pending
                        .reply
                        .send(Ok(message.get("result").cloned().unwrap_or(Value::Null)));
                }
            }
        } else if let Some(method) = message.get("method").and_then(|m| m.as_str()) {
            if method == PublishDiagnostics::METHOD {
                if let Ok(params) =
                    serde_json::from_value::<PublishDiagnosticsParams>(message["params"].clone())
                {
                    inner
                        .diagnostics
                        .lock()
                        .await
                        .insert(params.uri.clone(), params);
                }
            }
            // Other server->client requests get a minimal safe answer.
            else if message.get("id").is_some() {
                let id = message.get("id").cloned();
                if let Some(id) = id {
                    let reply = json!({ "jsonrpc": "2.0", "id": id, "result": Value::Null });
                    let line = serde_json::to_string(&reply).unwrap();
                    let _ = inner.writer.send(line).await;
                }
            }
        }
    }
}
