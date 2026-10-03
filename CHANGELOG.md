# Changelog

All notable changes to ghostedit.

The format follows Keep a Changelog. Versions follow SemVer. Nothing is
released yet; the 0.1.0 section ships with the first release.

## [0.1.0] - Unreleased

### Added

- MCP stdio server: `initialize`, `tools/list`, `tools/call`, `ping`.
  Both wire formats work: newline-delimited JSON and `Content-Length`
  frames. The server mirrors the format of the client's first message.
- 13 tools: `open_file`, `read_file`, `edit_lines`, `edit_text`,
  `goto_definition`, `hover`, `references`, `diagnostics`, `code_actions`,
  `rename`, `install_server`, `search`, `symbols`.
- Hard deadlines on every tool call. The `initialize` response publishes the
  effective table under `result.ghostedit.deadlines`. An agent-supplied
  `deadline_ms` can only lower a budget. Every timeout returns a structured
  error, never silence.
- Per-language deadline configuration through `deadlines_ms`, with
  `initialization_options` and `settings` per language entry.
- Atomic writes: temp file, fsync, rename. A direct write is the fallback
  and is reported in the reply.
- Direct LSP clients on `lsp-types` plus a hand-rolled tokio layer:
  spawn per language, `didOpen` once, client-side version tracking, and a
  diagnostics mailbox.
- Cold-start handling: position tools, rename, symbols, and code_actions
  wait for the first diagnostics push, then retry once inside the deadline.
  A fuzzy fallback nudges a missed position to the nearest identifier.
- Document text sync: full-text `didChange` after external reloads and own
  edits, plus `workspace/didChangeWatchedFiles` for unopened documents.
  Server error `-32801` (content modified) retries inside the deadline.
- Supervision: a dead child restarts at most 3 times per 60 seconds, then
  stays down with a reason. File operations never depend on LSP health.
- Idle eviction of language server children: 15 minute default window, 30
  second warm-up grace for fresh spawns, adaptive sweep interval
  (idle/4, clamped to 5-60 seconds). `--idle-evict-min 0` disables it.
- Server registry: an embedded snapshot of 330 mason-registry LSP recipes
  (273 installable), an overlay for launch data, and background refresh
  from mason releases with SHA-256 verification. Modes `latest`, `pinned`,
  and `off`; any failure falls back to the snapshot.
- `install_server` tool with five installers (npm, pypi, cargo, golang,
  github). Argv-only spawns, a toolchain pre-check, an idempotent job
  manager, and per-job logs.
- `language_server_missing` envelope on LSP tools when no server resolves
  but a recipe exists. It carries the recipe name, status, and the install
  command.
- Capability-aware refusals: a tool refuses fast with a structured error
  when the server does not advertise its method.
- `search` tool: a gitignore-aware walker with literal and regex modes,
  binary sniffing, a 4 MiB file cap, 200 matches per page, and glob
  scoping. The glob filter runs before any file I/O.
- `symbols` tool: `documentSymbol` outline and `workspace/symbol` name
  search, with `max_results` clamping and `next_offset` pagination.
- `code_actions` tool: list and apply by index, with `codeAction/resolve`
  support for lazy actions.
- Rename across files: both `changes` and `documentChanges` replies are
  applied, and every touched file is saved atomically.
- Test suites: unit, hermetic integration against a mock language server,
  a per-language conformance suite against real servers, and a wire test.
- Benchmarks against the nvim baseline (`bench/bench.py`,
  `bench/RESULTS.md`): initialize latency and token cost per round trip.
- An `IndexStore` trait with a no-op implementation, so a real search
  backend can plug in later without tool-layer changes.
- Performance work: an event-driven diagnostics settle (25 ms mailbox
  polls up to 300 ms instead of a fixed sleep), an `open_file` LSP
  pre-warm task that never blocks the reply, and a parallel search
  walker (scoped thread pool, deterministic sort). A 4,920-file tree
  dropped from 269-632 ms to 50-130 ms per query.
- A per-language CI conformance matrix (rust, python, go, typescript);
  each job installs only its own language server.
- A release workflow: a `v*` tag builds one static linux-x86_64 binary,
  smoke tests it, and attaches a tarball plus a sha256 checksum to the
  GitHub release. The toolchain is pinned in `rust-toolchain.toml`.
- MIT `LICENSE` file.

### Changed

- The read deadline default is 10 seconds (was 2 seconds); the symbol
  default is 20 seconds (was 8 seconds). Cold language servers need the
  extra budget.
- Search and symbols stay walker and LSP based in v1. No local index
  database; the `IndexStore` seam covers a later backend.
- The release profile enables thin LTO and one codegen unit.
- The binary links the registry through the library crate instead of a
  second module copy; `cargo clippy --all-targets` is warning-free.

### Fixed

- Content-Length framing: the writer now sends frames, and the reader
  parses the length from the first header block.
- File URIs are canonical with no extra slashes.
- `didOpen` is sent exactly once per document; versions stay monotonic.
- Servers that gate replies on client capabilities now work: the client
  declares `textDocument.publishDiagnostics` (with `relatedInformation`)
  and `codeActionLiteralSupport` at initialize.
- `utf16_to_byte` advances through the string with `char_indices`, so
  rename edits land at the right offset.
- npm installs symlink the real binary instead of copying the node
  wrapper, which broke `require()` resolution.
- The document store lock is never re-entered; handlers drop the guard
  before any second lock.
- Server-to-client requests (progress and similar) are never treated as
  replies.
- `read_file` numbering right-aligns to the widest printed line number,
  matching the nvim tools and saving tokens on large reads.
