# ghostedit

A headless editor MCP server. File operations plus direct language server
protocol clients. No editor, no plugins, no hangs.

Every tool call runs inside a hard deadline. A timeout returns a structured
error, never silence.

## Status

Phase 0 (skeleton) and Phase 1 (LSP manager, rust-analyzer) are in progress.
The plan lives in `plan.md`.

## Run

```bash
cargo build
target/debug/ghostedit --cwd /path/to/workspace
```

Connect over stdio MCP. Configuration:

```toml
# ~/.config/ghostedit/config.toml
[languages.rust]
extensions = [".rs"]
command = ["rust-analyzer"]

[languages.python]
extensions = [".py"]
command = ["pyright-langserver", "--stdio"]

[languages.java]
extensions = [".java"]
command = ["jdtls"]
deadlines_ms = { read = 5000, edit = 10000, rename = 20000, symbol = 15000 }
```

A project `.ghostedit/config.toml` replaces whole language entries from the
global file.

## Tools

| Tool | Purpose |
|---|---|
| `open_file` | Open a file, report facts, start the language server |
| `read_file` | Numbered lines, 1-based |
| `edit_lines` | Range replace/insert, atomic save |
| `edit_text` | Exact text replace, whitespace tolerant, closest-window hint |
| `goto_definition` | LSP definition (Phase 1) |
| `hover` | LSP type/docs (Phase 1) |
| `references` | LSP references (Phase 1) |
| `diagnostics` | Per-file or all-open diagnostics (Phase 1) |
| `rename` | LSP rename with atomic saves (Phase 1) |

Deadlines: the `initialize` response carries the effective deadline table.
An agent-supplied `deadline_ms` can only lower a budget, never raise it.

## Development

```bash
cargo test
cargo build
```

Rules for contributors: stdout carries the MCP protocol only; logs go to
stderr. Every write is atomic when the filesystem supports it. Every tool
call returns within its deadline.
