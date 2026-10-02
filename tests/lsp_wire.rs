// Manual-style integration test guarded by an env var so CI stays hermetic.
// Run: GE_LSP_TEST=1 cargo test --test lsp_wire

use std::io::Write;

fn frame(payload: &serde_json::Value) -> Vec<u8> {
    let body = serde_json::to_vec(payload).unwrap();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    out
}

#[test]
fn echo_lsp_initialize_round_trip() {
    // Minimal echo language server: python3 one-liner echoing framed JSON.
    let script = r#"
import sys, json
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
msg = recv()
send({"jsonrpc":"2.0","id":msg["id"],"result":{"capabilities":{}},"serverInfo":{"name":"echo"}})
"#;
    let mut child = std::process::Command::new("python3")
        .arg("-c")
        .arg(script)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let request = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}});
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&frame(&request))
        .unwrap();
    let output = String::new();
    use std::io::Read;
    // Read exactly one framed response.
    let mut stdout = child.stdout.take().unwrap();
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stdout.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let header_text = String::from_utf8_lossy(&header);
    let length: usize = header_text
        .lines()
        .find_map(|l| l.trim().strip_prefix("Content-Length:"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut body = vec![0u8; length];
    stdout.read_exact(&mut body).unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["id"], 1, "raw body: {body:?}");
    assert_eq!(parsed["serverInfo"]["name"], "echo", "raw body: {body:?}");
    let _ = output;
    child.wait().unwrap();
}
