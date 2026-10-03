#!/usr/bin/env python3
"""P3.4 benchmarks: S3 startup (< 250 ms initialize) and S4 token cost
vs the neovim-use-mcp baseline captured in nvim_baseline.json.

Token metric: characters / 4 (standard heuristic), applied to both sides.
Primary metric (S4): the text content blocks the agent reads.
Secondary: the serialized structuredContent payload (JSON duplicate).

Usage: python3 bench/bench.py [--json OUT]
"""

import json
import os
import statistics
import subprocess
import sys
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(REPO, "target", "debug", "ghostedit")
CWD = "/tmp/ge-rust"
BASELINE = os.path.join(REPO, "bench", "nvim_baseline.json")


def frame(msg):
    body = json.dumps(msg).encode()
    return b"Content-Length: %d\r\n\r\n" % len(body) + body


class Session:
    def __init__(self, cwd):
        self.p = subprocess.Popen(
            [BIN, "--cwd", cwd],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            cwd=cwd,
        )
        self.buf = b""

    def _frame(self, timeout):
        end = time.time() + timeout
        while True:
            i = self.buf.find(b"\r\n\r\n")
            if i >= 0:
                head = self.buf[:i].decode()
                for line in head.splitlines():
                    k, _, v = line.partition(":")
                    if k.strip() == "Content-Length":
                        n = int(v.strip())
                        if len(self.buf) >= i + 4 + n:
                            body = self.buf[i + 4 : i + 4 + n]
                            self.buf = self.buf[i + 4 + n :]
                            return json.loads(body)
            if time.time() > end:
                raise TimeoutError("no frame")
            chunk = os.read(self.p.stdout.fileno(), 65536)
            if chunk:
                self.buf += chunk
                continue
            time.sleep(0.005)

    def reply(self, want_id, timeout=20):
        end = time.time() + timeout
        while True:
            msg = self._frame(max(0.01, end - time.time()))
            if msg.get("id") == want_id:
                return msg

    def send(self, msg):
        self.p.stdin.write(frame(msg))
        self.p.stdin.flush()

    def call(self, i, name, args, timeout=20):
        self.send(
            {
                "jsonrpc": "2.0",
                "id": i,
                "method": "tools/call",
                "params": {"name": name, "arguments": args},
            }
        )
        return self.reply(i, timeout)

    def close(self):
        self.p.stdin.close()
        try:
            self.p.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.p.kill()


def tokens(text):
    return int(len(text) / 4 + 0.5)


def agent_text(reply):
    """The text the agent reads: concatenated text content blocks."""
    blocks = reply["result"].get("content") or []
    return "\n".join(b.get("text", "") for b in blocks if b.get("type") == "text")


def structured(reply):
    return reply["result"].get("structuredContent")


def initialize(s):
    s.send(
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "bench", "version": "0"},
            },
        }
    )
    s.reply(1, 10)
    s.send({"jsonrpc": "2.0", "method": "notifications/initialized"})


def bench_startup(runs=5):
    times = []
    for _ in range(runs):
        s = Session(CWD)
        t0 = time.perf_counter()
        initialize(s)
        times.append((time.perf_counter() - t0) * 1000)
        s.close()
    return {
        "runs": runs,
        "min_ms": round(min(times), 1),
        "median_ms": round(statistics.median(times), 1),
        "max_ms": round(max(times), 1),
        "s3_pass": max(times) < 250,
    }


def bench_tokens():
    # Reset the sample so every run sees the same input.
    subprocess.run(["git", "checkout", "src/main.rs"], cwd=CWD, capture_output=True)
    s = Session(CWD)
    initialize(s)

    calls = [
        ("open_file", {"path": "src/main.rs"}),
        ("read_file", {"path": "src/main.rs"}),
        ("edit_lines", {"path": "src/main.rs", "start_line": 2, "end_line": 2, "text": "    a + b // edited"}),
        ("hover", {"path": "src/main.rs", "line": 1, "column": 5}),
        ("diagnostics", {"path": "src/main.rs"}),
    ]
    sizes = {}
    for i, (name, args) in enumerate(calls, start=2):
        reply = s.call(i, name, args)
        text = agent_text(reply)
        entry = {"chars_text": len(text), "tokens_text": tokens(text)}
        struct = structured(reply)
        if struct is not None:
            blob = json.dumps(struct)
            entry["chars_struct"] = len(blob)
            entry["tokens_struct"] = tokens(blob)
        sizes[name] = entry
    s.close()
    subprocess.run(["git", "checkout", "src/main.rs"], cwd=CWD, capture_output=True)
    return sizes


def compare(sizes):
    with open(BASELINE) as f:
        baseline = json.load(f)
    rows = []
    for name, nvim_text in sorted(baseline.items()):
        if name not in sizes:
            continue
        ge = sizes[name]
        nv_tokens = tokens(nvim_text)
        rows.append(
            {
                "op": name,
                "ghostedit_tokens": ge["tokens_text"],
                "nvim_tokens": nv_tokens,
                "s4_pass": ge["tokens_text"] <= nv_tokens,
            }
        )
    return rows


def main():
    report = {}
    report["s3_startup"] = bench_startup()
    sizes = bench_tokens()
    report["ghostedit_payloads"] = sizes
    if os.path.exists(BASELINE):
        report["s4_comparison"] = compare(sizes)
    print(json.dumps(report, indent=2))
    if "--json" in sys.argv:
        path = sys.argv[sys.argv.index("--json") + 1]
        with open(path, "w") as f:
            json.dump(report, f, indent=2)


if __name__ == "__main__":
    main()
