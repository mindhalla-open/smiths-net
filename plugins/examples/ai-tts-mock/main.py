#!/usr/bin/env python3
"""Reference sidecar plugin — `ai.tts` capability, synthesis stubbed.

Implements the MVP handshake contract from
`docs/architecture/05-ai-plugin-protocol.md`:

  * one JSON-RPC 2.0 request per stdin line;
  * one JSON-RPC response per stdout line;
  * `describe_capabilities` returns a valid `ai.tts` descriptor.

When the engine lands the invocation path (next slice), this plugin
will grow a `synthesize` method that returns PCM bytes; until then it
declines any non-handshake call with a clean JSON-RPC error so
operators can verify that validation fails loud.
"""

from __future__ import annotations

import base64
import json
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import wave
from pathlib import Path


DESCRIPTOR = {
    "capability": "ai.tts",
    "plugin": "ai-tts-mock",
    "model_id": "mock-voice-v1",
    "abi": "1.0",
    "description": "Mock TTS used for protocol wiring demos.",
    "voices": [
        {
            "id": "irina",
            "lang": "ru",
            "gender": "female",
            "description": "Neutral Russian narrator.",
            "supported_sample_rates": [8000, 16000, 22050],
        },
        {
            "id": "dmitri",
            "lang": "ru",
            "gender": "male",
            "supported_sample_rates": [8000, 16000],
        },
        {
            "id": "alice",
            "lang": "en",
            "gender": "female",
            "supported_sample_rates": [8000, 16000, 22050],
        },
    ],
    "default_voice": "irina",
    "output_formats": [
        {"codec": "pcm_s16le", "sample_rates": [8000, 16000, 22050]},
        {"codec": "pcmu", "sample_rates": [8000]},
    ],
    "controls": {
        "rate":   {"type": "number", "minimum": 0.5, "maximum": 2.0, "default": 1.0, "unit": "ratio"},
        "pitch":  {"type": "number", "minimum": -12, "maximum": 12, "unit": "semitones"},
        "volume": {"type": "number", "minimum": 0.0, "maximum": 1.0, "default": 1.0},
    },
    "ssml":      {"supported": False, "dialects": []},
    "streaming": {"supported": True, "first_chunk_ms": 50, "chunk_ms": 200},
    "latency_ms": {"p50": 40, "p95": 120},
    "concurrency": {"max_in_flight": 4},
}


def reply(id_: int | None, *, result=None, error=None) -> None:
    """Emit one JSON-RPC response / error line to stdout."""
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
    """Plugin diagnostics — always to stderr, never stdout."""
    sys.stderr.write(f"[ai-tts-mock] {msg}\n")
    sys.stderr.flush()


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
            # Protocol returns either a single descriptor or a list.
            reply(id_, result=[DESCRIPTOR])
        elif method == "synthesize":
            try:
                result = handle_synthesize(req.get("params") or {})
                reply(id_, result=result)
            except ValueError as e:
                reply(id_, error={"code": -32602, "message": str(e)})
            except Exception as e:  # noqa: BLE001
                reply(id_, error={"code": -32603, "message": f"synthesize failed: {e}"})
        elif method == "shutdown":
            reply(id_, result=None)
            log("shutdown requested — exiting")
            return 0
        elif method in ("ping",):
            reply(id_, result={"ok": True})
        else:
            reply(
                id_,
                error={
                    "code": -32601,
                    "message": f"method `{method}` not implemented yet",
                },
            )
    return 0


def handle_synthesize(params: dict) -> dict:
    """Render `params['text']` to PCM16 LE at 8 kHz and return it
    base64-encoded alongside format metadata."""
    text = params.get("text")
    if not isinstance(text, str) or not text:
        raise ValueError("missing `text`")
    # Validate voice here too (engine also validates, belt-and-braces).
    voice_id = params.get("voice") or DESCRIPTOR["default_voice"]
    voices = {v["id"]: v for v in DESCRIPTOR["voices"]}
    if voice_id not in voices:
        raise ValueError(
            f"unknown voice `{voice_id}`; allowed: {sorted(voices)}"
        )
    out = params.get("output") or {}
    sample_rate = int(out.get("sample_rate", 8000))
    codec = out.get("codec", "pcm_s16le")
    if codec != "pcm_s16le":
        raise ValueError(f"codec `{codec}` not supported by this plugin (only pcm_s16le)")
    if sample_rate not in (8000, 16000, 22050):
        raise ValueError(f"sample_rate {sample_rate} unsupported")

    pcm = _tts_pcm16(text, voice_id, sample_rate)
    return {
        "codec": codec,
        "sample_rate": sample_rate,
        "frames": len(pcm) // 2,
        "duration_ms": int(len(pcm) // 2 * 1000 / sample_rate),
        "audio_base64": base64.b64encode(pcm).decode("ascii"),
        "voice": voice_id,
    }


def _tts_pcm16(text: str, voice_id: str, sample_rate: int) -> bytes:
    """Real TTS via macOS `say`; fallback = silence (so tests still pass
    on Linux CI without altering the response shape)."""
    if shutil.which("say"):
        # macOS voice names don't line up with our descriptor voice ids;
        # map a couple of the common ones and let unknowns fall back.
        macos_voice = {
            "irina": "Milena",   # Russian female
            "dmitri": "Yuri",    # Russian male
            "alice": "Samantha", # English female
        }.get(voice_id, "Samantha")
        with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as fh:
            wav_path = Path(fh.name)
        try:
            subprocess.run(
                [
                    "say", "-v", macos_voice,
                    "--file-format=WAVE",
                    f"--data-format=LEI16@{sample_rate}",
                    "-o", str(wav_path), text,
                ],
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            with wave.open(str(wav_path), "rb") as wf:
                return wf.readframes(wf.getnframes())
        finally:
            wav_path.unlink(missing_ok=True)
    # No `say` — return a short silence so the wire still works.
    samples = sample_rate // 10  # 100 ms
    return struct.pack(f"<{samples}h", *([0] * samples))


if __name__ == "__main__":
    sys.exit(main())
