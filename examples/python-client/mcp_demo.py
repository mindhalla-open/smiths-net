#!/usr/bin/env python3
"""Minimal MCP stdio client demo against a smiths-net engine.

Spawns the engine in MCP-stdio mode, runs the JSON-RPC handshake,
lists tools, and calls a few. Pure stdlib — no `mcp` SDK required.

Usage:
    # From the repo root:
    python3 examples/python-client/mcp_demo.py

The demo launches the release binary
(`./target/release/smiths-net --mcp stdio`). Build it first:

    cargo build --release

This demonstrates the exact wire an LLM host (Claude Code, Cursor,
etc.) uses when it spawns smiths-net as an MCP subprocess.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path


class McpStdioClient:
    """One request-one response JSON-RPC 2.0 client over a child's stdio."""

    def __init__(self, cmd: list[str]) -> None:
        self.proc = subprocess.Popen(
            cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
        )
        self._next_id = 0

    def _send(
        self, method: str, params: dict | None = None, *, notification: bool = False
    ) -> dict | None:
        frame: dict = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            frame["params"] = params
        if not notification:
            self._next_id += 1
            frame["id"] = self._next_id
        assert self.proc.stdin is not None
        self.proc.stdin.write((json.dumps(frame) + "\n").encode())
        self.proc.stdin.flush()
        if notification:
            return None
        assert self.proc.stdout is not None
        line = self.proc.stdout.readline()
        if not line:
            raise RuntimeError("MCP server closed stdout unexpectedly")
        return json.loads(line)

    def initialize(self) -> dict:
        return self._send(
            "initialize",
            {
                "protocolVersion": "2024-11-05",
                "clientInfo": {"name": "mcp_demo.py", "version": "0.1"},
                "capabilities": {},
            },
        )

    def initialized(self) -> None:
        self._send("notifications/initialized", {}, notification=True)

    def list_tools(self) -> dict:
        return self._send("tools/list")

    def call_tool(self, name: str, args: dict | None = None) -> dict:
        return self._send("tools/call", {"name": name, "arguments": args or {}})

    def close(self) -> None:
        if self.proc.stdin:
            self.proc.stdin.close()
        self.proc.wait(timeout=2)


def pretty(label: str, payload) -> None:
    print(f"\n=== {label} ===")
    print(json.dumps(payload, indent=2, ensure_ascii=False))


def locate_binary() -> Path:
    """Find the release binary relative to the repo root (CWD convention)."""
    for candidate in (
        Path("target/release/smiths-net"),
        Path("target/debug/smiths-net"),
    ):
        if candidate.exists():
            return candidate
    raise SystemExit(
        "smiths-net binary not found. Build it first: `cargo build --release`."
    )


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--bin",
        type=Path,
        help="Path to smiths-net binary (auto-detected if omitted).",
    )
    ap.add_argument(
        "--config",
        default="examples/config.toml",
        help="TOML config used by the engine (even in MCP mode it reads one).",
    )
    args = ap.parse_args()

    binary = args.bin or locate_binary()
    cmd = [str(binary), "--config", args.config, "--mcp", "stdio"]
    print(f"spawning: {' '.join(cmd)}")

    client = McpStdioClient(cmd)
    try:
        pretty("initialize", client.initialize())
        client.initialized()

        pretty("tools/list", client.list_tools())
        pretty("health", client.call_tool("health"))
        pretty("list_calls", client.call_tool("list_calls"))
        pretty(
            "get_call_status (missing id — expects isError=true)",
            client.call_tool("get_call_status", {"call_id": "does-not-exist"}),
        )
    finally:
        client.close()
        # Drain stderr so engine log output is visible when debugging.
        if client.proc.stderr:
            tail = client.proc.stderr.read().decode(errors="replace")
            if tail.strip():
                print("\n--- engine stderr (tail) ---")
                print(tail)

    return 0


if __name__ == "__main__":
    sys.exit(main())
