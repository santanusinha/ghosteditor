//! P2.13: conformance suite. One scenario per language server that exists on
//! the host. Each scenario drives ghostedit over stdio MCP against a real
//! language server (rust-analyzer, pyright, gopls, typescript-language-server)
//! and asserts one shared tool contract: hover, goto_definition, references,
//! document outline, diagnostics, rename round trip, edit, and read.
//!
//! The purpose is to catch per-server handshake quirks before release. A
//! scenario skips when its server binary is absent. Each scenario builds a
//! disposable workspace; a passing scenario removes it, a failing scenario
//! leaves it behind and the panic message carries the path.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

// ---------- scenario table ----------

struct Hover {
    file: &'static str,
    line: usize,
    column: usize,
    /// Lower-case substring expected in the reply text.
    contains: &'static str,
}

struct Defn {
    file: &'static str,
    line: usize,
}

struct Diag {
    file: &'static str,
    /// None: the reply must say no diagnostics. Some: poll until an item
    /// with this lower-case substring arrives.
    hint: Option<&'static str>,
}

struct Spec {
    language: &'static str,
    /// Candidate commands (binary plus args). First one that resolves wins.
    candidates: &'static [&'static [&'static str]],
    extensions: &'static [&'static str],
    files: &'static [(&'static str, &'static str)],
    hover: Hover,
    defn: Defn,
    /// Name expected in the document outline.
    outline_name: &'static str,
    references_min: usize,
    diag: Diag,
    /// Old symbol name for the rename round trip.
    old_name: &'static str,
    new_name: &'static str,
    /// Definition-site text expected on disk after rename, e.g. "fn plus(".
    def_site: &'static str,
    /// Comment marker for the edit round trip.
    marker: &'static str,
    /// File with an unused import/symbol that has a quickfix action.
    quickfix_file: &'static str,
    /// 1-based (line, column) of the quickfix target.
    quickfix_line: usize,
    quickfix_column: usize,
    /// Lower-case substring of the quickfix title to apply.
    quickfix_title: &'static str,
    /// Text that must be gone from the file after the quickfix applies.
    quickfix_gone: &'static str,
}

fn specs() -> Vec<Spec> {
    vec![
        Spec {
            language: "rust",
            candidates: &[
                &["/home/santanu/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rust-analyzer"],
                &["rust-analyzer"],
            ],
            extensions: &[".rs"],
            files: &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"conf\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
                ),
                (
                    "src/main.rs",
                    "use std::collections::HashMap;\n\nfn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\nfn main() {\n    let sum = add(1, 2);\n    println!(\"{sum}\");\n}\n\nmod bad;\n",
                ),
                (
                    "src/bad.rs",
                    "pub fn wrong() -> i32 {\n    let s: i32 = \"text\";\n    s\n}\n",
                ),
            ],
            hover: Hover {
                file: "src/main.rs",
                line: 8,
                column: 15,
                contains: "add",
            },
            defn: Defn {
                file: "src/main.rs",
                line: 3,
            },
            outline_name: "add",
            references_min: 2,
            diag: Diag {
                file: "src/bad.rs",
                hint: Some("mismatched types"),
            },
            old_name: "add",
            new_name: "plus",
            def_site: "fn plus(",
            marker: "// marker",
            quickfix_file: "src/main.rs",
            quickfix_line: 1,
            quickfix_column: 20,
            quickfix_title: "remove",
            quickfix_gone: "HashMap",
        },
        Spec {
            language: "python",
            candidates: &[&["pyright-langserver", "--stdio"], &["pyright"]],
            extensions: &[".py"],
            files: &[(
                "sample.py",
                "import json\n\ndef add(a: int, b: int) -> int:\n    return a + b\n\n\nresult = add(3, 4)\nprint(result)\n",
            )],
            hover: Hover {
                file: "sample.py",
                line: 6,
                column: 10,
                contains: "add",
            },
            defn: Defn {
                file: "sample.py",
                line: 3,
            },
            outline_name: "add",
            references_min: 2,
            diag: Diag {
                file: "sample.py",
                hint: None,
            },
            old_name: "add",
            new_name: "plus",
            def_site: "def plus(",
            marker: "# marker",
            quickfix_file: "sample.py",
            quickfix_line: 1,
            quickfix_column: 8,
            quickfix_title: "remove",
            quickfix_gone: "json",
        },
        Spec {
            language: "go",
            candidates: &[&["gopls"]],
            extensions: &[".go"],
            files: &[
                ("go.mod", "module conf\n\ngo 1.22\n"),
                (
                    "main.go",
                    "package main\n\nimport \"fmt\"\nimport \"os\"\n\nfunc add(a int, b int) int {\n\treturn a + b\n}\n\nfunc main() {\n\tfmt.Println(add(1, 2))\n\tfmt.Println(add(1, \"x\"))\n}\n",
                ),
            ],
            hover: Hover {
                file: "main.go",
                line: 12,
                column: 14,
                contains: "func add",
            },
            defn: Defn {
                file: "main.go",
                line: 6,
            },
            outline_name: "add",
            references_min: 2,
            diag: Diag {
                file: "main.go",
                hint: Some("cannot use"),
            },
            old_name: "add",
            new_name: "plus",
            def_site: "func plus(",
            marker: "// marker",
            quickfix_file: "main.go",
            quickfix_line: 3,
            quickfix_column: 8,
            quickfix_title: "organize imports",
            quickfix_gone: "\"os\"",
        },
        Spec {
            language: "typescript",
            candidates: &[&["typescript-language-server", "--stdio"], &["tsserver"]],
            extensions: &[".ts"],
            files: &[
                (
                    "sample.ts",
                    "import os from \"os\";\n\nfunction add(a: number, b: number): number {\n  return a + b;\n}\n\nconst sum = add(1, 2);\nconsole.log(sum);\n",
                ),
                (
                    "bad.ts",
                    "function greet(name: string): string {\n  return \"hi \" + name;\n}\n\nconst message = greet(42);\nconsole.log(message);\n",
                ),
            ],
            hover: Hover {
                file: "sample.ts",
                line: 6,
                column: 13,
                contains: "function add",
            },
            defn: Defn {
                file: "sample.ts",
                line: 3,
            },
            outline_name: "add",
            references_min: 2,
            diag: Diag {
                file: "bad.ts",
                hint: Some("not assignable"),
            },
            old_name: "add",
            new_name: "plus",
            def_site: "function plus(",
            marker: "// marker",
            quickfix_file: "sample.ts",
            quickfix_line: 1,
            quickfix_column: 10,
            quickfix_title: "remove",
            quickfix_gone: "\"os\"",
        },
    ]
}

// ---------- resolution and workspace setup ----------

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

fn resolve_command(spec: &Spec) -> Option<Vec<String>> {
    for candidate in spec.candidates {
        let head = candidate[0];
        let exists = if head.contains('/') {
            Path::new(head).is_file()
        } else {
            on_path(head)
        };
        if exists {
            return Some(candidate.iter().map(|c| c.to_string()).collect());
        }
    }
    None
}

/// Build a workspace; None means skip. The caller removes it on success.
fn workspace(spec: &Spec) -> Option<PathBuf> {
    let command = resolve_command(spec)?;
    let unique = format!(
        "ghostedit-conformance-{}-{}",
        spec.language,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let root = std::env::temp_dir().join(unique);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    for (name, content) in spec.files {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }
    // Config entry with the resolved command; registry refresh off keeps the
    // scenario offline.
    let quoted: Vec<String> = command
        .iter()
        .map(|c| format!("\"{}\"", c.replace('\\', "\\\\")))
        .collect();
    let config = format!(
        "[registry]\nrefresh = \"off\"\n\n[languages.{}]\nextensions = [{}]\ncommand = [{}]\n",
        spec.language,
        spec.extensions
            .iter()
            .map(|e| format!("\"{e}\""))
            .collect::<Vec<_>>()
            .join(", "),
        quoted.join(", "),
    );
    std::fs::write(root.join("conformance-config.toml"), config).unwrap();
    Some(root)
}

// ---------- stdio MCP driver ----------

fn frame(payload: &serde_json::Value) -> Vec<u8> {
    let body = serde_json::to_vec(payload).unwrap();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    out
}

struct Session {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    rx: mpsc::Receiver<serde_json::Value>,
    next_id: i64,
}

impl Session {
    fn spawn(root: &Path) -> Self {
        let binary = env!("CARGO_BIN_EXE_ghostedit");
        let mut child = Command::new(binary)
            .arg("--cwd")
            .arg(root)
            .arg("--config")
            .arg(root.join("conformance-config.toml"))
            // P3.9: keep conformance runs out of the real state dir.
            .arg("--log-file")
            .arg("off")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // Read framed replies line by line until EOF. ghostedit answers
            // every request, so each request has exactly one reply.
            let mut reader = stdout;
            let mut buffer: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let read = match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                buffer.extend_from_slice(&chunk[..read]);
                while let Some(message) = next_frame(&mut buffer) {
                    if tx.send(message).is_err() {
                        return;
                    }
                }
            }
        });
        Self {
            child,
            stdin,
            rx,
            next_id: 1,
        }
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }));
        self.receive()
    }

    fn notify(&mut self, method: &str) {
        self.send(serde_json::json!({
            "jsonrpc": "2.0", "method": method
        }));
    }

    fn send(&mut self, message: serde_json::Value) {
        self.stdin.write_all(&frame(&message)).unwrap();
        self.stdin.flush().unwrap();
    }

    fn receive(&self) -> serde_json::Value {
        self.rx
            .recv_timeout(Duration::from_millis(60_000))
            .expect("no reply from ghostedit within 60 s")
    }

    /// One tools/call round trip. Panics when the reply is a tool error.
    fn call(&mut self, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        let response = self.request(
            "tools/call",
            serde_json::json!({ "name": name, "arguments": arguments }),
        );
        let result = response["result"].clone();
        assert!(
            !result.is_null(),
            "tools/call {name} returned a protocol error: {response}"
        );
        let error_text = result["content"][0]["text"].as_str().unwrap_or("");
        assert!(
            result["isError"].as_bool() != Some(true),
            "tool {name} failed: {error_text}"
        );
        result["structuredContent"].clone()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Pull one Content-Length framed message off the front of the buffer.
fn next_frame(buffer: &mut Vec<u8>) -> Option<serde_json::Value> {
    let split = buffer.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&buffer[..split]).to_string();
    let length: usize = head
        .lines()
        .find_map(|l| l.trim().strip_prefix("Content-Length:"))?
        .trim()
        .parse()
        .ok()?;
    if buffer.len() < split + 4 + length {
        return None;
    }
    let body = buffer[split + 4..split + 4 + length].to_vec();
    buffer.drain(..split + 4 + length);
    serde_json::from_slice(&body).ok()
}

// ---------- scenario runner ----------

fn run(spec: &Spec) {
    let Some(root) = workspace(spec) else {
        eprintln!(
            "skipping {}: no server binary ({})",
            spec.language,
            spec.candidates
                .iter()
                .map(|c| c[0])
                .collect::<Vec<_>>()
                .join(", ")
        );
        return;
    };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        scenario(spec, &root);
    }));
    if outcome.is_err() {
        panic!(
            "conformance failure for {} — workspace kept at {}",
            spec.language,
            root.display()
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

fn scenario(spec: &Spec, root: &Path) {
    let mut session = Session::spawn(root);

    // initialize publishes the deadline table and the server status map.
    let init = session.request(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "conformance", "version": "0" }
        }),
    );
    session.notify("notifications/initialized");
    let deadlines = &init["result"]["ghostedit"]["deadlines"]["defaults_ms"];
    assert!(
        deadlines["read"].as_u64().unwrap_or(0) > 0,
        "no deadline table"
    );
    let status = &init["result"]["ghostedit"]["servers"]["languages"][spec.language];
    assert_eq!(
        status.as_str(),
        Some("available"),
        "language {} not reported available (got {status})",
        spec.language
    );

    let target = spec.hover.file;
    let target_path = root.join(target).display().to_string();

    // 1. hover at the call site.
    let content = session.call(
        "hover",
        serde_json::json!({
            "path": target_path,
            "line": spec.hover.line,
            "column": spec.hover.column,
        }),
    );
    let hover_text = content["text"].as_str().unwrap_or("").to_lowercase();
    assert!(
        hover_text.contains(spec.hover.contains),
        "hover text was: {}",
        content["text"]
    );

    // 2. goto_definition lands in the definition file at the definition line.
    let content = session.call(
        "goto_definition",
        serde_json::json!({
            "path": target_path,
            "line": spec.hover.line,
            "column": spec.hover.column,
        }),
    );
    let locations = content["locations"].as_array().cloned().unwrap_or_default();
    let needle = format!(":{}:", spec.defn.line);
    assert!(
        locations.iter().any(|l| {
            l.as_str()
                .is_some_and(|s| s.contains(spec.defn.file) && s.contains(&needle))
        }),
        "no definition location for {} line {} in {locations:?}",
        spec.defn.file,
        spec.defn.line
    );

    // 3. references include the definition and the call sites.
    let content = session.call(
        "references",
        serde_json::json!({
            "path": target_path,
            "line": spec.hover.line,
            "column": spec.hover.column,
        }),
    );
    let count = content["count"].as_u64().unwrap_or(0);
    assert!(
        count as usize >= spec.references_min,
        "expected at least {} references, got {count}",
        spec.references_min
    );

    // 4. document outline contains the symbol.
    let content = session.call("symbols", serde_json::json!({ "path": target_path }));
    let symbols = content["symbols"].as_array().cloned().unwrap_or_default();
    assert!(
        symbols.iter().any(|s| s["name"] == spec.outline_name),
        "outline misses {}: {}",
        spec.outline_name,
        content["text"]
    );

    // 5. diagnostics: poll up to 15 s when errors are expected.
    let diag_path = root.join(spec.diag.file).display().to_string();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let content = session.call("diagnostics", serde_json::json!({ "path": diag_path }));
        let items = content["diagnostics"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        match spec.diag.hint {
            None => {
                assert_eq!(content["text"], "No diagnostics.", "expected a clean file");
                break;
            }
            Some(hint) => {
                let hit = items.iter().any(|d| {
                    let severity = d["severity"].as_str().unwrap_or("");
                    let message = d["message"].as_str().unwrap_or("").to_lowercase();
                    (severity == "ERROR" || severity == "WARN") && message.contains(hint)
                });
                if hit {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "no diagnostic containing {hint:?}; last reply: {}",
                    content["text"]
                );
                std::thread::sleep(Duration::from_millis(1_000));
            }
        }
    }

    // 7. rename round trip: reply lists files and disk carries the new name.
    let content = session.call(
        "rename",
        serde_json::json!({
            "path": target_path,
            "line": spec.hover.line,
            "column": spec.hover.column,
            "new_name": spec.new_name,
        }),
    );
    let files = content["files"].as_array().cloned().unwrap_or_default();
    assert!(!files.is_empty(), "rename changed no files");
    let disk = std::fs::read_to_string(root.join(target)).unwrap();
    assert!(
        disk.contains(spec.def_site),
        "disk misses {} after rename; content:\n{disk}",
        spec.def_site
    );
    assert!(
        !disk.contains(spec.old_name),
        "old name {old} still on disk:\n{disk}",
        old = spec.old_name
    );

    // 7. code_actions: list shows a quickfix, apply removes the unused import.
    let quickfix_path = root.join(spec.quickfix_file).display().to_string();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let index = loop {
        let content = session.call(
            "code_actions",
            serde_json::json!({
                "path": quickfix_path,
                "line": spec.quickfix_line,
                "column": spec.quickfix_column,
            }),
        );
        let entries = content["actions"].as_array().cloned().unwrap_or_default();
        let found = entries.iter().find(|a| {
            a["title"]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .contains(spec.quickfix_title)
        });
        if let Some(found) = found {
            break found["index"].as_u64().unwrap();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no quickfix containing {:?}; last reply: {}",
            spec.quickfix_title,
            content["text"]
        );
        std::thread::sleep(Duration::from_millis(1_000));
    };
    let content = session.call(
        "code_actions",
        serde_json::json!({
            "path": quickfix_path,
            "line": spec.quickfix_line,
            "column": spec.quickfix_column,
            "apply": index,
        }),
    );
    let files = content["files"].as_array().cloned().unwrap_or_default();
    assert!(!files.is_empty(), "code action changed no files");
    let disk = std::fs::read_to_string(root.join(spec.quickfix_file)).unwrap();
    assert!(
        !disk.contains(spec.quickfix_gone),
        "quickfix target still on disk:\n{disk}"
    );

    // 8. edit_lines inserts at the top and saves.
    let content = session.call(
        "edit_lines",
        serde_json::json!({
            "path": target_path,
            "start_line": 1,
            "end_line": 0,
            "text": spec.marker,
        }),
    );
    assert_eq!(content["mode"], "insert");
    assert_eq!(content["saved"], true);

    // 9. read_file returns the marker at line 1.
    let content = session.call(
        "read_file",
        serde_json::json!({
            "path": target_path,
            "start_line": 1,
            "end_line": 1,
        }),
    );
    assert_eq!(content["start_line"], 1);
    let first_line = content["text"].as_str().unwrap_or("");
    assert!(first_line.contains(spec.marker), "line 1 was: {first_line}");
}

// ---------- tests ----------

macro_rules! conformance {
    ($name:ident, $language:literal) => {
        #[test]
        fn $name() {
            for spec in specs() {
                if spec.language == $language {
                    run(&spec);
                    return;
                }
            }
            unreachable!("no spec for {}", $language);
        }
    };
}

conformance!(rust_conformance, "rust");
conformance!(python_conformance, "python");
conformance!(go_conformance, "go");
conformance!(typescript_conformance, "typescript");
