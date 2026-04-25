#!/usr/bin/env python3
"""
Subprocess test for canonical_hook.py — exercises the full
describe → invoke → shutdown lifecycle without the engine.

Run:
    python3 test_canonical_hook.py
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

SCRIPT = Path(__file__).parent / "canonical_hook.py"


def start() -> subprocess.Popen:
    """Spawn canonical_hook.py as a subprocess."""
    return subprocess.Popen(
        [sys.executable, str(SCRIPT)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )


def rpc(proc: subprocess.Popen, req_id: int, method: str, params: dict | None = None) -> dict:
    """Send one JSON-RPC request and read the response."""
    request = {
        "jsonrpc": "2.0",
        "id": req_id,
        "method": method,
        "params": params or {},
    }
    frame = json.dumps(request, separators=(",", ":")) + "\n"
    proc.stdin.write(frame)
    proc.stdin.flush()

    line = proc.stdout.readline()
    assert line, f"expected response for {method}, got EOF"
    return json.loads(line)


# ── Tests ──────────────────────────────────────────────

def test_describe_capabilities() -> None:
    proc = start()
    try:
        resp = rpc(proc, 1, "describe_capabilities")
        assert resp["id"] == 1, f"id mismatch: {resp}"
        result = resp["result"]
        assert result["name"] == "canonical-hook-py", f"bad name: {result}"
        assert result["version"] == "0.1.0", f"bad version: {result}"
        assert len(result["tools"]) == 1, f"expected 1 tool: {result}"
        tool = result["tools"][0]
        assert tool["name"] == "ai.summarize", f"bad tool name: {tool}"
        assert "text" in tool["parameters"]["required"], f"text not required: {tool}"
        print("✓ describe_capabilities")
    finally:
        proc.terminate()
        proc.wait()


def test_invoke_summarize() -> None:
    proc = start()
    try:
        resp = rpc(proc, 2, "invoke", {
            "tool": "ai.summarize",
            "args": {
                "text": "First sentence. Second sentence. Third sentence. Fourth sentence.",
                "max_sentences": 2,
            },
        })
        assert resp["id"] == 2, f"id mismatch: {resp}"
        summary = resp["result"]["summary"]
        assert "First sentence" in summary, f"missing first: {summary}"
        assert "Second sentence" in summary, f"missing second: {summary}"
        assert "Third sentence" not in summary, f"should be truncated: {summary}"
        print("✓ invoke ai.summarize")
    finally:
        proc.terminate()
        proc.wait()


def test_invoke_summarize_defaults() -> None:
    proc = start()
    try:
        resp = rpc(proc, 3, "invoke", {
            "tool": "ai.summarize",
            "args": {"text": "A. B. C. D. E."},
        })
        assert resp["id"] == 3
        summary = resp["result"]["summary"]
        # Default max_sentences=3, so we get A, B, C
        assert summary.count(".") >= 3, f"expected 3 sentences: {summary}"
        print("✓ invoke ai.summarize (defaults)")
    finally:
        proc.terminate()
        proc.wait()


def test_invoke_unknown_tool() -> None:
    proc = start()
    try:
        resp = rpc(proc, 4, "invoke", {"tool": "nonexistent"})
        assert resp["id"] == 4
        assert "error" in resp, f"expected error: {resp}"
        assert resp["error"]["code"] == -32601, f"wrong error code: {resp}"
        print("✓ invoke unknown tool → error")
    finally:
        proc.terminate()
        proc.wait()


def test_invoke_missing_tool_param() -> None:
    proc = start()
    try:
        resp = rpc(proc, 5, "invoke", {"args": {}})
        assert resp["id"] == 5
        assert "error" in resp, f"expected error: {resp}"
        assert resp["error"]["code"] == -32602, f"wrong error code: {resp}"
        print("✓ invoke missing tool param → error")
    finally:
        proc.terminate()
        proc.wait()


def test_unknown_method() -> None:
    proc = start()
    try:
        resp = rpc(proc, 6, "nonexistent_method")
        assert resp["id"] == 6
        assert "error" in resp, f"expected error: {resp}"
        assert resp["error"]["code"] == -32601, f"wrong error code: {resp}"
        print("✓ unknown method → error")
    finally:
        proc.terminate()
        proc.wait()


def test_shutdown() -> None:
    proc = start()
    resp = rpc(proc, 7, "shutdown")
    assert resp["id"] == 7
    assert resp["result"]["ok"] is True, f"bad shutdown: {resp}"
    rc = proc.wait(timeout=5)
    assert rc == 0, f"expected exit 0, got {rc}"
    print("✓ shutdown")


def test_full_lifecycle() -> None:
    """describe → invoke → shutdown in one session."""
    proc = start()

    # 1. describe
    r1 = rpc(proc, 10, "describe_capabilities")
    assert r1["result"]["tools"][0]["name"] == "ai.summarize"

    # 2. invoke
    r2 = rpc(proc, 11, "invoke", {
        "tool": "ai.summarize",
        "args": {"text": "Hello world. Goodbye world."},
    })
    assert "Hello world" in r2["result"]["summary"]

    # 3. shutdown
    r3 = rpc(proc, 12, "shutdown")
    assert r3["result"]["ok"] is True
    assert proc.wait(timeout=5) == 0

    print("✓ full lifecycle (describe → invoke → shutdown)")


if __name__ == "__main__":
    test_describe_capabilities()
    test_invoke_summarize()
    test_invoke_summarize_defaults()
    test_invoke_unknown_tool()
    test_invoke_missing_tool_param()
    test_unknown_method()
    test_shutdown()
    test_full_lifecycle()
    print("\nAll tests passed.")
