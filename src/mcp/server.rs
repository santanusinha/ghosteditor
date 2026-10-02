//! The MCP stdio server (P0.2-P0.5).
//!
//! Hand-rolled: Content-Length framing over stdin/stdout, JSON-RPC
//! requests and notifications, id correlation, and a deadline watchdog
//! around every tool call. A tool that exceeds its budget returns a
//! structured timeout. It never blocks the loop.

use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::config::Config;
use crate::docstore::DocStore;
use crate::lsp::LspManager;
use crate::mcp::tools;
use crate::util::logging;
use crate::util::{ErrorKind, ToolError};

/// Shared state for tool handlers.
pub struct Server {
    pub config: Config,
    pub store: Mutex<DocStore>,
    pub lsp: LspManager,
}

impl Server {
    pub fn new(config: Config) -> Self {
        Self {
            lsp: LspManager::new(config.clone()),
            store: Mutex::new(DocStore::new()),
            config,
        }
    }

    /// Serve MCP over stdio until stdin ends or the client disconnects.
    pub async fn run(self) {
        let server = std::sync::Arc::new(self);
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let stdout = tokio::io::stdout();
        let mut writer = stdout;

        let mut ndjson = false;
        loop {
            let message = match read_message(&mut reader).await {
                Ok(Some((message, client_ndjson))) => {
                    if client_ndjson {
                        ndjson = true;
                    }
                    message
                }
                Ok(None) => break, // stdin closed: clean exit
                Err(error) => {
                    logging::info(format!("stdin read failed: {error}"));
                    break;
                }
            };

            let request: Value = match serde_json::from_slice(&message) {
                Ok(value) => value,
                Err(error) => {
                    logging::info(format!("bad json on stdin: {error}"));
                    continue;
                }
            };

            let id = request.get("id").cloned();
            let method = request
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();

            match dispatch(server.clone(), id, &method, request).await {
                Some(response) => {
                    if write_message(&mut writer, &response, ndjson).await.is_err() {
                        break;
                    }
                }
                None => continue,
            }
        }
        logging::info("stdin closed; exiting");
    }
}

/// Route one message. Returns a response to send, or None for notifications.
async fn dispatch(
    server: std::sync::Arc<Server>,
    id: Option<Value>,
    method: &str,
    request: Value,
) -> Option<Value> {
    match method {
        "initialize" => Some(handle_initialize(&server, id)),
        "notifications/initialized" => None,
        "ping" => Some(ok_result(id, json!({}))),
        "tools/list" => Some(ok_result(id, json!({ "tools": tools::tool_list() }))),
        "tools/call" => Some(handle_tools_call(server, id, request).await),
        other => {
            logging::debug(format!("unknown method ignored: {other}"));
            id.map(|id| error_result(id, -32601, &format!("method not found: {other}")))
        }
    }
}

/// `initialize`: server info plus the deadline table (S8, P0.3).
fn handle_initialize(server: &Server, id: Option<Value>) -> Value {
    let mut info = json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": "ghostedit",
            "version": env!("CARGO_PKG_VERSION"),
        }
    });
    // The effective deadline table per language, so an agent can honour it.
    info["ghostedit"] = json!({
        "workspace": server.config.cwd.display().to_string(),
        "deadlines": server.config.deadline_table(),
    });
    ok_result(id, info)
}

/// `tools/call`: find the tool, run it inside its deadline (P0.4, P0.5).
async fn handle_tools_call(
    server: std::sync::Arc<Server>,
    id: Option<Value>,
    request: Value,
) -> Value {
    let id = id.unwrap_or(Value::Null);

    let name = request
        .pointer("/params/name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let arguments = request
        .pointer("/params/arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    if let Some(tool) = tools::lookup(&name) {
        // The budget for this call. Server-set; agent arguments may only
        // lower it, never raise it (decision 4.5.2).
        let deadline_ms = tool.deadline_ms(&server.config, &arguments);
        let tool_name = name.clone();
        let server = server.clone();
        let args = arguments.clone();

        let work = async move { (tool.run)(server, args).await };
        match tokio::time::timeout(Duration::from_millis(deadline_ms), work).await {
            Ok(Ok(value)) => ok_result(Some(id), call_content(value)),
            Ok(Err(error)) => {
                let payload = error.to_payload();
                // MCP tool errors are results with isError, not protocol
                // errors: the call completed, with a negative outcome.
                ok_result(
                    Some(id),
                    json!({
                        "content": [
                            { "type": "text", "text": error.to_string() }
                        ],
                        "isError": true,
                        "structuredContent": payload,
                    }),
                )
            }
            Err(_) => {
                let tool_error = ToolError::new(
                    ErrorKind::Timeout {
                        layer: format!("tool {tool_name}"),
                        deadline_ms,
                    },
                    tool_name,
                );
                let payload = tool_error.to_payload();
                ok_result(
                    Some(id),
                    json!({
                        "content": [
                            { "type": "text", "text": tool_error.to_string() }
                        ],
                        "isError": true,
                        "structuredContent": payload,
                    }),
                )
            }
        }
    } else {
        error_result(id, -32602, &format!("unknown tool: {name}"))
    }
}

fn call_content(value: Value) -> Value {
    let text = value
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    json!({
        "content": [ { "type": "text", "text": text } ],
        "structuredContent": value,
    })
}

fn ok_result(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result })
}

fn error_result(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

/// Read one message. Accepts both MCP wire formats:
/// Content-Length framing and newline-delimited JSON (NDJSON). None means
/// EOF. The first byte decides: `{` starts an NDJSON line, `C`/`c` starts
/// a header.
async fn read_message<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<(Vec<u8>, bool)>, std::io::Error> {
    // Read the first byte of the line.
    let mut first = [0u8; 1];
    loop {
        match reader.read(&mut first).await? {
            0 => return Ok(None),
            _ if first[0] == b'\n' || first[0] == b'\r' => continue, // blank line
            _ => break,
        }
    }
    if first[0] == b'{' {
        // NDJSON: read the rest of the line.
        let mut body = vec![first[0]];
        let mut byte = [0u8; 1];
        loop {
            let n = reader.read(&mut byte).await?;
            if n == 0 {
                return Ok(Some((body, true)));
            }
            if byte[0] == b'\n' {
                break;
            }
            body.push(byte[0]);
        }
        return Ok(Some((body, true)));
    }

    // Content-Length framing: the first byte starts the header line.
    let mut header = vec![first[0]];
    let mut content_length: Option<usize> = None;
    // Finish the current header line.
    loop {
        let mut byte = [0u8; 1];
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            return Ok(None);
        }
        if byte[0] == b'\n' {
            break;
        }
        header.push(byte[0]);
    }
    let header = String::from_utf8_lossy(&header).trim().to_string();
    if let Some(value) = header
        .strip_prefix("Content-Length:")
        .or_else(|| header.strip_prefix("content-length:"))
    {
        content_length = value.trim().parse::<usize>().ok();
    }
    // Further headers until the blank line.
    loop {
        let mut line = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            let n = reader.read(&mut byte).await?;
            if n == 0 {
                return Ok(None);
            }
            if byte[0] == b'\n' {
                break;
            }
            line.push(byte[0]);
        }
        let line = String::from_utf8_lossy(&line).trim().to_string();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    let length = match content_length {
        Some(length) => length,
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "missing Content-Length header",
            ))
        }
    };
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;
    Ok(Some((body, false)))
}

/// Write one message in the client's wire format.
async fn write_message<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    value: &Value,
    ndjson: bool,
) -> Result<(), std::io::Error> {
    let body = serde_json::to_vec(value)?;
    if ndjson {
        writer.write_all(&body).await?;
        writer.write_all(b"\n").await?;
    } else {
        writer
            .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
            .await?;
        writer.write_all(&body).await?;
    }
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn framing_round_trips() {
        let payload = json!({ "jsonrpc": "2.0", "id": 1, "result": { "ok": true } });
        let mut buffer = Vec::new();
        write_message(&mut buffer, &payload, false).await.unwrap();

        let mut cursor = std::io::Cursor::new(buffer);
        let (message, _ndjson) = read_message(&mut cursor).await.unwrap().unwrap();
        let parsed: Value = serde_json::from_slice(&message).unwrap();
        assert_eq!(parsed["result"]["ok"], json!(true));
    }

    #[tokio::test]
    async fn read_eof_returns_none() {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        let message = read_message(&mut cursor).await.unwrap();
        assert!(message.is_none());
    }
}
