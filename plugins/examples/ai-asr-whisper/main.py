#!/usr/bin/env python3
"""whisper.cpp-backed `ai.asr` sidecar — slice 3.3 reference.

Shells out to the `whisper-cli` (or legacy `main`) binary from the
whisper.cpp distribution, feeding it a temp WAV decoded from the
`audio_base64` payload. Parses the JSON output and returns it in
the shape the engine's `ai.asr` contract expects (see
`docs/architecture/05-ai-plugin-protocol.md`).

Environment overrides:
  WHISPER_BIN      — whisper.cpp executable (default `whisper-cli`)
  WHISPER_MODEL    — path to the GGML model file (required at call
                      time; descriptor still loads without it)
  WHISPER_THREADS  — `-t` value (default 4)
  WHISPER_LANG     — `-l` value; use `auto` for language detection

Stdlib-only; no `whisper` / `faster-whisper` Python dep.
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

WHISPER_BIN = os.environ.get("WHISPER_BIN", "whisper-cli")
WHISPER_MODEL = os.environ.get("WHISPER_MODEL", "")
WHISPER_THREADS = int(os.environ.get("WHISPER_THREADS", "4"))
WHISPER_DEFAULT_LANG = os.environ.get("WHISPER_LANG", "auto")

DESCRIPTOR = {
    "capability": "ai.asr",
    "plugin": "ai-asr-whisper",
    "model_id": Path(WHISPER_MODEL).name or "whisper-ggml",
    "abi": "1.0",
    "description": "whisper.cpp neural ASR (local GGML model).",
    # Cloud LLMs live in slice 3.2 at priority 15/16; we leave 15-19
    # open for hypothetical cloud ASRs and pick 20 here so Whisper
    # beats the canned mock (50) by default.
    "priority": 20,
    "languages": ["auto", "en", "ru", "es", "fr", "de", "zh", "ja", "pt"],
    "features": [],
    "input_formats": [
        {"codec": "pcm_s16le", "sample_rates": [8000, 16000]},
    ],
    "streaming": {"supported": False},
    "controls": {
        "language": {"type": "string", "default": WHISPER_DEFAULT_LANG},
        "beam_size": {"type": "integer", "minimum": 1, "maximum": 10, "default": 5},
    },
    "latency_ms": {"p50": 700, "p95": 4000},
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
    sys.stderr.write(f"[ai-asr-whisper] {msg}\n")
    sys.stderr.flush()


def _pcm_bytes_to_wav(pcm: bytes, sample_rate: int, path: Path) -> None:
    """Wrap PCM16 LE mono `pcm` in a valid WAV container at `path`.
    whisper.cpp wants a file, not stdin, so every call pays one
    tempfile write. The overhead is negligible next to inference."""
    with wave.open(str(path), "wb") as wf:
        wf.setnchannels(1)
        wf.setsampwidth(2)
        wf.setframerate(sample_rate)
        wf.writeframes(pcm)


def _upsample_to_16k(pcm: bytes, from_hz: int) -> bytes:
    """Crude integer upsampler — whisper.cpp requires 16 kHz input.
    Duplicate samples when going 8 → 16 kHz; when already 16 kHz,
    return unchanged. Production plugins should use a real resampler
    for quality, but this is adequate for speech at 8 kHz source."""
    if from_hz == 16_000:
        return pcm
    if from_hz == 8_000:
        samples = struct.unpack(f"<{len(pcm) // 2}h", pcm)
        upsampled = []
        for s in samples:
            upsampled.append(s)
            upsampled.append(s)
        return struct.pack(f"<{len(upsampled)}h", *upsampled)
    # Unsupported rate — let whisper.cpp error out with a clear message.
    return pcm


def transcribe(params: dict) -> dict:
    if not shutil.which(WHISPER_BIN):
        raise RuntimeError(f"`{WHISPER_BIN}` not on PATH")
    if not WHISPER_MODEL or not os.path.exists(WHISPER_MODEL):
        raise RuntimeError(
            f"WHISPER_MODEL not set or missing: `{WHISPER_MODEL}`"
        )
    b64 = params.get("audio_base64")
    if not isinstance(b64, str):
        raise ValueError("`audio_base64` must be a string")
    try:
        pcm = base64.b64decode(b64)
    except Exception as e:
        raise ValueError(f"audio_base64 decode failed: {e}") from e
    sample_rate = int(params.get("sample_rate") or 8000)
    controls = params.get("controls") or {}
    language = (
        params.get("language")
        or controls.get("language")
        or WHISPER_DEFAULT_LANG
    )
    beam_size = int(controls.get("beam_size", 5))

    pcm16k = _upsample_to_16k(pcm, sample_rate)

    with tempfile.TemporaryDirectory() as d:
        wav = Path(d) / "in.wav"
        _pcm_bytes_to_wav(pcm16k, 16_000, wav)
        out_prefix = Path(d) / "out"
        cmd = [
            WHISPER_BIN,
            "-m", WHISPER_MODEL,
            "-f", str(wav),
            "-t", str(WHISPER_THREADS),
            "-bs", str(beam_size),
            "-oj",  # write JSON
            "-of", str(out_prefix),
            "-nt",  # no timestamps in stdout (JSON has them)
        ]
        if language and language != "auto":
            cmd += ["-l", language]
        proc = subprocess.run(
            cmd,
            capture_output=True,
            check=False,
        )
        if proc.returncode != 0:
            raise RuntimeError(
                f"whisper-cli exited {proc.returncode}: "
                f"{proc.stderr.decode('utf-8', errors='replace')[:400]}"
            )
        json_path = Path(f"{out_prefix}.json")
        if not json_path.exists():
            # Some forks print JSON to stdout instead of a file.
            data = json.loads(proc.stdout.decode("utf-8", errors="replace") or "{}")
        else:
            with open(json_path, "r", encoding="utf-8") as fh:
                data = json.load(fh)

    segments = data.get("transcription") or data.get("segments") or []
    text = "".join(seg.get("text", "") for seg in segments).strip()
    if not text:
        text = (data.get("text") or "").strip()
    detected_lang = data.get("language") or language

    return {
        "text": text,
        "language": detected_lang,
        "confidence": None,
        "segments": segments,
    }


def main() -> int:
    log(f"ready (bin={WHISPER_BIN} model={WHISPER_MODEL or '<unset>'})")
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
                reply(id_, result=transcribe(req.get("params") or {}))
            except ValueError as e:
                reply(id_, error={"code": -32602, "message": str(e)})
            except Exception as e:  # noqa: BLE001
                reply(id_, error={"code": -32603, "message": f"transcribe failed: {e}"})
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
