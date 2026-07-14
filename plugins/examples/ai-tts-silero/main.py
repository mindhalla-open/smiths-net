#!/usr/bin/env python3
"""Silero TTS v5 offline `ai.tts` sidecar.

Best open-source Russian TTS quality: auto-stress, homographs, questions
(v5_5_ru). Runs fully on CPU after the model is cached — no network calls
during synthesis.

Environment:
  SILERO_MODEL    — model id (default `v5_5_ru`; also v5_4_ru, v5_3_ru, v5_ru)
  SILERO_SPEAKER  — voice within model (default `xenia`)
                    v5_5_ru: aidar, baya, kseniya, xenia, eugene
  SILERO_THREADS  — PyTorch intra-op threads (default 4)
  SILERO_DEVICE   — `cpu` (default) or `cuda`

Requires (one-time install + model cache download):
  pip install torch silero

The model (~150 MB) downloads on first synthesize and is cached locally.
Request sample_rate=8000 for telephony — Silero synthesizes at 8 kHz natively.
"""

from __future__ import annotations

import base64
import json
import os
import sys
import threading
from pathlib import Path

# Shared resampler (anti-aliasing downsample for non-native rates).
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from audio_resample import resample_pcm16  # noqa: E402

SILERO_MODEL = os.environ.get("SILERO_MODEL", "v5_5_ru")
SILERO_SPEAKER = os.environ.get("SILERO_SPEAKER", "xenia")
SILERO_THREADS = int(os.environ.get("SILERO_THREADS", "4"))
SILERO_DEVICE = os.environ.get("SILERO_DEVICE", "cpu")
# HQ telephony: synthesize at a wideband rate and anti-alias downsample to
# the requested narrowband rate. Silero's native 8 kHz voice is muffled and
# "robotic"; rendering at 24 kHz then resampling keeps the formants intact and
# sounds far clearer on the 8 kHz G.711 wire (same trick FreeSWITCH uses).
SILERO_HQ = os.environ.get("SILERO_HQ", "1").lower() in ("1", "true", "yes", "on")
SILERO_HQ_RATE = int(os.environ.get("SILERO_HQ_RATE", "24000"))

RU_SPEAKERS = {
    "v5_5_ru": ("aidar", "baya", "kseniya", "xenia", "eugene"),
    "v5_4_ru": ("aidar", "baya", "kseniya", "xenia"),
    "v5_3_ru": ("aidar", "baya", "kseniya", "xenia", "eugene"),
    "v5_2_ru": ("aidar", "baya", "kseniya", "xenia", "eugene"),
    "v5_ru": ("aidar", "baya", "kseniya", "xenia", "eugene"),
}
NATIVE_RATES = (8000, 24000, 48000)

DESCRIPTOR = {
    "capability": "ai.tts",
    "plugin": "ai-tts-silero",
    "model_id": SILERO_MODEL,
    "abi": "1.0",
    "description": f"Silero offline Russian TTS ({SILERO_MODEL}, speaker={SILERO_SPEAKER}).",
    # Prefer Silero over Piper (20) and Edge (22) when loaded.
    "priority": 15,
    "voices": [
        {
            "id": sp,
            "lang": "ru",
            "supported_sample_rates": list(NATIVE_RATES),
        }
        for sp in RU_SPEAKERS.get(SILERO_MODEL, RU_SPEAKERS["v5_5_ru"])
    ],
    "default_voice": SILERO_SPEAKER,
    "output_formats": [
        {"codec": "pcm_s16le", "sample_rates": list(NATIVE_RATES)},
    ],
    "controls": {
        "rate": {
            "type": "number", "minimum": 0.5, "maximum": 2.0, "default": 1.0,
        },
    },
    "ssml": {"supported": True, "dialects": ["silero-ru"]},
    "streaming": {"supported": False},
    "latency_ms": {"p50": 200, "p95": 900},
    "concurrency": {"max_in_flight": 2},
}

_model = None
_model_lock = threading.Lock()
_load_error = ""


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
    sys.stderr.write(f"[ai-tts-silero] {msg}\n")
    sys.stderr.flush()


def _load_model():
    global _model, _load_error
    with _model_lock:
        if _model is not None:
            return _model
        if _load_error:
            raise RuntimeError(_load_error)
        try:
            import torch

            torch.set_num_threads(SILERO_THREADS)
            device = torch.device(SILERO_DEVICE)

            try:
                from silero import silero_tts

                model, _ = silero_tts(language="ru", speaker=SILERO_MODEL)
                model.to(device)
                _model = model
                log(f"model loaded via silero pip: {SILERO_MODEL} on {device}")
                return _model
            except ImportError:
                pass

            # Fallback: torch.hub (downloads on first use, then offline).
            hub_model = __import__("torch").hub.load(
                repo_or_dir="snakers4/silero-models",
                model="silero_tts",
                language="ru",
                speaker=SILERO_MODEL,
                trust_repo=True,
            )
            if isinstance(hub_model, tuple):
                # Older hub API returns (model, symbols, sr, example, apply_tts).
                apply_fn = hub_model[-1]
                _model = ("hub_fn", apply_fn, device)
            else:
                hub_model.to(device)
                _model = hub_model
            log(f"model loaded via torch.hub: {SILERO_MODEL} on {device}")
            return _model
        except Exception as e:  # noqa: BLE001
            _load_error = (
                f"Silero load failed ({e}). Install: pip install torch silero"
            )
            raise RuntimeError(_load_error) from e


def _warmup() -> None:
    try:
        model = _load_model()
        _synth_pcm(model, "тест", SILERO_SPEAKER, 8000)
        log("warmup complete")
    except Exception as e:  # noqa: BLE001
        log(f"warmup skipped: {e}")


def _synth_pcm(model, text: str, speaker: str, sample_rate: int) -> bytes:
    if isinstance(model, tuple) and model[0] == "hub_fn":
        _, apply_fn, device = model
        audio = apply_fn(
            text=text,
            speaker=speaker,
            sample_rate=sample_rate,
            put_accent=True,
            put_yo=True,
            device=device,
        )
    else:
        audio = model.apply_tts(
            text=text,
            speaker=speaker,
            sample_rate=sample_rate,
        )

    # Silero returns float tensor/array in [-1, 1].
    if hasattr(audio, "cpu"):
        audio = audio.cpu()
    if hasattr(audio, "numpy"):
        import numpy as np

        arr = audio.numpy().astype("float32").flatten()
    else:
        import numpy as np

        arr = np.asarray(audio, dtype=np.float32).flatten()

    import numpy as np

    arr = np.clip(arr, -1.0, 1.0)
    return (arr * 32767).astype(np.int16).tobytes()


def synthesize(params: dict) -> dict:
    text = params.get("text")
    if not isinstance(text, str) or not text:
        raise ValueError("missing `text`")

    voice_id = params.get("voice") or SILERO_SPEAKER
    allowed = RU_SPEAKERS.get(SILERO_MODEL, RU_SPEAKERS["v5_5_ru"])
    if voice_id not in allowed:
        raise ValueError(f"unknown voice `{voice_id}`; supported: {', '.join(allowed)}")

    out = params.get("output") or {}
    codec = out.get("codec", "pcm_s16le")
    if codec != "pcm_s16le":
        raise ValueError(f"codec `{codec}` not supported (pcm_s16le only)")

    sample_rate = int(out.get("sample_rate", 8000))
    if sample_rate not in NATIVE_RATES:
        raise ValueError(f"sample_rate {sample_rate} unsupported; use {NATIVE_RATES}")

    model = _load_model()

    # HQ path: render wideband then anti-alias downsample to the narrowband
    # telephony rate — clearer than Silero's native 8 kHz voice.
    if SILERO_HQ and sample_rate < SILERO_HQ_RATE and SILERO_HQ_RATE in NATIVE_RATES:
        pcm = _synth_pcm(model, text, voice_id, SILERO_HQ_RATE)
        pcm = resample_pcm16(pcm, SILERO_HQ_RATE, sample_rate)
    else:
        pcm = _synth_pcm(model, text, voice_id, sample_rate)

    return {
        "codec": codec,
        "sample_rate": sample_rate,
        "frames": len(pcm) // 2,
        "duration_ms": int(len(pcm) // 2 * 1000 / sample_rate),
        "audio_base64": base64.b64encode(pcm).decode("ascii"),
        "voice": voice_id,
    }


def main() -> int:
    log(
        f"ready (model={SILERO_MODEL} speaker={SILERO_SPEAKER} "
        f"device={SILERO_DEVICE} threads={SILERO_THREADS})"
    )
    threading.Thread(target=_warmup, daemon=True).start()

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
