#!/usr/bin/env python3
"""
Canonical sidecar plugin for Smiths-Net.

Implements the full JSON-RPC 2.0 sidecar protocol:
  - describe_capabilities  → declare tools
  - invoke                 → execute a tool by name
  - shutdown               → clean exit

Exposes a single tool: ai.summarize
"""

from __future__ import annotations

import json
import sys
from typing import Any


# ── JSON-RPC helpers ────────────────────────────────────

def send(obj: dict[str, Any]) -> None:
    """Write one JSON-RPC frame + newline to stdout, then flush."""
    sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def respond(req_id: int, result: Any) -> None:
    """Send a JSON-RPC success response."""
    send({"jsonrpc": "2.0", "id": req_id, "result": result})


def respond_error(req_id: int, code: int, message: str) -> None:
    """Send a JSON-RPC error response."""
    send({
        "jsonrpc": "2.0",
        "id": req_id,
        "error": {"code": code, "message": message},
    })


# ── Tool descriptors ───────────────────────────────────

TOOLS = [
    {
        "name": "ai.summarize",
        "description": "Summarize a block of text into a few sentences.",
        "parameters": {
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "The text to summarize.",
                },
                "max_sentences": {
                    "type": "integer",
                    "description": "Maximum sentences in the summary.",
                    "default": 3,
                },
            },
            "required": ["text"],
        },
    }
]

# Build a fast lookup: tool name → descriptor
_TOOL_MAP: dict[str, dict] = {t["name"]: t for t in TOOLS}


# ── Tool implementations ───────────────────────────────

def summarize(text: str, max_sentences: int = 3) -> str:
    """Naive extractive summariser — split on period, take first N."""
    sentences = [s.strip() for s in text.split(".") if s.strip()]
    if not sentences:
        return ""
    return ". ".join(sentences[:max_sentences]) + "."


# ── RPC handlers ───────────────────────────────────────

def handle_describe_capabilities(req_id: int, _params: Any) -> None:
    """Return plugin metadata and the list of tools it provides."""
    respond(req_id, {
        "name": "canonical-hook-py",
        "version": "0.1.0",
        "tools": TOOLS,
    })


def handle_invoke(req_id: int, params: Any) -> None:
    """Route to the correct tool implementation by name."""
    if not isinstance(params, dict):
        respond_error(req_id, -32602, "params must be an object")
        return

    tool_name = params.get("tool")
    if not tool_name:
        respond_error(req_id, -32602, "missing 'tool' in params")
        return

    if tool_name not in _TOOL_MAP:
        respond_error(req_id, -32601, f"unknown tool: {tool_name}")
        return

    args = params.get("args", {})

    if tool_name == "ai.summarize":
        text = args.get("text", "")
        max_sentences = args.get("max_sentences", 3)
        summary = summarize(text, max_sentences)
        respond(req_id, {"summary": summary})
    else:
        respond_error(req_id, -32601, f"unimplemented tool: {tool_name}")


def handle_shutdown(req_id: int, _params: Any) -> None:
    """Acknowledge and exit cleanly."""
    respond(req_id, {"ok": True})
    sys.exit(0)


# ── Dispatch table ─────────────────────────────────────

HANDLERS: dict[str, Any] = {
    "describe_capabilities": handle_describe_capabilities,
    "invoke": handle_invoke,
    "shutdown": handle_shutdown,
}


# ── Main loop ──────────────────────────────────────────

def main() -> None:
    """Read JSON-RPC requests from stdin, dispatch each one."""
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue

        try:
            request = json.loads(line)
        except json.JSONDecodeError:
            print(f"bad frame: {line!r}", file=sys.stderr)
            continue

        req_id = request.get("id")
        method = request.get("method", "")

        handler = HANDLERS.get(method)
        if handler:
            handler(req_id, request.get("params", {}))
        else:
            respond_error(
                req_id, -32601,
                f"method not found: {method}",
            )


if __name__ == "__main__":
    main()
