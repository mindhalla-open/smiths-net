#!/usr/bin/env python3
"""Piper-backed `ai.tts` sidecar — slice 3.1 reference.

Shells out to the `piper` binary with `--output_raw` and streams the
resulting 22.05 kHz mono PCM16 LE bytes back through the contract
shape this engine expects (see
`docs/architecture/05-ai-plugin-protocol.md`).

Environment overrides:
  PIPER_BIN       — path to the `piper` executable (default `piper`)
  PIPER_VOICE     — path to the ONNX voice model
  PIPER_VOICE_ID  — voice id advertised in the descriptor (default `default`)
  PIPER_LANG      — BCP-47 tag for the voice (default `en`)

If `piper` isn't on PATH or the voice file is missing,
`describe_capabilities` still succeeds but `synthesize` returns a
JSON-RPC error and the dispatcher fails over to the next ai.tts
provider. Stdlib-only.
"""

from __future__ import annotations

import base64
import json
import os
import shutil
import struct
import subprocess
import sys

PIPER_BIN = os.environ.get("PIPER_BIN", "piper")
PIPER_VOICE = os.environ.get("PIPER_VOICE", "")
PIPER_VOICE_ID = os.environ.get("PIPER_VOICE_ID", "default")
PIPER_LANG = os.environ.get("PIPER_LANG", "en")
# Piper's default output rate for the common en_US / ru / etc. voice
# packs. Callers may still request 8 kHz; we resample by decimation.
NATIVE_RATE = 22050


DESCRIPTOR = {
    "capability": "ai.tts",
    "plugin": "ai-tts-piper",
    "model_id": os.path.basename(PIPER_VOICE) or "piper",
    "abi": "1.0",
    "description": "Piper neural TTS (local ONNX voice).",
    # Lower number wins — prefer Piper over the canned mock.
    "priority": 20,
    "voices": [
        {
            "id": PIPER_VOICE_ID,
            "lang": PIPER_LANG,
            "supported_sample_rates": [8000, 16000, 22050],
        },
    ],
    "default_voice": PIPER_VOICE_ID,
    "output_formats": [
        {"codec": "pcm_s16le", "sample_rates": [8000, 16000, 22050]},
    ],
    "controls": {
        "rate": {
            "type": "number", "minimum": 0.5, "maximum": 2.0, "default": 1.0,
        },
    },
    "ssml": {"supported": False, "dialects": []},
    "streaming": {"supported": False},
    "latency_ms": {"p50": 300, "p95": 1500},
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
    sys.stderr.write(f"[ai-tts-piper] {msg}\n")
    sys.stderr.flush()


def piper_synth(text: str, length_scale: float) -> bytes:
    if not shutil.which(PIPER_BIN):
        raise RuntimeError(f"`{PIPER_BIN}` not on PATH")
    if not PIPER_VOICE or not os.path.exists(PIPER_VOICE):
        raise RuntimeError(f"PIPER_VOICE not set or missing: `{PIPER_VOICE}`")
    proc = subprocess.run(
        [
            PIPER_BIN,
            "--model", PIPER_VOICE,
            "--output_raw",
            "--length_scale", str(length_scale),
        ],
        input=text.encode("utf-8"),
        capture_output=True,
        check=True,
    )
    return proc.stdout


def decimate_to(pcm: bytes, from_hz: int, to_hz: int) -> bytes:
    if from_hz == to_hz:
        return pcm
    if to_hz <= 0 or from_hz <= 0 or to_hz > from_hz:
        return pcm  # don't upsample — the caller asked for something odd
    # Crude pick-every-Nth — good enough for a walking-skeleton sample;
    # a production plugin would use a proper resampler.
    step = from_hz / to_hz
    samples = struct.unpack(f"<{len(pcm) // 2}h", pcm)
    out_len = int(len(samples) / step)
    picked = [samples[int(i * step)] for i in range(out_len)]
    return struct.pack(f"<{len(picked)}h", *picked)


def synthesize(params: dict) -> dict:
    text = params.get("text")
    if not isinstance(text, str) or not text:
        raise ValueError("missing `text`")
    voice_id = params.get("voice") or PIPER_VOICE_ID
    if voice_id != PIPER_VOICE_ID:
        raise ValueError(f"unknown voice `{voice_id}`; this plugin serves `{PIPER_VOICE_ID}`")
    out = params.get("output") or {}
    codec = out.get("codec", "pcm_s16le")
    if codec != "pcm_s16le":
        raise ValueError(f"codec `{codec}` not supported (pcm_s16le only)")
    sample_rate = int(out.get("sample_rate", NATIVE_RATE))
    if sample_rate not in (8000, 16000, 22050):
        raise ValueError(f"sample_rate {sample_rate} unsupported")

    controls = params.get("controls") or {}
    rate = float(controls.get("rate", 1.0))
    # Piper uses `length_scale` — inverse of the speech rate.
    length_scale = 1.0 / max(rate, 0.1)

    pcm = piper_synth(text, length_scale)
    pcm = decimate_to(pcm, NATIVE_RATE, sample_rate)
    return {
        "codec": codec,
        "sample_rate": sample_rate,
        "frames": len(pcm) // 2,
        "duration_ms": int(len(pcm) // 2 * 1000 / sample_rate),
        "audio_base64": base64.b64encode(pcm).decode("ascii"),
        "voice": voice_id,
    }


def main() -> int:
    log(f"ready (bin={PIPER_BIN} voice={PIPER_VOICE or '<unset>'})")
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
        elif method == "synthesize":
            try:
                reply(id_, result=synthesize(req.get("params") or {}))
            except ValueError as e:
                reply(id_, error={"code": -32602, "message": str(e)})
            except subprocess.CalledProcessError as e:
                reply(id_, error={"code": -32603, "message": f"piper exited {e.returncode}: {e.stderr!r}"})
            except Exception as e:  # noqa: BLE001
                reply(id_, error={"code": -32603, "message": f"synthesize failed: {e}"})
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
