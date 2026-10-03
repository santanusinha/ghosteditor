# ghostedit — A headless editor MCP

**Status: SPEC / PLAN — approved for prototype. No code written yet.**
**Revision 3: Rust runtime; no index in v1 (LSP owns symbols); per-server
configurable deadlines published at `initialize`.**

Date: 2026-10-01
Repo target: `/home/santanu/Work/OSS/ghostedit`

---

## 1. Problem statement

`neovim-use-mcp` edits files through a real Neovim process. It is token
efficient, but it hangs in real sessions. The hang has an architectural cause,
not a bug-level cause:

| #   | Cause                                                                                                                                                                                                                                               | Where it lives today                         |
| --- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------- |
| H1  | nvim is single threaded. While a Lua chunk polls (`vim.wait` loops up to 3 s for attach, 5 s for rename, 500 ms for diagnostics), nvim cannot answer any other request. The MCP side queues everything behind it.                                   | `lua.ts` OPEN*FILE/DIAGNOSTICS/LSP*\* chunks |
| H3  | The user config brings plugin land into a headless process: lazy-loading (the WARMUP chunk fires VeryLazy by hand), formatters, linters, autocmds. A plugin can block on a prompt, a hung child process, or a bad hook. Nondeterministic by nature. | `WARMUP` + `config-mode=user`                |
| H4  | No watchdog exists at the MCP layer. If nvim never replies, the tool call never returns and the agent stalls.                                                                                                                                       | missing                                      |

A linter bridge or a formatter that waits on input can wedge the one process
that all tools depend on. The fix is to stop depending on an editor.

## 2. Insight

nvim gives the current server four things:

1. buffer ops (read/set lines),
2. `vim.lsp` client management,
3. `vim.diagnostic` storage,
4. `apply_workspace_edit` + format.

These are the only parts an agent needs. They sit inside one OS process that
also carries a config, a plugin runtime and an input loop. The rest — TUI, key
maps, UI events — is what an agent never uses, and what produces the hangs.

**ghostedit** builds only the parts in the list: an in-memory document store,
direct LSP clients (JSON-RPC over stdio), disk writes, and a diagnostics
mailbox. No editor. No plugins. An event loop we own, so every tool call gets
a hard deadline.

## 3. Goals

- G1: Zero hangs. Every tool call returns within its deadline. Always.
- G2: Token efficiency at least equal to the current tools: numbered lines,
  small feedback payloads, edits that return fresh diagnostics.
- G3: One process per workspace. Short startup. Memory stays bounded.
- G4: Pluggable language servers via one config file, not a plugin ecosystem.
- G5: Drop-in habits: same tool grammar the agent already knows
  (open/read/edit/rename/hover/references/diagnostics).
- G6: The agent never waits on the first LSP answer. The server warms up
  in the background while the agent reads files.

Non-goals (for v1)

- No terminal UI, no keymaps, no treesitter highlighting.
- No formatter plugins; LSP `textDocument/formatting` only.
- No remote/workspace-wide formatters beyond LSP.

## 4. Architecture

```
                MCP client (agent)
                       │ stdio
                       ▼
     ┌─────────────────────────────────────────────┐
     │ ghostedit server (Rust)                      │
     │                                             │
     │  Tool layer      deadline-wrapped handlers  │
     │  Doc store       in-memory open documents   │
     │  LSP manager     one child per (lang, root)  │
     │  Fs layer        atomic writes              │
     └───────┬──────────────┬──────────────┬───────┘
             │ stdio JSON-RPC│              │
             ▼               ▼              ▼
       typescript-ls     gopls, rust-analyzer, ...
```

Runtime decision (from review): **Rust**. Reasons: single static binary,
cheap supervision of child processes, small memory footprint, no runtime
dependency to ship, and the user already works in Rust (tsindex, smdr and
codebase-memory-mcp are Rust or C neighbors). The LSP client will be
`lsp-types` + `tower-lsp` crates or a small hand-written JSON-RPC layer;
see 8c.

### 4.1 Process model

- One server per workspace, spawned by the agent, dies with it.

### 4.2 Tool surface (v1, 10 tools)

| Tool | Wraps | Notes |
| `search` | Built-in walker | Literal or regex token match over non-ignored files |
| `symbols` | LSP | File outline (documentSymbol); name search (workspace/symbol) |
| `rename` | LSP | WorkspaceEdit applied by ghostedit itself |
| `diagnostics` | DocStore | Fresh or all-open-files sweep |

Tool names carry no prefix (from review). Lines/columns 1-based, same as
today. Keep the `EditFeedback` payload. A later `code_actions` is a natural
v1.1 (needs action caching + `codeAction/resolve`); keep it out of v1.

- Lines/columns 1-based, same as today. Keep the `EditFeedback` payload.
- A later `code_actions` is a natural v1.1 (needs action caching +
  `codeAction/resolve`); keep it out of v1 to stay small.

### 4.3 Language server routing and install

```toml
[languages.typescript]
extensions = [".ts", ".tsx", ".js", ".jsx"]
command = ["typescript-language-server", "--stdio"]

[languages.python]
extensions = [".py"]
command = ["pyright-langserver", "--stdio"]
initialization_options = { ... }   # LSP-specific config

[languages.java]
extensions = [".java"]
command = ["jdtls"]
initialization_options = { ... }
settings = { ... }                 # workspace/didChangeConfiguration
deadlines_ms = { read = 5000, edit = 10000, rename = 20000, symbol = 15000 }
```

- File extension picks the language. Markers (`package.json`, `Cargo.toml`)
  can refine the default, but extension routing is the simple core.
- **Per-server LSP config** (from review): each language entry accepts
  `initialization_options` and `settings`, so complex servers such as jdtls
  get the same configuration the user has in the ss-nvim profile.
- **On-demand install** (from review): a registry of known language servers
  ships with the binary. It maps a language to install recipes
  (npm, cargo, pip, mason-like scripts). The registry refreshes in the
  background from a GitHub repo. When a tool needs a server that is absent,
  the response says `language_server_missing` and offers `install_server`
  as the next step; the install runs with user-visible stderr and does not
  block other tools.
- The pool keys a client as `(language, root)`. One workspace, many languages.
- Server availability is probed on use, not at startup, and reported in the
  tool response so the agent learns "no server for this file" fast and clean.

### 4.4 Document store and edit pipeline

```
edit → validate in memory → apply to buffer copy → LSP didChange
     → collect diagnostics → write to disk
```

- `didChange` sends only the range that moved (incremental sync), so the
  server stays fast on big files.
- Buffer is the source of truth while open. The write is atomic when the
  filesystem supports it (from review): write a temp file, fsync, rename.
  On filesystems without atomic rename, fall back to direct write and say
  so in the response.
- Diagnostics are state pushed by servers (`publishDiagnostics`), collected
  per document, and delivered with each edit's feedback. No polling loops.
  A `maxWaitMs` (default ~200) lets a fast server's update land; the tool
  returns without it if it does not.

### 4.5 Liveness rules (the anti-hang core)

### 4.5 Liveness rules (the anti-hang core)

1. **Every tool call has a hard deadline.** No infinite waits exist anywhere.
   Default budgets: reads ~2 s, edits ~5 s, rename ~8 s.
2. **Deadlines are configurable per language server** (round 3 decision). A
   language table entry can override every default, so slow servers
   (jdtls on a big workspace) get longer budgets while fast ones stay tight:

   ```toml
   [languages.java]
   command = ["jdtls"]
   deadlines_ms = { read = 5000, edit = 10000, rename = 20000, symbol = 15000 }
   ```

   Resolution order: tool argument (agent-supplied) → server-specific
   override → global `--deadline-ms` → built-in default. An agent-supplied
   value never raises the ceiling above the server override; it can only
   lower it. The agent cannot use a timeout to extend a wait.

3. **The agent learns the deadlines at initialize.** The MCP `initialize`
   response carries the effective deadline table per configured language:
   `{ languages: { java: { read_ms: 5000, ... } }, defaults_ms: { ... } }`.
   Any `wait_ms` / `timeout_ms` argument an agent sends is honoured only
   within these ceilings, so an agent timeout can never outlive the real
   budget and hang the session.
4. **Timeout ≠ silence.** A timeout returns a structured error: what was
   asked, which layer timed out, what state is safe, what to do next.
5. **A wedged child does not wedge the server.** LSP clients are independent
   processes. A dead client is evicted, its docs marked "LSP degraded", file
   ops still work. Reads and edits never depend on LSP health.
6. **No blocking on first contact.** `open_file` starts the server and
   returns "starting" with file facts. LSP answers fill in later.
7. **Supervision.** A child that dies restarts at most 3 times in 60 s, then
   stays down with a reason. The server never waits forever on a child.
8. **Edit check.** `edit_text` mirrors one trick from today's server: on
   no match, return the closest window with a similarity hint, so the agent
   retries with better context instead of failing blind.
9. **Global ops budget.** A workspace symbol query or a rename returns paged,
   bounded results (`max_results`, `next_offset`), so no answer floods the
   context.

### 4.6 Search and symbols

index, so a local one duplicates warm state we would then have to keep in
sync). Two sources only:

1. `search` — a small built-in content walker over non-ignored files
   (literal or regex token match, capped results, streaming scan).
   This covers arbitrary text in code, comments, strings, and non-code
   files (YAML, Markdown, Dockerfiles), which no LSP indexes.
2. `symbols` — `documentSymbol` for a file outline and
   `workspace/symbol` for name search, straight from the language
   server. If the server is cold or missing, the response says so.

- **IndexStore seam.** An internal `IndexStore` trait stays in the design.
  v1 ships a no-op implementation. If dogfooding shows the walker is too
  slow on large repos, Turso (libSQL) plugs in behind the trait in a later
  phase: file rows keyed by SHA-256 + mtime + size, FTS5 over symbol names,
  restart-stable (skip unchanged files), branch column as a stretch goal.
- **P3.5 DECISION (2026-10-03): keep walker-only. No Turso in v1.**
  Dogfood on vllm (43k walkable files): unscoped search 4-6 s warm, one
  cold run hit the 10 s read deadline and returned a structured timeout;
  scoped search with the new `glob` argument is sub-second (392 ms on
  `csrc/**/*.cpp`, 13 files). The walker plus glob scoping covers the real
  cases; an index would add the libsql dependency, build and invalidation
  machinery, and staleness risk for marginal gain. The seam lives in
  `src/index.rs` (`IndexStore` trait + `NoopIndex`), so a real backend
  plugs in later without tool-layer changes. The pathological case is
  covered by the deadline rules: a slow walk times out with a structured
  error and the agent retries with a glob.
- Cross-language and cold-start gaps are accepted as startup latency, not
  correctness failures. The anti-hang rules cover them: a cold server
  returns "starting" and the agent moves on.

### 4.7 File ops

- create / edit / delete handled through the DocStore so LSP sees every
  change (`didOpen`, `didChange`, `didClose`, and `didSave` when supported).
- Delete closes any open doc first, then unlinks. Create opens it. Neither
  needs an editor.

### 4.8 Diagnostics plumbing

- `publishDiagnostics` notifications land in a per-doc mailbox with version
  numbers, so stale diagnostics after an edit are detectable and droppable.
- The edit feedback returns the mailbox snapshot, not a poll result.

## 5. Config surface (v1)

| Flag / env                         | Default                           | Meaning                                          |
| ---------------------------------- | --------------------------------- | ------------------------------------------------ |
| `--cwd` / `GE_CWD`                 | process cwd                       | workspace root                                   |
| `--config` / `GE_CONFIG`           | `~/.config/ghostedit/config.toml` | language table                                   |
| `--languages`                      | auto                              | force language list                              |
| `--deadline-ms` / `GE_DEADLINE_MS` | per-tool defaults                 | global ceiling; per-server overrides win (4.5.2) |
| `--idle-evict-min`                 | 15                                | LSP child idle eviction                          |
| `--max-results`                    | 200                               | result budget for list tools                     |
| `--no-lsp`                         | off                               | file ops only mode                               |
| `--debug` / `GE_DEBUG`             | off                               | stderr tracing                                   |

## 6. Success criteria

| Test                | Passes when                                                                                                     |
| ------------------- | --------------------------------------------------------------------------------------------------------------- |
| S1 Hang             | A language server that never replies must not exceed its deadline; the server must still answer all other tools |
| S2 Hung child       | `kill -STOP` on a language server: edits still succeed with a degradation note                                  |
| S3 Boot             | `initialize` answers in < 250 ms cold, no synchronous work in the path                                          |
| S4 Tokens           | An edit round trip uses no more tokens than the nvim tool for the same change                                   |
| S5 Grammar          | An agent that knows the nvim tool set needs no new instructions to use ghostedit                                |
| S6 Rename           | A rename applies across files, saves each file, lists them                                                      |
| S7 Crash safety     | A kill during a write leaves the original file intact (atomic rename)                                           |
| S8 Deadline publish | The `initialize` response carries the effective per-language deadline table; agent timeouts clamp inside it     |

## 7. Risks and mitigations

| Risk                                                                 | Mitigation                                                                                                                 |
| -------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| Per-language server quirks (tsserver vs gopls handshake differences) | One conformance test suite per supported language in CI, the agent-lsp approach                                            |
| UTF-16 offset math in edits and renames                              | The `lsp-types` crate carries the offset rules; property-test round trips                                                  |
| External edits (the agent also uses shell)                           | DocStore compares disk mtime/size on read and reloads when they differ; LSP gets `didChangeWatchedFiles`                   |
| Slow content walker on very large repos                              | Result caps + streaming scan; if dogfooding shows it is too slow, Turso/libSQL plugs in behind the `IndexStore` seam (4.6) |
| libsql crate churn, if the index phase happens                       | Pin the crate version; the `IndexStore` trait keeps the backend swappable                                                  |
| ts_daemon/tsserver watch-mode bugs                                   | treat "stays in starting state" as its own timeout class                                                                   |

## 8. Phasing

- **Phase 0** — skeleton: stdio MCP server, config, DocStore,
  `open/read/edit_*` without LSP. Hang-proofing rules wired from day one.
  Dogfood against this very repo.
- **Phase 1** — LSP manager: one language (rust first, per 9.3), definition/
  hover/references/diagnostics. Rename with WorkspaceEdit application.
  Test target: a small sample project in C or Python.
- **Phase 2** — routing config + more languages (python, go, typescript,
  java with jdtls init options), the server registry and on-demand
  install, `search` (walker) / `symbols` (LSP).
- **Phase 3** — polish: code actions, workspace symbols, idle eviction
  tuning, optional Turso index behind the `IndexStore` seam if the walker
  proves too slow, benchmarks against the nvim tools.

## 8a. Prior art (research, 2026-10-01)

At least six LSP-to-MCP servers exist already. None matches the full goal set
(editing + file ops + no hang + token-efficient), but each has lessons:

| Project                       | What it does                          | What we take                                                                                                                                                                                     |
| ----------------------------- | ------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `beruang/lsp-mcp` (TS)        | Read-only code intelligence, 59 tools | Per-method timeout env vars; restart cap 3 per 60 s; structured error envelope. Caution: 59 tools is tool-selection bloat for the agent                                                          |
| `OpticLM/lsp-mcp` (TS/Effect) | Full headless editor around one LS    | Capability-aware tool registration (hide what the server cannot do); diagnostics also as MCP resources; LRU of open docs; builds on `vscode-languageserver-protocol`                             |
| `afriemann/lsp-mcp` (Python)  | Editing + navigation on multilspy     | multilspy is a vetted LSP client: UTF-16 offsets, lifecycle, and watchdog problems already solved; server pool keyed by `(server, root)`; global config at `~/.config/`; idle eviction at 15 min |
| `theUpsider/lsp-mcp`          | Zero-config, 13 languages             | Marker-based language detection (`package.json`, `go.mod`)                                                                                                                                       |
| `agent-lsp` (Go)              | Warm session + skill layer            | Fuzzy position fallback: a miss on line/col retries via symbol search instead of failing empty; speculative in-memory edits; 150 ms debounced watcher                                            |
| `lsproxy` (Rust)              | Multi-language proxy + tree-sitter    | Conformance test suite per language in CI                                                                                                                                                        |

Key recommendation: do not write the JSON-RPC layer by hand if a crate fits.
The hardest parts — JSON-RPC framing, UTF-16 offset math, server lifecycle
quirks — are solved problems. Our own value sits in the anti-hang
supervisor, the tool grammar, and the edit feedback loop. In Rust this means
`lsp-types` (protocol definitions and offset types) plus a thin client built

## 9. Open questions (updated after round 3)

Round 1 answered the big design questions. Round 2 (chat) settled the index:
no index in v1, LSP owns symbols, the walker owns text search. Round 3
settled deadlines: per-server configurable, published to the agent at
`initialize`. The remaining questions, in the order they block work:

1. **Rust LSP client crate.** `lsp-types` + a thin hand-rolled tokio client,
   or `tower-lsp`? Locked recommendation: `lsp-types` + own client; we need
   client-role behavior (we drive servers), while `tower-lsp` is
   server-role oriented.
2. **MCP SDK.** `rmcp` (official Rust SDK) or hand-rolled stdio? Locked
   recommendation: hand-rolled JSON-RPC stdio for the spike (fully under
   our control, fits the anti-hang rules); revisit `rmcp` before the first
   release.
3. **Phase 1 language.** Rust first (dogfood against ghostedit itself),
   then Python with pyright in Phase 2 to prove routing. Test target: a
   small sample project in C or Python.
4. **Registry source.** GitHub repo shape for the language-server registry,
   pinned to release tags with a shipped fallback snapshot inside the binary.
5. **Config precedence details.** Project `.ghostedit/config.toml` overrides
   global. Full replace per language entry (simpler to explain and debug).

## 9a. Answers locked in across review rounds

| Question                | Answer                                                                                                                                                                                                  |
| ----------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Implementation language | **Rust**, not JS/TS                                                                                                                                                                                     |
| Index in v1             | **None.** The LSP already maintains a symbol index; a local DB would duplicate it. Text search uses a built-in walker. Turso/libSQL sits behind an `IndexStore` seam for a later phase                  |
| Tool naming             | **No `ge_` prefix**; short simple names                                                                                                                                                                 |
| On-demand LSP install   | **Yes**: a registry shipped with the binary, refreshed from a GitHub repo, install on demand                                                                                                            |
| Per-server LSP config   | **Yes**: `initialization_options` + `settings` per language (jdtls-style configs supported)                                                                                                             |
| Deadlines               | **Configurable per language server** (`deadlines_ms` per entry); effective table published to the agent in the `initialize` response; agent-supplied timeouts clamp inside these ceilings (4.5.2–4.5.3) |
| Atomic writes           | **When the filesystem supports it**; otherwise plain write with a note                                                                                                                                  |
| Symbol search source    | **LSP-provided** (`workspace/symbol`, `documentSymbol`)                                                                                                                                                 |
| Text search source      | **Built-in walker**; no external tools (rg, etc.)                                                                                                                                                       |
| External dependencies   | **None**; Turso would run embedded if a later phase adopts it                                                                                                                                           |

## 10. Explicitly out of scope (v1)

- No Neovim. No plugins. No plugin manager warm-up hacks.
- No UI of any kind.
- No formatter chains beyond LSP.
- No embeddings/vector search — lexical + symbol search only.
- No local index DB; no external search tools (rg, etc.). The `IndexStore`
  seam exists, but v1 ships the no-op implementation.
- No remote Turso (cloud sync); moot until the index exists.
- No multi-workspace federation (single root per process; multi-root is v2).

## 11. Reviewer checklist

- [x] Round 1 comments applied (runtime, naming, install, config, writes)
- [x] Round 2 (chat): index dropped from v1 — LSP owns symbols, walker owns
      text; Turso deferred behind the `IndexStore` seam
- [x] Round 3 (chat): recommendations 9.1–9.5 approved; deadlines are
      per-server configurable and published at `initialize`
- [x] All open questions resolved — plan is ready for prototype execution
- [ ] Prototype gate: user gave go-ahead ("lfg") pending this update;
      Phase 0 + Phase 1 may start against a small C or Python sample project
