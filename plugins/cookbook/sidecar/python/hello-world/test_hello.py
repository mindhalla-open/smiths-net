#!/usr/bin/env python3
"""
Subprocess test for hello.py — exercises the stdin/stdout JSON-RPC
contract without the Smiths-Net engine.

Run:
    python3 test_hello.py
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

SCRIPT = Path(__file__).parent / "hello.py"


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


def test_describe_capabilities() -> None:
    proc = subprocess.Popen(
        [sys.executable, str(SCRIPT)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )

    try:
        resp = rpc(proc, 1, "describe_capabilities")
        assert resp["id"] == 1, f"id mismatch: {resp}"
        assert "result" in resp, f"missing result: {resp}"
        result = resp["result"]
        assert result["name"] == "hello-world-py", f"bad name: {result}"
        assert result["version"] == "0.1.0", f"bad version: {result}"
        print("✓ describe_capabilities")
    finally:
        proc.terminate()
        proc.wait()


def test_echo_unknown_method() -> None:
    proc = subprocess.Popen(
        [sys.executable, str(SCRIPT)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )

    try:
        resp = rpc(proc, 2, "some_custom_method")
        assert resp["id"] == 2, f"id mismatch: {resp}"
        assert resp["result"]["echo"] == "some_custom_method", f"bad echo: {resp}"
        print("✓ echo unknown method")
    finally:
        proc.terminate()
        proc.wait()


def test_shutdown() -> None:
    proc = subprocess.Popen(
        [sys.executable, str(SCRIPT)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )

    resp = rpc(proc, 3, "shutdown")
    assert resp["id"] == 3, f"id mismatch: {resp}"
    assert resp["result"]["ok"] is True, f"bad shutdown result: {resp}"

    # Process should exit cleanly after shutdown.
    rc = proc.wait(timeout=5)
    assert rc == 0, f"expected exit 0, got {rc}"
    print("✓ shutdown")


def test_multiple_requests() -> None:
    proc = subprocess.Popen(
        [sys.executable, str(SCRIPT)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )

    try:
        r1 = rpc(proc, 10, "describe_capabilities")
        assert r1["id"] == 10
        r2 = rpc(proc, 11, "ping")
        assert r2["id"] == 11
        assert r2["result"]["echo"] == "ping"
        r3 = rpc(proc, 12, "another")
        assert r3["id"] == 12
        assert r3["result"]["echo"] == "another"
        print("✓ multiple sequential requests")
    finally:
        proc.terminate()
        proc.wait()


if __name__ == "__main__":
    test_describe_capabilities()
    test_echo_unknown_method()
    test_shutdown()
    test_multiple_requests()
    print("\nAll tests passed.")
