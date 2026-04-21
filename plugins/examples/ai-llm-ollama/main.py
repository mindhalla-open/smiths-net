#!/usr/bin/env python3
"""Ollama-backed `ai.llm.chat` sidecar — slice 3.1 reference.

Calls the local Ollama daemon's `/api/chat` endpoint and translates
the response into the shape this engine's `ai.llm.chat` contract
expects (see `docs/architecture/05-ai-plugin-protocol.md`).

Environment overrides:
  OLLAMA_HOST   — base URL (default `http://127.0.0.1:11434`)
  OLLAMA_MODEL  — model tag (default `llama3.2:3b`)

If the daemon is unreachable, `describe_capabilities` still returns
the descriptor so the dispatcher sees the candidate; `chat` returns a
JSON-RPC error and the dispatcher fails over to the next provider.
Stdlib-only; no extra deps.
"""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request

OLLAMA_HOST = os.environ.get("OLLAMA_HOST", "http://127.0.0.1:11434").rstrip("/")
OLLAMA_MODEL = os.environ.get("OLLAMA_MODEL", "llama3.2:3b")
REQUEST_TIMEOUT = float(os.environ.get("OLLAMA_TIMEOUT_SECS", "60"))

DESCRIPTOR = {
    "capability": "ai.llm.chat",
    "plugin": "ai-llm-ollama",
    "model_id": OLLAMA_MODEL,
    "abi": "1.0",
    "description": f"Ollama chat proxy ({OLLAMA_MODEL}).",
    # Lower number wins — prefer Ollama over the canned mock when both
    # are loaded in the same plugins dir.
    "priority": 20,
    "context_window": 8192,
    "max_output": 2048,
    "features": ["system_prompt"],
    "roles": ["system", "user", "assistant"],
    "streaming": {"supported": False},
    "controls": {
        "temperature": {
            "type": "number", "minimum": 0.0, "maximum": 2.0, "default": 0.7,
        },
        "max_tokens": {
            "type": "integer", "minimum": 1, "maximum": 2048, "default": 512,
        },
    },
    "latency_ms": {"p50": 400, "p95": 2000},
    "concurrency": {"max_in_flight": 2},
}


def reply(id_, *, result=None, error=None):
    frame = {"jsonrpc": "2.0"}
    if id_ is not None:
        frame["id"] = id_
    if error is not None:
        frame["error"] = error
    else:
        frame["result"] = result
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()


def log(msg: str) -> None:
    sys.stderr.write(f"[ai-llm-ollama] {msg}\n")
    sys.stderr.flush()


def ollama_chat(messages: list, controls: dict | None) -> dict:
    body = {
        "model": OLLAMA_MODEL,
        "messages": messages,
        "stream": False,
    }
    if controls:
        opts = {}
        if "temperature" in controls:
            opts["temperature"] = controls["temperature"]
        if "max_tokens" in controls:
            opts["num_predict"] = controls["max_tokens"]
        if opts:
            body["options"] = opts

    req = urllib.request.Request(
        f"{OLLAMA_HOST}/api/chat",
        data=json.dumps(body).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=REQUEST_TIMEOUT) as resp:
        raw = resp.read().decode("utf-8")
    return json.loads(raw)


def chat(params: dict) -> dict:
    messages = params.get("messages")
    if not isinstance(messages, list) or not messages:
        raise ValueError("`messages` must be a non-empty array")
    controls = params.get("controls") or {}
    try:
        data = ollama_chat(messages, controls)
    except urllib.error.URLError as e:
        raise RuntimeError(f"ollama unreachable at {OLLAMA_HOST}: {e}") from e

    msg = data.get("message") or {}
    content = msg.get("content", "")
    return {
        "message": {"role": "assistant", "content": content},
        "usage": {
            "input_tokens": data.get("prompt_eval_count", 0),
            "output_tokens": data.get("eval_count", 0),
        },
        "finish_reason": "stop" if data.get("done") else "length",
    }


def main() -> int:
    log(f"ready (host={OLLAMA_HOST} model={OLLAMA_MODEL})")
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError as e:
            reply(None, error={"code": -32700, "message": f"parse error: {e}"})
            continue

        id_ = req.get("id")
        method = req.get("method")

        if method == "describe_capabilities":
            reply(id_, result=[DESCRIPTOR])
        elif method == "chat":
            try:
                reply(id_, result=chat(req.get("params") or {}))
            except ValueError as e:
                reply(id_, error={"code": -32602, "message": str(e)})
            except Exception as e:  # noqa: BLE001
                reply(id_, error={"code": -32603, "message": f"chat failed: {e}"})
        elif method == "shutdown":
            reply(id_, result=None)
            log("shutdown — exiting")
            return 0
        elif method == "ping":
            reply(id_, result={"ok": True})
        else:
            reply(id_, error={"code": -32601, "message": f"method `{method}` not implemented"})
    return 0


if __name__ == "__main__":
    sys.exit(main())
