#!/usr/bin/env python3
"""Reference sidecar — `ai.llm.chat` capability, canned completions.

Implements `describe_capabilities` + `chat` per
`docs/architecture/05-ai-plugin-protocol.md`. The stub keeps the same
response shape a real provider (Ollama / llama.cpp / OpenAI / Anthropic)
would return — agent code written against this plugin runs unchanged
against a real model once swapped in.
"""

from __future__ import annotations

import json
import sys

DESCRIPTOR = {
    "capability": "ai.llm.chat",
    "plugin": "ai-llm-mock",
    "model_id": "mock-llm-v1",
    "abi": "1.0",
    "description": "Canned LLM stub used for protocol wiring demos.",
    "context_window": 4096,
    "max_output": 512,
    "features": ["system_prompt"],
    "roles": ["system", "user", "assistant"],
    "streaming": {"supported": False},
    "controls": {
        "temperature": {
            "type": "number",
            "minimum": 0.0,
            "maximum": 2.0,
            "default": 0.7,
        },
        "max_tokens": {
            "type": "integer",
            "minimum": 1,
            "maximum": 512,
            "default": 128,
        },
    },
    "latency_ms": {"p50": 20, "p95": 80},
    "concurrency": {"max_in_flight": 4},
}

# Very small canned-response rule table. Replace with a real LLM call
# in a production plugin.
CANNED = [
    ("silence", "Алло? Я вас не слышу."),
    ("привет",  "Здравствуйте! Чем могу помочь?"),
    ("hello",   "Hello, how can I help you today?"),
]
DEFAULT_REPLY = "Алло, Алиса слушает вас"


def reply(id_: int | None, *, result=None, error=None) -> None:
    frame: dict = {"jsonrpc": "2.0"}
    if id_ is not None:
        frame["id"] = id_
    if error is not None:
        frame["error"] = error
    else:
        frame["result"] = result
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()


def log(msg: str) -> None:
    sys.stderr.write(f"[ai-llm-mock] {msg}\n")
    sys.stderr.flush()


def fake_chat(params: dict) -> dict:
    messages = params.get("messages")
    if not isinstance(messages, list) or not messages:
        raise ValueError("`messages` must be a non-empty array")
    # Use the last user turn as the 'prompt' for our canned lookup.
    last_user = next(
        (m for m in reversed(messages) if m.get("role") == "user"),
        None,
    )
    prompt = (last_user or {}).get("content", "") if last_user else ""
    text = DEFAULT_REPLY
    low = prompt.lower()
    for needle, resp in CANNED:
        if needle in low:
            text = resp
            break
    return {
        "message": {"role": "assistant", "content": text},
        "usage": {
            "input_tokens": sum(len((m.get("content") or "").split()) for m in messages),
            "output_tokens": len(text.split()),
        },
        "finish_reason": "stop",
    }


def main() -> int:
    log("ready")
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
                reply(id_, result=fake_chat(req.get("params") or {}))
            except ValueError as e:
                reply(id_, error={"code": -32602, "message": str(e)})
            except Exception as e:  # noqa: BLE001
                reply(id_, error={"code": -32603, "message": f"chat failed: {e}"})
        elif method == "shutdown":
            reply(id_, result=None)
            log("shutdown requested — exiting")
            return 0
        elif method == "ping":
            reply(id_, result={"ok": True})
        else:
            reply(id_, error={"code": -32601, "message": f"method `{method}` not implemented"})
    return 0


if __name__ == "__main__":
    sys.exit(main())
