#!/usr/bin/env python3
"""A2A (agent-to-agent) HTTP client demo against a smiths-net engine.

Talks to the engine's A2A adapter over plain HTTP + JSON-RPC. Fetches
the agent card, lists tools, and invokes a few — all with only
`urllib`.

Run the engine with A2A enabled:

    cat > tmp/a2a.toml <<EOF
    [observability]
    health_bind = "127.0.0.1:8080"
    [sip]
    bind = ["127.0.0.1:5060"]
    [a2a]
    enabled = true
    bind = "127.0.0.1:7879"
    EOF

    ./target/release/smiths-net --config tmp/a2a.toml

Then:

    python3 examples/python-client/a2a_demo.py
"""

from __future__ import annotations

import argparse
import json
import sys
import urllib.error
import urllib.request


class A2aClient:
    """JSON-RPC 2.0 over HTTP POST, `urllib`-only."""

    def __init__(self, base_url: str) -> None:
        self.base_url = base_url.rstrip("/")
        self._next_id = 0

    def _post(self, path: str, body: dict) -> dict:
        data = json.dumps(body).encode()
        req = urllib.request.Request(
            self.base_url + path,
            data=data,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        with urllib.request.urlopen(req, timeout=5) as resp:
            return json.loads(resp.read())

    def _get(self, path: str) -> dict:
        req = urllib.request.Request(self.base_url + path, method="GET")
        with urllib.request.urlopen(req, timeout=5) as resp:
            return json.loads(resp.read())

    def _rpc(self, method: str, params: dict | None = None) -> dict:
        self._next_id += 1
        return self._post(
            "/a2a",
            {
                "jsonrpc": "2.0",
                "id": self._next_id,
                "method": method,
                "params": params or {},
            },
        )

    def agent_card(self) -> dict:
        return self._get("/.well-known/agent.json")

    def list_tools(self) -> dict:
        return self._rpc("tools/list")

    def call_tool(self, name: str, args: dict | None = None) -> dict:
        return self._rpc("tools/call", {"name": name, "arguments": args or {}})


def pretty(label: str, payload) -> None:
    print(f"\n=== {label} ===")
    print(json.dumps(payload, indent=2, ensure_ascii=False))


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--url", default="http://127.0.0.1:7879", help="Base URL of the A2A endpoint."
    )
    args = ap.parse_args()

    client = A2aClient(args.url)

    try:
        pretty("agent card (/.well-known/agent.json)", client.agent_card())
        pretty("tools/list", client.list_tools())
        pretty("health", client.call_tool("health"))
        pretty("list_calls", client.call_tool("list_calls"))
        pretty(
            "get_call_status (missing id — expects error frame)",
            client.call_tool("get_call_status", {"call_id": "does-not-exist"}),
        )
    except urllib.error.URLError as e:
        print(f"HTTP error — is the engine running with [a2a].enabled=true? {e}")
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
