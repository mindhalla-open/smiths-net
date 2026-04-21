#!/usr/bin/env python3
"""OpenAI-backed `ai.llm.chat` sidecar — slice 3.2 reference.

Implements the engine's `describe_capabilities` + `chat` contract
against OpenAI's `/v1/chat/completions` API. When the caller sets
`controls.stream = true`, partial deltas are streamed back through
`ai.llm.partial` JSON-RPC notifications and the final response
carries the full assembled message + token usage.

Environment:
  OPENAI_API_KEY    — required (sidecar returns an error per-call
                      if missing; descriptor still loads so operators
                      can see the provider on `list_ai_providers`).
  OPENAI_API_BASE   — default `https://api.openai.com`
  OPENAI_MODEL      — default `gpt-4o-mini`
  OPENAI_TIMEOUT_SECS — default 60 (non-streaming only; streaming
                         does its own per-chunk socket read)

Stdlib-only; no `openai` package dependency.
"""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request

OPENAI_API_KEY = os.environ.get("OPENAI_API_KEY", "")
OPENAI_API_BASE = os.environ.get("OPENAI_API_BASE", "https://api.openai.com").rstrip("/")
OPENAI_MODEL = os.environ.get("OPENAI_MODEL", "gpt-4o-mini")
REQUEST_TIMEOUT = float(os.environ.get("OPENAI_TIMEOUT_SECS", "60"))

DESCRIPTOR = {
    "capability": "ai.llm.chat",
    "plugin": "ai-llm-openai",
    "model_id": OPENAI_MODEL,
    "abi": "1.0",
    "description": f"OpenAI Chat Completions proxy ({OPENAI_MODEL}).",
    # Cloud > local-Ollama (priority 20) > canned mock (50).
    "priority": 15,
    "context_window": 128_000,
    "max_output": 4096,
    "features": ["system_prompt"],
    "roles": ["system", "user", "assistant", "tool"],
    "streaming": {"supported": True, "event": "ai.llm.partial"},
    "controls": {
        "temperature": {
            "type": "number", "minimum": 0.0, "maximum": 2.0, "default": 1.0,
        },
        "max_tokens": {
            "type": "integer", "minimum": 1, "maximum": 4096, "default": 512,
        },
        "stream": {"type": "boolean", "default": False},
    },
    "latency_ms": {"p50": 400, "p95": 3500},
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
    """JSON-RPC 2.0 notification — the supervisor broadcasts these as
    `PluginNotification`s; the MCP adapter forwards them to clients
    as `notifications/plugin/<method>` frames."""
    frame = {"jsonrpc": "2.0", "method": method, "params": params}
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()


def log(msg: str) -> None:
    sys.stderr.write(f"[ai-llm-openai] {msg}\n")
    sys.stderr.flush()


def _auth_headers() -> dict:
    if not OPENAI_API_KEY:
        raise RuntimeError("OPENAI_API_KEY not set")
    return {
        "Content-Type": "application/json",
        "Authorization": f"Bearer {OPENAI_API_KEY}",
    }


def _openai_body(messages: list, controls: dict, stream: bool) -> dict:
    body = {
        "model": OPENAI_MODEL,
        "messages": messages,
        "stream": stream,
    }
    if controls:
        if "temperature" in controls:
            body["temperature"] = controls["temperature"]
        if "max_tokens" in controls:
            body["max_tokens"] = controls["max_tokens"]
    if stream:
        # Ask for usage even in streaming mode so token accounting
        # matches the non-streaming path.
        body["stream_options"] = {"include_usage": True}
    return body


def _chat_blocking(messages: list, controls: dict) -> dict:
    body = _openai_body(messages, controls, stream=False)
    req = urllib.request.Request(
        f"{OPENAI_API_BASE}/v1/chat/completions",
        data=json.dumps(body).encode("utf-8"),
        headers=_auth_headers(),
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=REQUEST_TIMEOUT) as resp:
        raw = resp.read().decode("utf-8")
    data = json.loads(raw)
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


def _chat_streaming(messages: list, controls: dict) -> dict:
    """Stream chunks from OpenAI's SSE, emitting `ai.llm.partial` per
    delta + returning the fully assembled response."""
    body = _openai_body(messages, controls, stream=True)
    req = urllib.request.Request(
        f"{OPENAI_API_BASE}/v1/chat/completions",
        data=json.dumps(body).encode("utf-8"),
        headers=_auth_headers(),
        method="POST",
    )
    acc = ""
    finish_reason = "stop"
    input_tokens = 0
    output_tokens = 0

    with urllib.request.urlopen(req, timeout=REQUEST_TIMEOUT) as resp:
        for raw_line in resp:
            line = raw_line.decode("utf-8").strip()
            if not line or not line.startswith("data:"):
                continue
            payload = line[len("data:"):].strip()
            if payload == "[DONE]":
                break
            try:
                evt = json.loads(payload)
            except json.JSONDecodeError:
                log(f"bad SSE frame: {payload!r}")
                continue
            choices = evt.get("choices") or []
            if choices:
                delta = (choices[0].get("delta") or {}).get("content") or ""
                if delta:
                    acc += delta
                    notify("ai.llm.partial", {
                        "text_delta": delta,
                        "text": acc,
                        "is_final": False,
                    })
                fr = choices[0].get("finish_reason")
                if fr:
                    finish_reason = fr
            usage = evt.get("usage")
            if usage:
                input_tokens = usage.get("prompt_tokens", input_tokens)
                output_tokens = usage.get("completion_tokens", output_tokens)

    # Final-marker partial so clients can close their streaming UI.
    notify("ai.llm.partial", {
        "text_delta": "",
        "text": acc,
        "is_final": True,
    })
    return {
        "message": {"role": "assistant", "content": acc},
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
        "finish_reason": finish_reason,
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
        raise RuntimeError(f"openai HTTP {e.code}: {body[:400]}") from e
    except urllib.error.URLError as e:
        raise RuntimeError(f"openai unreachable at {OPENAI_API_BASE}: {e}") from e


def main() -> int:
    log(f"ready (base={OPENAI_API_BASE} model={OPENAI_MODEL} key={'set' if OPENAI_API_KEY else 'unset'})")
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
