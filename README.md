# ghostedit

A headless editor MCP server. File operations plus direct language server
clients. No editor process, no plugins, no hangs.

Every tool call runs inside a hard deadline. A timeout returns a structured
error, never silence.

## Quick start

```bash
cargo build --release
./target/release/ghostedit --cwd /path/to/workspace
```

ghostedit speaks MCP over stdio. Point your MCP client at the binary:

```json
{
  "mcpServers": {
    "ghostedit": {
      "command": "/path/to/target/release/ghostedit",
      "args": ["--cwd", "/path/to/workspace"]
    }
  }
}
```

One process serves one workspace. The process dies with the client. Logs go
to a state file (see Logs); stderr carries errors only; stdout carries the
MCP protocol only.

## Configuration

ghostedit reads two TOML files:

| File | Role |
|---|---|
| `~/.config/ghostedit/config.toml` | Global defaults |
| `<workspace>/.ghostedit/config.toml` | Project overrides |

The project file replaces whole tables from the global file. A
`[languages.X]` entry and the `[registry]` table never merge field by field.

```toml
# ~/.config/ghostedit/config.toml
# Global deadline defaults, in milliseconds.
defaults_ms = { read = 10000, edit = 5000, rename = 8000, symbol = 20000, install = 300000 }

[languages.rust]
extensions = [".rs"]
command = ["rust-analyzer"]

[languages.typescript]
extensions = [".ts", ".tsx", ".js", ".jsx"]
command = ["typescript-language-server", "--stdio"]
initialization_options = { hostInfo = "ghostedit" }
settings = { typescript = { inlayHints = { includeInlayParameterTypeHints = true } } }
deadlines_ms = { read = 20000, edit = 10000, rename = 30000, symbol = 40000, install = 300000 }

[registry]
refresh = "latest"          # latest | pinned | off
# pin = "2026-10-02-uppity-comic"   # tag, used when refresh = "pinned"
```

### Language entries

| Key | Type | Meaning |
|---|---|---|
| `extensions` | array | File extensions this entry owns |
| `command` | array | argv to spawn the language server |
| `initialization_options` | table | Optional; sent as LSP `initializationOptions` |
| `settings` | table | Optional; sent as `workspace/didChangeConfiguration` |
| `deadlines_ms` | table | Optional; replaces all five budgets for this language |

A `deadlines_ms` table replaces all five budgets. A missing key takes the
built-in default, not the global `defaults_ms`.

### Flags and environment

| Flag | Environment | Default | Meaning |
|---|---|---|---|
| `--cwd` | `GE_CWD` | process cwd | Workspace root |
| `--config` | `GE_CONFIG` | `~/.config/ghostedit/config.toml` | Config path |
| `--debug` | `GE_DEBUG` | off | Alias for `--log-level debug` |
| `--idle-evict-min` | `GE_IDLE_EVICT_MIN` | 15 | Idle LSP eviction in minutes; 0 disables |
| `--log-file` | `GE_LOG_FILE` | `~/.local/state/ghostedit/ghostedit.log` | Log file path; `off` disables the file |
| `--log-level` | `GE_LOG_LEVEL` | `info` | `error`, `warn`, `info`, or `debug` |

Flags win over environment variables.

## Tools

13 tools. Names carry no prefix. Lines and columns are 1-based everywhere.

### File operations

#### `open_file`

Open a file and report facts. Never blocks on a language server.

| Argument | Type | Required | Notes |
|---|---|---|---|
| `path` | string | yes | |
| `deadline_ms` | integer | no | Can only lower the budget |

Returns `exists`, `line_count`, `language`, `reloaded`.

#### `read_file`

Read numbered lines. The number prefix right-aligns to the widest printed
line number, then two spaces.

| Argument | Type | Required | Notes |
|---|---|---|---|
| `path` | string | yes | |
| `start_line` | integer | no | 1-based |
| `end_line` | integer | no | 1-based, inclusive |
| `deadline_ms` | integer | no | |

A long reply ends with a `... truncated at line N of M` note.

#### `edit_lines`

Replace a line range. To insert before a line, pass `start_line = end_line + 1`.

| Argument | Type | Required | Notes |
|---|---|---|---|
| `path` | string | yes | |
| `start_line` | integer | yes | 1-based |
| `end_line` | integer | yes | 1-based, inclusive |
| `text` | string | yes | New content |
| `save` | boolean | no | Default `true`; writes atomically |
| `deadline_ms` | integer | no | |

The reply carries `mode` (`replace` or `insert`), `new_line_count`, and
`write_outcome` (`atomic`, `direct`, or `none`).

#### `edit_text`

Replace exact text. Matches must span whole lines; trailing whitespace
differences are tolerated.

| Argument | Type | Required | Notes |
|---|---|---|---|
| `path` | string | yes | |
| `old_text` | string | yes | Must match whole lines |
| `new_text` | string | yes | |
| `replace_all` | boolean | no | Default `false` |
| `save` | boolean | no | Default `true` |
| `deadline_ms` | integer | no | |

Behavior on edge cases:

- One match: replaced in place.
- Many matches without `replace_all`: refused; the reply names the first
  match line.
- No match: refused; the reply shows the closest window with its line, so
  you can retry with better context.

### Language server operations

Position arguments (`line`, `column`) are 1-based. Every tool in this group
starts the language server on first use and waits for its first diagnostics
push before it answers.

#### `goto_definition`

| Argument | Type | Required |
|---|---|---|
| `path` | string | yes |
| `line` | integer | yes |
| `column` | integer | yes |
| `deadline_ms` | integer | no |

Returns `locations` with `path`, `line`, `column`.

#### `hover`

Same arguments as `goto_definition`. Returns the hover contents as text.

#### `references`

Same arguments as `goto_definition`. Returns every reference site.

#### `diagnostics`

| Argument | Type | Required |
|---|---|---|
| `path` | string | yes |
| `deadline_ms` | integer | no |

Returns `diagnostics` items with `line`, `column`, `severity`, `message`,
`source`. Empty when the file is clean.

#### `code_actions`

List quickfixes and refactors at a position, or execute one.

| Argument | Type | Required | Notes |
|---|---|---|---|
| `path` | string | yes | |
| `line` | integer | yes | |
| `column` | integer | yes | |
| `apply` | integer | no | Index from a previous list reply |
| `deadline_ms` | integer | no | |

Without `apply`, the reply lists `actions` items with `index`, `title`,
`kind`, `is_preferred`, and `has_edit`. With `apply`, the tool re-requests
the list, resolves the action when needed, applies its edit, and saves the
touched files.

#### `rename`

Rename a symbol across the workspace. Every touched file is saved atomically.

| Argument | Type | Required |
|---|---|---|
| `path` | string | yes |
| `line` | integer | yes |
| `column` | integer | yes |
| `new_name` | string | yes |
| `deadline_ms` | integer | no |

Returns `files`, the list of changed paths.

#### `symbols`

Two modes. Pass `path` for a document outline. Pass `path` and `query` for a
workspace-wide name search.

| Argument | Type | Required | Notes |
|---|---|---|---|
| `path` | string | one of | Outline file, or server picker for `query` |
| `query` | string | one of | Name search across the workspace |
| `max_results` | integer | no | Default 200, clamped to 1..500 |
| `offset` | integer | no | Page from `next_offset` |
| `deadline_ms` | integer | no | |

Paged replies carry `total`, `truncated`, and `next_offset`.

### Workspace operations

#### `search`

Search file contents with the built-in walker. Literal matches are
case-insensitive. Ignored and binary files are skipped.

| Argument | Type | Required | Notes |
|---|---|---|---|
| `query` | string | yes | Substring or regex pattern |
| `regex` | boolean | no | Default `false` |
| `offset` | integer | no | Skip the first N matches |
| `glob` | string | no | Restrict to matching paths |
| `deadline_ms` | integer | no | |

Returns `matches` items with `path`, `line`, `column`, `text`, plus `count`,
`files_scanned`, `truncated`, and `next_offset`.

Limits: 200 matches per page, 4 MiB per file, long lines cut at 200
characters. The glob filter runs before any file I/O, so a scoped search
never reads rejected files.

Glob syntax: `*` matches within one path segment, `**` crosses segments, `?`
matches one character. `a/**/b` also matches `a/b`. A leading `**/` is
optional. Example: `src/**/*.rs`.

#### `install_server`

Install a language server from the registry by recipe name.

| Argument | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | Recipe name, for example `pyright` |
| `version` | string | no | Defaults to the recipe version |
| `deadline_ms` | integer | no | Default 300000 |

The install keeps running past the deadline. The tool waits until one second
before the budget, then returns the current status (`running`, `installed`,
or `failed`). Poll by calling again with the same `name`.

## Deadline semantics

Built-in defaults, in milliseconds:

| Kind | Default | Used by |
|---|---|---|
| `read` | 10000 | `open_file`, `read_file`, position tools, `search` |
| `edit` | 5000 | `edit_lines`, `edit_text` |
| `rename` | 8000 | `rename` |
| `symbol` | 20000 | `symbols` |
| `install` | 300000 | `install_server` |

Rules:

1. The `initialize` response publishes the effective table under
   `result.ghostedit.deadlines`, with `defaults_ms` and a `languages` map.
2. A `deadline_ms` argument can only lower a budget. It can never raise one.
3. Every timeout returns a structured error. The reply carries
   `isError: true` and `structuredContent` with the code `timeout`, the
   layer, and the deadline.

```json
{
  "isError": true,
  "structuredContent": {
    "error": {
      "code": "timeout",
      "tool": "hover",
      "message": "timeout in tool hover after 10000 ms",
      "details": { "layer": "tool hover", "deadline_ms": 10000 }
    }
  }
}
```

## Language servers

### Zero-config routing

The binary embeds a snapshot of the mason-registry LSP recipes: 330 recipes,
273 installable. When no config entry matches a file, ghostedit looks up the
extension in the registry and probes, in order:

1. The binary from the config `command`.
2. The recipe binary on `PATH`.
3. The recipe binary in the install directory.

This makes common servers work with no configuration at all. The
`initialize` response reports the state under `result.ghostedit.servers`:
the registry pin, recipe counts, and a status map per language
(`available`, `not_installed`, `no_recipe`).

### Missing servers

When no server resolves for a file but a recipe exists, LSP tools return a
`language_server_missing` envelope instead of an error:

```json
{
  "text": "No language server configured for app.py. A recipe exists for pyright but it is not installed. Call install_server with name pyright.",
  "language_server_missing": {
    "path": "/ws/app.py",
    "extension": ".py",
    "recipe": "pyright",
    "recipe_status": "not_installed",
    "install_command": "install_server {\"name\": \"pyright\"}"
  }
}
```

When a server resolves but lacks a method, the tool refuses fast with a
structured `invalid` error. It never waits for a reply the server cannot send.

### Installs and cache

| Path | Content |
|---|---|
| `~/.local/share/ghostedit/lsp/<name>/bin/` | Installed server binaries |
| `~/.cache/ghostedit/installs/<name>.log` | Install job log |
| `~/.cache/ghostedit/registry/registry.json` | Refreshed registry cache |

The registry refreshes in the background at startup, with a 60 second cap.
Mode `latest` fetches the newest mason release, `pinned` fetches the tag in
`pin`, and `off` never fetches. Any failure falls back to the embedded
snapshot. Nothing prints to stdout.

### Idle eviction

Idle language server children are evicted to free memory. The window is 15
minutes by default; set `--idle-evict-min 0` to disable. A fresh spawn gets
a 30 second warm-up grace, so a server is never evicted mid-analysis. The
sweep interval adapts to the window: idle/4, clamped to 5-60 seconds. The
next tool call respawns the server.

## Logs

Logs go to a file. The default location is
`$XDG_STATE_HOME/ghostedit/ghostedit.log`, that is
`~/.local/state/ghostedit/ghostedit.log`. A relative or empty
`XDG_STATE_HOME` is ignored, per the XDG spec. `--log-file <path>` moves the
file. `--log-file off` disables it. Stdout is never touched.

- Levels: `error` < `warn` < `info` < `debug`. The default keeps `info`.
  `--log-level debug` (or `--debug`) keeps everything.
- Precedence: `--log-level` > `--debug` > `GE_LOG_LEVEL` > `GE_DEBUG` > info.
- Line format: `2026-10-03T09:07:03.123Z INFO  message`. UTC, milliseconds.
- `error` lines also print to stderr. Fatal startup errors stay visible when
  the file is off or unavailable.
- Rotation: at 10 MiB the file moves to `ghostedit.log.1`. One old file is
  kept; a newer rotation replaces it.
- Writes are asynchronous. One writer thread owns the file, so no tool call
  blocks on log I/O. The queue drains at exit and on SIGTERM.
- If the file cannot be opened, one stderr line says so and logging falls
  back to stderr. A tool call never fails because of logging.

Several ghostedit processes may append to one file. Each line is one
append-mode write. Short lines do not interleave; very long lines can.

## Wire protocol

ghostedit speaks both MCP stdio framings. It mirrors the format of the
client's first message: newline-delimited JSON in, newline-delimited JSON
out; `Content-Length` frames in, frames out. The declared protocol version
is `2024-11-05`.

## Development

```bash
cargo test          # unit, integration, mock, conformance, wire
cargo clippy
cargo fmt --check
```

- `tests/mock_lsp.rs` drives the real binary against a mock language server.
  The suite skips when `python3` is absent.
- `tests/conformance.rs` runs one scenario per real server (rust, python,
  go, typescript). A scenario skips when its server binary is absent.
- `bench/RESULTS.md` records the token and latency benchmarks against the
  nvim baseline. `bench/bench.py` reruns them.
- `xtask registry-gen` regenerates the embedded registry snapshot from a
  mason release.

### Static release binary

The release build is one static executable with no runtime dependencies.

```bash
RUSTFLAGS="-C target-feature=+crt-static" cargo build --locked --release \
  --target x86_64-unknown-linux-gnu
file target/x86_64-unknown-linux-gnu/release/ghostedit
# ELF 64-bit ... statically linked
```

A push of a `v*` tag runs `.github/workflows/release.yml`. The workflow builds
the binary on ubuntu, checks that it is statically linked, answers one
`initialize` round trip, strips it, and attaches a tarball (binary, README,
LICENSE, CHANGELOG) plus a sha256 checksum to the GitHub release.

The toolchain is pinned in `rust-toolchain.toml`. Bump the pin deliberately so
release builds stay reproducible. A musl build (`x86_64-unknown-linux-musl`)
also works when `musl-tools` is installed; the shipped binary uses the glibc
static recipe above because CI verifies exactly that one.

## Status

Phase 3 (polish) is complete. The full plan lives in `plan.md`. See
`CHANGELOG.md` for the change history.
