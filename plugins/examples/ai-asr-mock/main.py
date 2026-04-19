#!/usr/bin/env python3
"""Reference sidecar — `ai.asr` capability, transcription stubbed.

Implements `describe_capabilities` + `transcribe` per
`docs/architecture/05-ai-plugin-protocol.md`. The stub returns a
deterministic transcript shaped from the input audio's duration —
enough to exercise the full MCP → plugin → MCP response loop without
dragging in Whisper / faster-whisper.

Real replacement (post-MVP P22): swap `fake_transcribe()` for a call
into whisper.cpp / faster-whisper / Deepgram; the descriptor
`controls` and I/O shape stay the same.
"""

from __future__ import annotations

import base64
import json
import sys

DESCRIPTOR = {
    "capability": "ai.asr",
    "plugin": "ai-asr-mock",
    "model_id": "mock-asr-v1",
    "abi": "1.0",
    "description": "Mock ASR used for protocol wiring demos.",
    "languages": ["auto", "ru", "en"],
    "features": [],
    "input_formats": [
        {"codec": "pcm_s16le", "sample_rates": [8000, 16000]},
    ],
    "streaming": {"supported": False},
    "controls": {
        "language": {
            "type": "string",
            "enum": ["auto", "ru", "en"],
            "default": "auto",
        },
        "beam_size": {"type": "integer", "minimum": 1, "maximum": 10, "default": 5},
        "stream": {"type": "boolean", "default": False},
    },
    "latency_ms": {"p50": 80, "p95": 300},
    "concurrency": {"max_in_flight": 2},
}


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
    sys.stderr.write(f"[ai-asr-mock] {msg}\n")
    sys.stderr.flush()


def emit_notification(method: str, params: dict) -> None:
    """Plugin → engine notification (JSON-RPC frame with no id)."""
    frame = {"jsonrpc": "2.0", "method": method, "params": params}
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()


def fake_transcribe(params: dict) -> dict:
    """Deterministic placeholder: derive 'text' from the input size
    so tests can still assert on it, but shape is identical to what a
    real model would return.

    When `controls.stream = true`, emit a handful of `emit_partial`
    notifications before the final response so the engine's
    bidirectional-RPC path can be exercised end-to-end."""
    b64 = params.get("audio_base64")
    if not isinstance(b64, str):
        raise ValueError("`audio_base64` required")
    raw = base64.b64decode(b64)
    sample_rate = int(params.get("sample_rate") or 8000)
    # PCM16 = 2 bytes per sample.
    samples = len(raw) // 2
    duration_s = samples / sample_rate if sample_rate else 0.0
    lang_req = (params.get("language") or DESCRIPTOR["controls"]["language"]["default"])
    # Auto → fall back to Russian for the demo (voice agent speaks Russian).
    lang = "ru" if lang_req == "auto" else lang_req
    if duration_s < 0.3:
        text = "(silence)"
        confidence = 0.0
    else:
        text = f"[mock ASR: ~{duration_s:.1f} s of audio]"
        confidence = 0.92

    controls = params.get("controls") or {}
    if bool(controls.get("stream")):
        # Emit a couple of partials before the final — enough to
        # prove the bidirectional path works without slowing tests.
        call_id = params.get("call_id")
        for i, fragment in enumerate(_partials_for(text)):
            emit_notification("emit_partial", {
                "call_id": call_id,
                "text": fragment,
                "confidence": round(confidence * (i + 1) / 3, 3),
                "is_final": False,
            })

    return {
        "text": text,
        "language": lang,
        "confidence": confidence,
        "duration_s": round(duration_s, 3),
    }


def _partials_for(text: str) -> list[str]:
    """Split `text` into two cumulative partials so streaming demos
    have something to render."""
    if not text:
        return []
    mid = max(1, len(text) // 2)
    return [text[:mid], text]


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
        elif method == "transcribe":
            try:
                reply(id_, result=fake_transcribe(req.get("params") or {}))
            except ValueError as e:
                reply(id_, error={"code": -32602, "message": str(e)})
            except Exception as e:  # noqa: BLE001
                reply(id_, error={"code": -32603, "message": f"transcribe failed: {e}"})
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
