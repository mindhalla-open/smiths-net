#!/usr/bin/env python3
"""llama.cpp server `ai.llm.chat` sidecar — local Gemma/Llama models.

Talks to `llama-server` (or any OpenAI-compatible local endpoint) via
`/v1/chat/completions`. Fully offline once the GGUF model is loaded.

Quick start (Gemma 3 4B):
  # Download a quantised Gemma GGUF, e.g. from HuggingFace:
  #   gemma-3-4b-it-Q4_K_M.gguf
  llama-server -m /path/to/gemma-3-4b-it-Q4_K_M.gguf \\
               --host 127.0.0.1 --port 8080 \\
               -c 4096 -ngl 99

Environment:
  LLAMACPP_HOST         — base URL (default `http://127.0.0.1:8080`)
  LLAMACPP_MODEL        — model name sent in API body (default `gemma`)
  LLAMACPP_API_KEY      — optional Bearer token (llama-server --api-key)
  LLAMACPP_TIMEOUT_SECS — request timeout (default 120)

Stdlib-only; no extra Python deps.
"""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request

LLAMACPP_HOST = os.environ.get("LLAMACPP_HOST", "http://127.0.0.1:8080").rstrip("/")
LLAMACPP_MODEL = os.environ.get("LLAMACPP_MODEL", "gemma")
LLAMACPP_API_KEY = os.environ.get("LLAMACPP_API_KEY", "")
REQUEST_TIMEOUT = float(os.environ.get("LLAMACPP_TIMEOUT_SECS", "120"))

DESCRIPTOR = {
    "capability": "ai.llm.chat",
    "plugin": "ai-llm-llamacpp",
    "model_id": LLAMACPP_MODEL,
    "abi": "1.0",
    "description": f"llama.cpp server chat proxy ({LLAMACPP_MODEL}).",
    # Prefer over Ollama (20) and mock (50); below GigaChat cloud (12).
    "priority": 18,
    "context_window": 8192,
    "max_output": 2048,
    "features": ["system_prompt"],
    "roles": ["system", "user", "assistant"],
    "streaming": {"supported": False},
    "controls": {
        "temperature": {
            "type": "number", "minimum": 0.0, "maximum": 2.0, "default": 0.3,
        },
        "max_tokens": {
            "type": "integer", "minimum": 1, "maximum": 2048, "default": 256,
        },
    },
    "latency_ms": {"p50": 800, "p95": 4000},
    "concurrency": {"max_in_flight": 1},
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
    sys.stderr.write(f"[ai-llm-llamacpp] {msg}\n")
    sys.stderr.flush()


def _headers() -> dict:
    h = {"Content-Type": "application/json"}
    if LLAMACPP_API_KEY:
        h["Authorization"] = f"Bearer {LLAMACPP_API_KEY}"
    return h


def llamacpp_chat(messages: list, controls: dict | None) -> dict:
    body: dict = {
        "model": LLAMACPP_MODEL,
        "messages": messages,
        "stream": False,
    }
    if controls:
        if "temperature" in controls:
            body["temperature"] = controls["temperature"]
        if "max_tokens" in controls:
            body["max_tokens"] = controls["max_tokens"]

    req = urllib.request.Request(
        f"{LLAMACPP_HOST}/v1/chat/completions",
        data=json.dumps(body).encode("utf-8"),
        headers=_headers(),
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=REQUEST_TIMEOUT) as resp:
            raw = resp.read().decode("utf-8")
    except urllib.error.HTTPError as e:
        detail = e.read().decode("utf-8", errors="replace")[:500]
        raise RuntimeError(
            f"llama.cpp HTTP {e.code} at {LLAMACPP_HOST}: {detail}"
        ) from e
    return json.loads(raw)


def chat(params: dict) -> dict:
    messages = params.get("messages")
    if not isinstance(messages, list) or not messages:
        raise ValueError("`messages` must be a non-empty array")
    controls = params.get("controls") or {}
    try:
        data = llamacpp_chat(messages, controls)
    except urllib.error.HTTPError:
        raise  # surfaced with body in llamacpp_chat()
    except urllib.error.URLError as e:
        raise RuntimeError(
            f"llama.cpp server unreachable at {LLAMACPP_HOST}: {e}. "
            "Start: bash examples/start-llamacpp.sh"
        ) from e

    choice = (data.get("choices") or [{}])[0]
    message = choice.get("message") or {}
    usage = data.get("usage") or {}
    return {
        "message": {
            "role": message.get("role", "assistant"),
            "content": message.get("content", ""),
        },
        "usage": {
            "input_tokens": usage.get("prompt_tokens", 0),
            "output_tokens": usage.get("completion_tokens", 0),
        },
        "finish_reason": choice.get("finish_reason", "stop"),
    }


def main() -> int:
    log(f"ready (host={LLAMACPP_HOST} model={LLAMACPP_MODEL})")
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
