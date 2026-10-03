# AGENTS.md

Guidance for agents that work on this repository.

## What this is

A Rust MCP server that edits files and talks to language servers directly.
No Neovim, no plugins, no editor process. The design goal is zero hangs:
every tool call has a hard deadline and returns a structured result.

The spec is `plan.md`. Read it before making design changes.

## Layout

| Path | Role |
|---|---|
| `src/main.rs` | Entry point, args, runtime, clean shutdown |
| `src/mcp/server.rs` | Stdio MCP: framing, dispatch, deadline watchdog |
| `src/mcp/tools.rs` | Tool registry and handlers |
| `src/docstore.rs` | In-memory document store, reload guard |
| `src/config.rs` | TOML config, deadline resolution |
| `src/fsops.rs` | Atomic writes |
| `src/lsp/` | Direct LSP clients (Phase 1) |
| `src/walker/` | Content search (Phase 2) |
| `src/util/` | Error envelope, async file logging |

## Rules

1. Never write to stdout. It carries the MCP protocol. Logs go to the state
   file (`~/.local/state/ghostedit/ghostedit.log`); stderr carries errors
   only. Tests spawn the binary with `--log-file off`.
2. Every tool call runs inside its deadline. A timeout returns a structured
   error through `ToolError`, never silence.
3. An agent-supplied `deadline_ms` can only lower a budget, never raise it.
4. Writes are atomic: temp file, fsync, rename. Fall back to a direct write
   only when the filesystem has no atomic rename.
5. Tool names carry no prefix. Keep the registry small; the grammar carries
   the value, not the tool count.
6. Run `cargo test` after every change. Fix new errors before the next
   change.
7. Keep `plan.md` and the phase TODO in sync with reality.
