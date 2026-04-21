#!/usr/bin/env python3
"""Anthropic-backed `ai.llm.chat` sidecar — slice 3.2 reference.

Translates the engine's `ai.llm.chat` contract onto Anthropic's
`/v1/messages` API. `system` messages collapse into the top-level
`system` field; the rest ride in the `messages` array. Streaming
emits one `ai.llm.partial` notification per `content_block_delta`
event; the final RPC response carries the full assembled message +
input/output token usage.

Environment:
  ANTHROPIC_API_KEY      — required for any `chat` call; descriptor
                           still loads without it so operators see
                           the provider in `list_ai_providers`.
  ANTHROPIC_API_BASE     — default `https://api.anthropic.com`
  ANTHROPIC_MODEL        — default `claude-3-5-haiku-latest`
  ANTHROPIC_VERSION      — default `2023-06-01` (wire header)
  ANTHROPIC_MAX_TOKENS   — default 1024 (required by the API)
  ANTHROPIC_TIMEOUT_SECS — default 60

Stdlib-only; no `anthropic` package dependency.
"""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request

ANTHROPIC_API_KEY = os.environ.get("ANTHROPIC_API_KEY", "")
ANTHROPIC_API_BASE = os.environ.get("ANTHROPIC_API_BASE", "https://api.anthropic.com").rstrip("/")
ANTHROPIC_MODEL = os.environ.get("ANTHROPIC_MODEL", "claude-3-5-haiku-latest")
ANTHROPIC_VERSION = os.environ.get("ANTHROPIC_VERSION", "2023-06-01")
ANTHROPIC_DEFAULT_MAX_TOKENS = int(os.environ.get("ANTHROPIC_MAX_TOKENS", "1024"))
REQUEST_TIMEOUT = float(os.environ.get("ANTHROPIC_TIMEOUT_SECS", "60"))


DESCRIPTOR = {
    "capability": "ai.llm.chat",
    "plugin": "ai-llm-anthropic",
    "model_id": ANTHROPIC_MODEL,
    "abi": "1.0",
    "description": f"Anthropic Messages proxy ({ANTHROPIC_MODEL}).",
    # One step behind OpenAI so operators can A/B by picking either
    # as primary; still ahead of Ollama and the mock.
    "priority": 16,
    "context_window": 200_000,
    "max_output": 8192,
    "features": ["system_prompt"],
    "roles": ["system", "user", "assistant"],
    "streaming": {"supported": True, "event": "ai.llm.partial"},
    "controls": {
        "temperature": {
            "type": "number", "minimum": 0.0, "maximum": 1.0, "default": 1.0,
        },
        "max_tokens": {
            "type": "integer", "minimum": 1, "maximum": 8192,
            "default": ANTHROPIC_DEFAULT_MAX_TOKENS,
        },
        "stream": {"type": "boolean", "default": False},
    },
    "latency_ms": {"p50": 450, "p95": 4000},
    "concurrency": {"max_in_flight": 4},
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


def notify(method: str, params: dict) -> None:
    frame = {"jsonrpc": "2.0", "method": method, "params": params}
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()


def log(msg: str) -> None:
    sys.stderr.write(f"[ai-llm-anthropic] {msg}\n")
    sys.stderr.flush()


def _auth_headers() -> dict:
    if not ANTHROPIC_API_KEY:
        raise RuntimeError("ANTHROPIC_API_KEY not set")
    return {
        "Content-Type": "application/json",
        "x-api-key": ANTHROPIC_API_KEY,
        "anthropic-version": ANTHROPIC_VERSION,
    }


def _split_system(messages: list) -> tuple[str, list]:
    """Anthropic wants `system` as a top-level field; collapse every
    `system` turn in `messages` into one string (joined by blank
    lines) and drop those entries from the conversation."""
    system_parts = [m.get("content", "") for m in messages if m.get("role") == "system"]
    conversation = [
        {"role": m["role"], "content": m.get("content", "")}
        for m in messages
        if m.get("role") in ("user", "assistant")
    ]
    return ("\n\n".join(s for s in system_parts if s), conversation)


def _body(messages: list, controls: dict, stream: bool) -> dict:
    system, conv = _split_system(messages)
    body = {
        "model": ANTHROPIC_MODEL,
        "max_tokens": ANTHROPIC_DEFAULT_MAX_TOKENS,
        "messages": conv,
        "stream": stream,
    }
    if system:
        body["system"] = system
    if controls:
        if "temperature" in controls:
            body["temperature"] = controls["temperature"]
        if "max_tokens" in controls:
            body["max_tokens"] = controls["max_tokens"]
    return body


def _chat_blocking(messages: list, controls: dict) -> dict:
    body = _body(messages, controls, stream=False)
    req = urllib.request.Request(
        f"{ANTHROPIC_API_BASE}/v1/messages",
        data=json.dumps(body).encode("utf-8"),
        headers=_auth_headers(),
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=REQUEST_TIMEOUT) as resp:
        raw = resp.read().decode("utf-8")
    data = json.loads(raw)
    # `content` is an array of content blocks; concat the text ones.
    content_blocks = data.get("content") or []
    text = "".join(
        block.get("text", "")
        for block in content_blocks
        if block.get("type") == "text"
    )
    usage = data.get("usage") or {}
    return {
        "message": {"role": "assistant", "content": text},
        "usage": {
            "input_tokens": usage.get("input_tokens", 0),
            "output_tokens": usage.get("output_tokens", 0),
        },
        "finish_reason": data.get("stop_reason") or "end_turn",
    }


def _chat_streaming(messages: list, controls: dict) -> dict:
    body = _body(messages, controls, stream=True)
    req = urllib.request.Request(
        f"{ANTHROPIC_API_BASE}/v1/messages",
        data=json.dumps(body).encode("utf-8"),
        headers=_auth_headers(),
        method="POST",
    )
    acc = ""
    input_tokens = 0
    output_tokens = 0
    stop_reason = "end_turn"

    with urllib.request.urlopen(req, timeout=REQUEST_TIMEOUT) as resp:
        for raw_line in resp:
            line = raw_line.decode("utf-8").strip()
            if not line.startswith("data:"):
                continue
            payload = line[len("data:"):].strip()
            if not payload or payload == "[DONE]":
                continue
            try:
                evt = json.loads(payload)
            except json.JSONDecodeError:
                log(f"bad SSE frame: {payload!r}")
                continue
            etype = evt.get("type")
            if etype == "content_block_delta":
                delta = (evt.get("delta") or {}).get("text") or ""
                if delta:
                    acc += delta
                    notify("ai.llm.partial", {
                        "text_delta": delta,
                        "text": acc,
                        "is_final": False,
                    })
            elif etype == "message_start":
                usage = (evt.get("message") or {}).get("usage") or {}
                input_tokens = usage.get("input_tokens", input_tokens)
            elif etype == "message_delta":
                usage = evt.get("usage") or {}
                output_tokens = usage.get("output_tokens", output_tokens)
                delta = evt.get("delta") or {}
                if delta.get("stop_reason"):
                    stop_reason = delta["stop_reason"]
            elif etype == "message_stop":
                break

    notify("ai.llm.partial", {
        "text_delta": "",
        "text": acc,
        "is_final": True,
    })
    return {
        "message": {"role": "assistant", "content": acc},
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
        "finish_reason": stop_reason,
    }


def chat(params: dict) -> dict:
    messages = params.get("messages")
    if not isinstance(messages, list) or not messages:
        raise ValueError("`messages` must be a non-empty array")
    controls = params.get("controls") or {}
    try:
        if controls.get("stream") is True:
            return _chat_streaming(messages, controls)
        return _chat_blocking(messages, controls)
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace") if hasattr(e, "read") else ""
        raise RuntimeError(f"anthropic HTTP {e.code}: {body[:400]}") from e
    except urllib.error.URLError as e:
        raise RuntimeError(f"anthropic unreachable at {ANTHROPIC_API_BASE}: {e}") from e


def main() -> int:
    log(f"ready (base={ANTHROPIC_API_BASE} model={ANTHROPIC_MODEL} key={'set' if ANTHROPIC_API_KEY else 'unset'})")
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
