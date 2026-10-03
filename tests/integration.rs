//! End-to-end integration tests against real ghostedit over stdio MCP.
//! The LSP-dependent tests use a controlled python echo/stub server, so
//! the suite stays hermetic; the rust-analyzer runs are manual.

use std::io::{Read, Write};

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
    // Keep the suite out of the real state dir unless a test opts in
    // with its own --log-file (P3.9).
    let mut command = Command::new(binary);
    if !args.contains(&"--log-file") {
        command.args(["--log-file", "off"]);
    }
    let mut child = command
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

#[test]
fn initialize_publishes_deadline_table() {
    // S8: the initialize response carries the effective deadline table.
    let responses = drive(&["--cwd", "/tmp"], &initialize_messages());
    let info = &responses[0]["result"];
    assert_eq!(info["serverInfo"]["name"], "ghostedit");
    let deadlines = &info["ghostedit"]["deadlines"]["defaults_ms"];
    assert!(deadlines["read"].is_u64());
    assert!(deadlines["edit"].is_u64());
    assert!(deadlines["rename"].is_u64());
    assert!(deadlines["symbol"].is_u64());
}

#[test]
fn tools_list_contains_phase0_and_phase1_tools() {
    let mut messages = initialize_messages();
    messages.push(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
    let responses = drive(&["--cwd", "/tmp"], &messages);
    let tools: Vec<String> = responses[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    for expected in [
        "open_file",
        "read_file",
        "edit_lines",
        "edit_text",
        "goto_definition",
        "hover",
        "references",
        "diagnostics",
        "rename",
    ] {
        assert!(tools.contains(&expected.to_string()), "missing {expected}");
    }
}

#[test]
fn unknown_tool_is_a_structured_protocol_error() {
    let mut messages = initialize_messages();
    messages.push(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"no_such_tool","arguments":{}}}));
    let responses = drive(&["--cwd", "/tmp"], &messages);
    assert!(responses[1].get("error").is_some());
}

#[test]
fn read_and_edit_round_trip_on_a_temp_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let sample = dir.path().join("sample.txt");
    std::fs::write(&sample, "one\ntwo\nthree\n").unwrap();

    let mut messages = initialize_messages();
    messages.push(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"sample.txt"}}}));
    messages.push(serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"edit_lines","arguments":{"path":"sample.txt","start_line":2,"end_line":2,"text":"TWO"}}}));
    messages.push(serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"sample.txt"}}}));
    let responses = drive(&["--cwd", &dir.path().display().to_string()], &messages);

    let first = &responses[1]["result"]["structuredContent"];
    let read_again = &responses[3]["result"]["structuredContent"];
    assert!(first["total_lines"] == 4); // one\ntwo\nthree\n -> 4 splits
    let saved = std::fs::read_to_string(&sample).unwrap();
    assert!(saved.contains("TWO"));
    assert!(read_again["total_lines"] == 4);
}

#[test]
fn edit_text_refuses_ambiguous_match_with_a_hint() {
    let dir = tempfile::tempdir().unwrap();
    let sample = dir.path().join("dupe.txt");
    std::fs::write(&sample, "same\nsame\n").unwrap();

    let mut messages = initialize_messages();
    messages.push(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"edit_text","arguments":{"path":"dupe.txt","old_text":"same","new_text":"other"}}}));
    let responses = drive(&["--cwd", &dir.path().display().to_string()], &messages);
    let result = &responses[1]["result"];
    assert_eq!(result["isError"], serde_json::json!(true));
    let hint = result["structuredContent"]["error"]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(hint.contains("matches"), "hint was: {hint}");
}

#[test]
fn symbols_tool_is_registered_with_a_symbol_deadline() {
    // P2.8: the 12th tool is 'symbols' with path and query inputs.
    let list_msg = serde_json::json!({"jsonrpc":"2.0","id":9,"method":"tools/list","params":{}});
    let responses = drive(
        &[],
        &[
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            list_msg,
        ],
    );
    let list = responses
        .iter()
        .find(|r| r.pointer("/result/tools").is_some())
        .unwrap();
    let tools = list.pointer("/result/tools").unwrap().as_array().unwrap();
    let symbols = tools
        .iter()
        .find(|t| t.get("name").and_then(|n| n.as_str()) == Some("symbols"))
        .expect("symbols tool registered");
    assert!(symbols.pointer("/inputSchema/properties/path").is_some());
    assert!(symbols.pointer("/inputSchema/properties/query").is_some());
}

#[test]
fn symbols_rejects_empty_arguments() {
    // P2.8: no path and no query is a structured Invalid error.
    let responses = drive(
        &[],
        &[
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}),
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"symbols","arguments":{}}}),
        ],
    );
    let call = responses
        .iter()
        .find(|r| r.get("id").and_then(|i| i.as_i64()) == Some(2))
        .unwrap();
    assert_eq!(
        call.pointer("/result/isError").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert!(call.pointer("/result/structuredContent/error").is_some());
}

#[test]
fn logs_land_in_the_file_when_log_file_is_set() {
    // P3.9: with --log-file, ready and shutdown land in the file, the
    // default level keeps debug lines out, and stdout stays protocol-only.
    use std::process::{Command, Stdio};
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("ghostedit.log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_ghostedit"))
        .arg("--cwd")
        .arg(dir.path())
        .arg("--log-file")
        .arg(log.display().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for message in &initialize_messages() {
        stdin.write_all(&frame(message)).unwrap();
    }
    drop(stdin); // EOF: clean shutdown path
    let mut protocol = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut protocol)
        .unwrap();
    child.wait().unwrap();

    let text = std::fs::read_to_string(&log).unwrap();
    let ready = text
        .lines()
        .find(|line| line.contains("ready"))
        .expect("ready line missing from the log file");
    assert!(ready.contains("INFO"), "ready line was: {ready}");
    assert!(text.contains("shutdown complete"), "log was: {text}");
    assert!(
        !text.contains("DEBUG"),
        "debug leaked at info level: {text}"
    );

    let out = String::from_utf8_lossy(&protocol);
    assert!(out.contains("ghostedit"), "stdout was: {out}");
    assert!(!out.contains("INFO"), "log text leaked to stdout: {out}");
}
