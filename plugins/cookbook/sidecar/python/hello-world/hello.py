#!/usr/bin/env python3
"""
Minimal sidecar plugin for Smiths-Net.

Speaks JSON-RPC 2.0 over stdin/stdout (newline-delimited).
The engine sends requests; this script replies.

Protocol:
  Engine → Plugin:  {"jsonrpc":"2.0","id":1,"method":"...","params":{}}
  Plugin → Engine:  {"jsonrpc":"2.0","id":1,"result":{...}}
"""

from __future__ import annotations

import json
import sys
from typing import Any


def send(obj: dict[str, Any]) -> None:
    """Write one JSON-RPC frame + newline to stdout, then flush."""
    sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def respond(req_id: int, result: Any) -> None:
    """Send a success response for the given request id."""
    send({"jsonrpc": "2.0", "id": req_id, "result": result})


def respond_error(req_id: int, code: int, message: str) -> None:
    """Send an error response for the given request id."""
    send({"jsonrpc": "2.0", "id": req_id, "error": {"code": code, "message": message}})


def handle(request: dict[str, Any]) -> None:
    """Dispatch one incoming JSON-RPC request."""
    req_id = request.get("id")
    method = request.get("method", "")

    if method == "describe_capabilities":
        respond(req_id, {
            "name": "hello-world-py",
            "version": "0.1.0",
            "capabilities": [],
        })
    elif method == "shutdown":
        respond(req_id, {"ok": True})
        sys.exit(0)
    else:
        # Echo the method name back — proof the plugin is alive.
        respond(req_id, {"echo": method})


def main() -> None:
    """Read JSON-RPC requests from stdin, dispatch each one."""
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
        except json.JSONDecodeError:
            # Bad frame — skip silently (engine logs stderr).
            print(f"bad frame: {line!r}", file=sys.stderr)
            continue
        handle(request)


if __name__ == "__main__":
    main()
