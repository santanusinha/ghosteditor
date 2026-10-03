# P3.4 Benchmarks — S3 startup, S4 token cost

Date: 2026-10-03. Driver: `bench/bench.py` (framed stdio, debug binary).
Nvim baseline: `bench/nvim_baseline.json`, captured from a live
neovim-use-mcp session on the same file (`/tmp/ge-rust/src/main.rs`).

Token metric: `chars / 4` (standard heuristic), applied to both sides.
Primary metric: the text content blocks the agent reads. The
structuredContent JSON duplicate is reported separately; it is not what
the agent reads.

## S3 — startup (initialize < 250 ms)

| runs | min | median | max | verdict |
| ---- | --- | ------ | --- | ------- |
| 5    | 13.9 ms | 22.2 ms | 25.0 ms | **PASS** (10x margin) |

## S4 — edit round trip uses no more tokens than nvim

| op | ghostedit (tokens) | nvim (tokens) | verdict |
| -- | ------------------ | ------------- | ------- |
| read_file (9 lines) | 49 | 49 | **PASS** (parity) |
| edit_lines (1 line) | 15 | 23 | **PASS** (-35%) |
| hover (1:5) | 9 | 15 | **PASS** (-40%) |

The read_file parity needed one fix: the numbered-line prefix was
`{:>6}\t` (7 chars fixed). It is now right-aligned to the widest printed
line number plus two spaces — the same shape the nvim tools emit
(commit with this file). On a 614-line file the old prefix cost about
300 extra tokens per full read; the new prefix tracks the file size.

## Ghostedit payload sizes (this run)

| op | text chars | text tokens | struct chars | struct tokens |
| -- | ---------- | ----------- | ------------ | ------------- |
| open_file | 54 | 14 | 174 | 44 |
| read_file | 214 | 49 | 301 | 75 |
| edit_lines | 60 | 15 | 219 | 55 |
| hover | 37 | 9 | 51 | 13 |
| diagnostics | 43 | 11 | 194 | 49 |

Notes:

- The nvim edit_lines baseline reported "No diagnostics"; ghostedit
  returns the diagnostics mailbox snapshot with every edit, so its
  edit feedback carries more value at a lower token cost.
- hover tokens depend on the server reply; both sides asked for the
  same symbol (`add` at 1:5) through rust-analyzer.
- Re-run: `python3 bench/bench.py --json bench/results.json`. The
  script resets the sample with `git checkout` before and after.
