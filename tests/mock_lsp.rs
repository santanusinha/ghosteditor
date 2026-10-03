//! P2.12: hermetic integration tests that drive ghostedit over stdio MCP
//! against mock language servers. Each mock is a small python3 script that
//! speaks LSP Content-Length framing and answers a fixed scenario.
//!
//! Covered: multi-language routing in one session, capability-aware
//! refusals, diagnostics plumbing, document outline, workspace symbol
//! search, rename round trip, search (walker), install_server error
//! shape, and the missing-server reply for an unknown extension.
//!
//! python3 is required; the tests skip when it is absent.

use std::io::{Read, Write};

// ---------- wire helpers ----------

fn frame(payload: &serde_json::Value) -> Vec<u8> {
    let body = serde_json::to_vec(payload).unwrap();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    out
}

/// Drive ghostedit with framed messages; return parsed responses.
fn drive(args: &[&str], messages: &[serde_json::Value]) -> Vec<serde_json::Value> {
    use std::process::{Command, Stdio};
    let binary = env!("CARGO_BIN_EXE_ghostedit");
    let mut child = Command::new(binary)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for message in messages {
        stdin.write_all(&frame(message)).unwrap();
    }
    drop(stdin);
    let mut out = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
    child.wait().unwrap();
    let mut responses = Vec::new();
    let mut i = 0;
    while i < out.len() {
        let head_end = out[i..].windows(4).position(|w| w == b"\r\n\r\n").unwrap() + i;
        let headers = String::from_utf8_lossy(&out[i..head_end]).to_string();
        let length: usize = headers
            .lines()
            .find_map(|l| l.trim().strip_prefix("Content-Length:"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let body = &out[head_end + 4..head_end + 4 + length];
        responses.push(serde_json::from_slice(body).unwrap());
        i = head_end + 4 + length;
    }
    responses
}

fn initialize_messages() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}),
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    ]
}

fn call(id: i64, name: &str, arguments: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}})
}

/// The response with the given id, panicking when absent.
fn by_id(responses: &[serde_json::Value], id: i64) -> &serde_json::Value {
    responses
        .iter()
        .find(|r| r.get("id").and_then(|i| i.as_i64()) == Some(id))
        .unwrap_or_else(|| panic!("no response with id {id}"))
}

fn struct_content(response: &serde_json::Value) -> &serde_json::Value {
    response
        .pointer("/result/structuredContent")
        .unwrap_or_else(|| panic!("no structuredContent in {response}"))
}

// ---------- mock language server ----------

/// The mock script. argv[1] selects the scenario:
/// - a language tag (shown in hover replies)
/// - "nocaps": advertise no capabilities at all
const MOCK_LSP: &str = r#"
import sys, json

LANG = "unknown"
if len(sys.argv) > 1:
    LANG = sys.argv[1]
NO_CAPS = LANG == "nocaps"

def recv():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if line in (b"\r\n", b"\n", b""):
            break
        k, _, v = line.decode().partition(":")
        headers[k.strip()] = v.strip()
    body = sys.stdin.buffer.read(int(headers["Content-Length"]))
    return json.loads(body)

def send(obj):
    body = json.dumps(obj).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    sys.stdout.buffer.flush()

CAPS = {
    "hoverProvider": True,
    "definitionProvider": True,
    "referencesProvider": True,
    "documentSymbolProvider": True,
    "workspaceSymbolProvider": True,
    "renameProvider": True,
    "textDocumentSync": 1,
}

def range(sl, sc, el, ec):
    return {"start": {"line": sl, "character": sc}, "end": {"line": el, "character": ec}}

while True:
    try:
        msg = recv()
    except Exception:
        break
    method = msg.get("method", "")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {
            "capabilities": {} if NO_CAPS else CAPS,
            "serverInfo": {"name": "mock-" + LANG},
        }})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": None})
    elif method == "exit":
        break
    elif method == "textDocument/didOpen":
        doc = msg["params"]["textDocument"]
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {
            "uri": doc["uri"],
            "diagnostics": [{
                "range": range(0, 0, 0, 5),
                "severity": 1,
                "message": "mock diagnostic in " + LANG,
                "source": "mock",
            }],
        }})
    elif method == "textDocument/hover":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {
            "contents": {"kind": "plaintext", "value": "mock hover: " + LANG},
        }})
    elif method == "textDocument/definition":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": [
            {"uri": msg["params"]["textDocument"]["uri"], "range": range(0, 0, 0, 1)}
        ]})
    elif method == "textDocument/references":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": [
            {"uri": msg["params"]["textDocument"]["uri"], "range": range(1, 0, 1, 3)},
            {"uri": msg["params"]["textDocument"]["uri"], "range": range(2, 0, 2, 3)},
        ]})
    elif method == "textDocument/documentSymbol":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": [
            {"name": "alpha", "kind": 12, "range": range(0, 0, 0, 5),
             "selectionRange": range(0, 0, 0, 5), "children": [
                {"name": "beta", "kind": 12, "range": range(1, 0, 1, 4),
                 "selectionRange": range(1, 0, 1, 4), "children": []},
             ]},
        ]})
    elif method == "workspace/symbol":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": [
            {"name": "alpha", "kind": 12,
             "location": {"uri": "file:///workspace/a.one", "range": range(0, 0, 0, 5)}},
        ]})
    elif method == "textDocument/rename":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {
            "documentChanges": [{
                "textDocument": {"uri": msg["params"]["textDocument"]["uri"], "version": None},
                "edits": [{"range": range(0, 0, 0, 5), "newText": "renamed"}],
            }],
        }})
    elif "id" in msg:
        send({"jsonrpc": "2.0", "id": msg["id"], "result": None})
"#;

/// Build a temp workspace with two mock languages (`.one` and `.two`) and
/// sample files. Returns the workspace dir.
fn two_language_workspace() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(root.join("mock_lsp.py"), MOCK_LSP).unwrap();
    std::fs::create_dir_all(root.join(".ghostedit")).unwrap();
    std::fs::write(
        root.join(".ghostedit/config.toml"),
        r#"
[languages.one]
extensions = [".one"]
command = ["python3", "mock_lsp.py", "one"]

[languages.two]
extensions = [".two"]
command = ["python3", "mock_lsp.py", "two"]
"#,
    )
    .unwrap();
    std::fs::write(root.join("a.one"), "alpha beta\nsecond line\nthird line\n").unwrap();
    std::fs::write(root.join("b.two"), "alpha beta\nsecond line\nthird line\n").unwrap();
    (dir, root)
}

/// True when python3 runs on this host.
fn python3_present() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

// ---------- tests ----------

#[test]
fn multi_language_hover_routes_per_extension() {
    if !python3_present() {
        eprintln!("python3 absent; skipping");
        return;
    }
    let (_dir, root) = two_language_workspace();
    let cwd = root.display().to_string();
    let mut messages = initialize_messages();
    messages.push(call(
        2,
        "hover",
        serde_json::json!({"path": "a.one", "line": 1, "column": 2}),
    ));
    messages.push(call(
        3,
        "hover",
        serde_json::json!({"path": "b.two", "line": 1, "column": 2}),
    ));
    let responses = drive(&["--cwd", &cwd], &messages);

    let one = struct_content(by_id(&responses, 2));
    let two = struct_content(by_id(&responses, 3));
    assert_eq!(one["text"].as_str().unwrap(), "mock hover: one");
    assert_eq!(two["text"].as_str().unwrap(), "mock hover: two");
}

#[test]
fn mock_diagnostics_are_surfaced() {
    if !python3_present() {
        eprintln!("python3 absent; skipping");
        return;
    }
    let (_dir, root) = two_language_workspace();
    let cwd = root.display().to_string();
    let mut messages = initialize_messages();
    messages.push(call(2, "diagnostics", serde_json::json!({"path": "a.one"})));
    let responses = drive(&["--cwd", &cwd], &messages);
    let content = struct_content(by_id(&responses, 2));
    let text = content["text"].as_str().unwrap();
    assert!(text.contains("mock diagnostic in one"), "text was: {text}");
    let list = content["diagnostics"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["severity"], "ERROR");
}

#[test]
fn mock_document_symbol_outline() {
    if !python3_present() {
        eprintln!("python3 absent; skipping");
        return;
    }
    let (_dir, root) = two_language_workspace();
    let cwd = root.display().to_string();
    let mut messages = initialize_messages();
    messages.push(call(2, "symbols", serde_json::json!({"path": "a.one"})));
    let responses = drive(&["--cwd", &cwd], &messages);
    let content = struct_content(by_id(&responses, 2));
    assert_eq!(content["language"], "one");
    let symbols = content["symbols"].as_array().unwrap();
    assert_eq!(symbols.len(), 2, "flat tree: {symbols:?}");
    assert_eq!(symbols[0]["name"], "alpha");
    assert_eq!(symbols[1]["name"], "beta");
}

#[test]
fn mock_workspace_symbol_search() {
    if !python3_present() {
        eprintln!("python3 absent; skipping");
        return;
    }
    let (_dir, root) = two_language_workspace();
    let cwd = root.display().to_string();
    let mut messages = initialize_messages();
    messages.push(call(
        2,
        "symbols",
        serde_json::json!({"path": "a.one", "query": "alp"}),
    ));
    let responses = drive(&["--cwd", &cwd], &messages);
    let content = struct_content(by_id(&responses, 2));
    assert_eq!(content["total"], 1);
    let symbols = content["symbols"].as_array().unwrap();
    assert_eq!(symbols[0]["name"], "alpha");
    assert!(!content["truncated"].as_bool().unwrap_or(true));
}

#[test]
fn mock_workspace_symbol_offset_pagination() {
    if !python3_present() {
        eprintln!("python3 absent; skipping");
        return;
    }
    let (_dir, root) = two_language_workspace();
    let cwd = root.display().to_string();
    let mut messages = initialize_messages();
    // The mock answers one symbol; offset beyond the total yields an empty
    // page with total intact (P3.2 hardening).
    messages.push(call(
        2,
        "symbols",
        serde_json::json!({"path": "a.one", "query": "alp", "offset": 1}),
    ));
    messages.push(call(
        3,
        "symbols",
        serde_json::json!({"path": "a.one", "query": "alp", "max_results": 1}),
    ));
    let responses = drive(&["--cwd", &cwd], &messages);
    let past_end = struct_content(by_id(&responses, 2));
    assert_eq!(past_end["total"], 1);
    assert_eq!(past_end["count"], 0);
    assert!(!past_end["truncated"].as_bool().unwrap_or(true));
    assert!(past_end["next_offset"].is_null());
    let page = struct_content(by_id(&responses, 3));
    assert_eq!(page["count"], 1);
    assert_eq!(page["symbols"].as_array().unwrap()[0]["name"], "alpha");
}

#[test]
fn mock_goto_definition_and_references() {
    if !python3_present() {
        eprintln!("python3 absent; skipping");
        return;
    }
    let (_dir, root) = two_language_workspace();
    let cwd = root.display().to_string();
    let mut messages = initialize_messages();
    messages.push(call(
        2,
        "goto_definition",
        serde_json::json!({"path": "a.one", "line": 2, "column": 3}),
    ));
    messages.push(call(
        3,
        "references",
        serde_json::json!({"path": "b.two", "line": 1, "column": 2}),
    ));
    let responses = drive(&["--cwd", &cwd], &messages);
    let def = struct_content(by_id(&responses, 2));
    assert_eq!(def["locations"].as_array().unwrap().len(), 1);
    assert!(def["text"].as_str().unwrap().contains("a.one:1:1"));
    let refs = struct_content(by_id(&responses, 3));
    assert_eq!(refs["count"], 2);
    assert!(refs["text"].as_str().unwrap().contains("b.two:2:1"));
}

#[test]
fn mock_rename_round_trip_saves_the_file() {
    if !python3_present() {
        eprintln!("python3 absent; skipping");
        return;
    }
    let (_dir, root) = two_language_workspace();
    let cwd = root.display().to_string();
    let mut messages = initialize_messages();
    messages.push(call(
        2,
        "rename",
        serde_json::json!({"path": "a.one", "line": 1, "column": 1, "new_name": "renamed"}),
    ));
    let responses = drive(&["--cwd", &cwd], &messages);
    let content = struct_content(by_id(&responses, 2));
    assert_eq!(
        content["files"].as_array().unwrap().len(),
        1,
        "content was: {content}"
    );
    let saved = std::fs::read_to_string(root.join("a.one")).unwrap();
    assert_eq!(saved, "renamed beta\nsecond line\nthird line\n");
}

#[test]
fn capability_absence_refuses_the_call() {
    if !python3_present() {
        eprintln!("python3 absent; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(root.join("mock_lsp.py"), MOCK_LSP).unwrap();
    std::fs::create_dir_all(root.join(".ghostedit")).unwrap();
    std::fs::write(
        root.join(".ghostedit/config.toml"),
        r#"
[languages.none]
extensions = [".one"]
command = ["python3", "mock_lsp.py", "nocaps"]
"#,
    )
    .unwrap();
    std::fs::write(root.join("a.one"), "alpha beta\n").unwrap();
    let cwd = root.display().to_string();

    let mut messages = initialize_messages();
    messages.push(call(
        2,
        "hover",
        serde_json::json!({"path": "a.one", "line": 1, "column": 1}),
    ));
    let responses = drive(&["--cwd", &cwd], &messages);
    let call_result = by_id(&responses, 2)["result"].clone();
    assert_eq!(call_result["isError"], serde_json::json!(true));
    let message = call_result
        .pointer("/structuredContent/error/message")
        .and_then(|m| m.as_str())
        .unwrap()
        .to_string();
    assert!(
        message.contains("does not support textDocument/hover"),
        "message was: {message}"
    );
}

#[test]
fn search_tool_finds_unique_string_in_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join("deep")).unwrap();
    std::fs::write(
        root.join("deep/probe.txt"),
        "nothing here\nzebra-unique-token\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("target/debug")).unwrap();
    std::fs::write(root.join("target/debug/probe.txt"), "zebra-unique-token\n").unwrap();
    let cwd = root.display().to_string();

    let mut messages = initialize_messages();
    messages.push(call(
        2,
        "search",
        serde_json::json!({"query": "zebra-unique-token"}),
    ));
    let responses = drive(&["--cwd", &cwd], &messages);
    let content = struct_content(by_id(&responses, 2));
    assert_eq!(content["count"], 1, "content: {content}");
    let matches = content["matches"].as_array().unwrap();
    assert_eq!(matches[0]["path"], "deep/probe.txt");
    assert_eq!(matches[0]["line"], 2);
    assert!(!content["truncated"].as_bool().unwrap_or(true));
}

#[test]
fn install_server_unknown_recipe_is_a_structured_error() {
    let mut messages = initialize_messages();
    messages.push(call(
        2,
        "install_server",
        serde_json::json!({"name": "definitely-not-a-recipe"}),
    ));
    let responses = drive(&["--cwd", "/tmp"], &messages);
    let result = by_id(&responses, 2)["result"].clone();
    assert_eq!(result["isError"], serde_json::json!(true));
    let message = result
        .pointer("/structuredContent/error/message")
        .and_then(|m| m.as_str())
        .unwrap()
        .to_string();
    assert!(
        message.contains("no registry recipe"),
        "message was: {message}"
    );
}

#[test]
fn hover_on_unknown_extension_reports_missing_server() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("mystery.zzz"), "content\n").unwrap();
    let cwd = dir.path().display().to_string();

    let mut messages = initialize_messages();
    messages.push(call(
        2,
        "hover",
        serde_json::json!({"path": "mystery.zzz", "line": 1, "column": 1}),
    ));
    let responses = drive(&["--cwd", &cwd], &messages);
    let result = by_id(&responses, 2)["result"].clone();
    // The reply may be a structured error or a missing-server envelope;
    // both must mention the missing server for the unknown extension.
    let rendered = result.to_string();
    assert!(
        rendered.to_lowercase().contains("language server"),
        "reply was: {rendered}"
    );
}
