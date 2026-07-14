#!/usr/bin/env python3
"""faster-whisper `ai.asr` sidecar — GPU, offline, high-accuracy STT.

Uses CTranslate2 (faster-whisper) to run Whisper large-v3 on the local
GPU. Far more accurate than the small Vosk model on noisy 8 kHz
telephony audio, while staying fully offline once the model is cached.

The model is loaded once at startup (and JIT-warmed on a silence frame)
so the first real utterance doesn't pay the cold-start cost.

Environment:
  FW_MODEL          — model size or local path (default `large-v3`).
                      First use downloads from HF into FW_DOWNLOAD_ROOT.
  FW_DEVICE         — `cuda` | `cpu` | `auto` (default `cuda`).
  FW_COMPUTE_TYPE   — CTranslate2 compute type (default `float16` on
                      cuda, `int8` on cpu).
  FW_DOWNLOAD_ROOT  — model cache dir (default
                      ~/.local/share/smiths-net/models/faster-whisper).
  FW_LANG           — language hint (default `ru`; `auto` to detect).
  FW_BEAM_SIZE      — beam width (default 5).
  FW_VAD            — `1` to enable built-in VAD silence trimming
                      (default 1 — kills tail-noise hallucinations).
  FW_INITIAL_PROMPT — optional text to bias decoding (domain/spelling).

Requires: pip install faster-whisper  (CTranslate2 + CUDA libs come
from the torch cu* wheels already installed in the venv).
"""

from __future__ import annotations

import base64
import json
import os
import sys
import threading
from pathlib import Path


def _ensure_cuda_libs() -> None:
    """Put the pip-installed CUDA-12 cuBLAS/cuDNN on LD_LIBRARY_PATH.

    CTranslate2 (faster-whisper) links `libcublas.so.12` / `libcudnn.so.9`,
    but the venv's torch ships CUDA-13 libs (`*.so.13`). The matching cu12
    wheels (`nvidia-cublas-cu12`, `nvidia-cudnn-cu12`) live under
    site-packages/nvidia/*/lib. The dynamic linker resolves DT_NEEDED at
    extension-import time, so we must fix LD_LIBRARY_PATH *before* importing
    ctranslate2 — hence a one-shot re-exec when the dirs are missing."""
    import sysconfig

    purelib = sysconfig.get_paths().get("purelib", "")
    wanted = [
        os.path.join(purelib, "nvidia", pkg, "lib")
        for pkg in ("cublas", "cudnn", "cuda_runtime")
    ]
    wanted = [d for d in wanted if os.path.isdir(d)]
    if not wanted:
        return
    cur = os.environ.get("LD_LIBRARY_PATH", "")
    have = cur.split(":") if cur else []
    missing = [d for d in wanted if d not in have]
    if not missing:
        return
    os.environ["LD_LIBRARY_PATH"] = ":".join(missing + have)
    os.execv(sys.executable, [sys.executable, *sys.argv])


_ensure_cuda_libs()

FW_MODEL = os.environ.get("FW_MODEL", "large-v3")
FW_DEVICE = os.environ.get("FW_DEVICE", "cuda")
FW_DOWNLOAD_ROOT = os.environ.get(
    "FW_DOWNLOAD_ROOT",
    os.path.expanduser("~/.local/share/smiths-net/models/faster-whisper"),
)
FW_LANG = os.environ.get("FW_LANG", "ru")
FW_BEAM_SIZE = int(os.environ.get("FW_BEAM_SIZE", "5"))
FW_VAD = os.environ.get("FW_VAD", "1").lower() in ("1", "true", "yes", "on")
FW_INITIAL_PROMPT = os.environ.get("FW_INITIAL_PROMPT", "") or None
# Confidence gates — segments quieter/less certain than this are treated as
# non-speech and dropped (kills phantom output on silence/line-noise).
FW_NO_SPEECH_THRESHOLD = float(os.environ.get("FW_NO_SPEECH_THRESHOLD", "0.6"))
FW_LOGPROB_THRESHOLD = float(os.environ.get("FW_LOGPROB_THRESHOLD", "-0.7"))

import re as _re  # noqa: E402

# Whisper (large-v3) was trained on YouTube subtitles and emits "subtitle
# credit" phrases on silence/short noisy clips — "Субтитры создавал DimaTorzok",
# "Продолжение следует...", "Спасибо за просмотр" etc. These are never real
# caller speech, so we blank them out. Match on a normalized (lowercased,
# punctuation-stripped) form. Extend via FW_HALLUCINATION_EXTRA (|-separated).
_HALLUCINATION_PATTERNS = [
    r"dima\s*torzok",
    r"субтитр\w*\s+(созда|сдела|редакт|коррект|подготов)",
    r"редактор\s+субтитр",
    r"коррект\w+\s+а\.?\s*егоров",
    r"продолжение\s+следует",
    r"спасибо\s+за\s+просмотр",
    r"спасибо\s+за\s+внимание",
    r"подпис\w*\s+(на\s+канал|в\s+коммент)",
    r"ставьте\s+лайк",
    r"до\s+новых\s+встреч",
    r"редактор\s+\w+\s+семкин",
]
_extra = os.environ.get("FW_HALLUCINATION_EXTRA", "").strip()
if _extra:
    _HALLUCINATION_PATTERNS += [p for p in _extra.split("|") if p]
_HALLUCINATION_RE = _re.compile("|".join(_HALLUCINATION_PATTERNS))


def _normalize(text: str) -> str:
    t = text.lower().replace("ё", "е")
    t = _re.sub(r"[^\w\s.]", " ", t)
    return _re.sub(r"\s+", " ", t).strip()


def _is_hallucination(text: str) -> bool:
    norm = _normalize(text)
    if not norm:
        return False
    return bool(_HALLUCINATION_RE.search(norm))


def _default_compute_type() -> str:
    if os.environ.get("FW_COMPUTE_TYPE"):
        return os.environ["FW_COMPUTE_TYPE"]
    return "float16" if FW_DEVICE.startswith("cuda") else "int8"


FW_COMPUTE_TYPE = _default_compute_type()

DESCRIPTOR = {
    "capability": "ai.asr",
    "plugin": "ai-asr-faster-whisper",
    "model_id": f"faster-whisper-{Path(FW_MODEL).name}",
    "abi": "1.0",
    "description": "faster-whisper (CTranslate2) neural ASR on GPU.",
    # Above Vosk (18) and whisper.cpp (20) so the dispatcher prefers the
    # accurate GPU engine when several local ASRs are loaded.
    "priority": 22,
    "languages": ["ru", "ru-RU", "en", "auto"],
    "features": [],
    "input_formats": [
        {"codec": "pcm_s16le", "sample_rates": [8000, 16000]},
    ],
    "streaming": {"supported": False},
    "controls": {
        "language": {"type": "string", "default": FW_LANG},
        "beam_size": {"type": "integer", "minimum": 1, "maximum": 10, "default": FW_BEAM_SIZE},
    },
    "latency_ms": {"p50": 300, "p95": 900},
    "concurrency": {"max_in_flight": 1},
}

_model = None
_model_error = ""
_model_lock = threading.Lock()


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
    sys.stderr.write(f"[ai-asr-faster-whisper] {msg}\n")
    sys.stderr.flush()


def _load_model():
    """Load the CTranslate2 model once (slow) and cache it process-wide.

    Serialized: the background warmup thread and a first real transcribe
    can race here, and CTranslate2 model construction is not re-entrant.
    """
    global _model, _model_error
    if _model is not None or _model_error:
        return
    with _model_lock:
        if _model is not None or _model_error:
            return
        _load_model_locked()


def _load_model_locked():
    global _model, _model_error
    try:
        from faster_whisper import WhisperModel
    except Exception as e:  # noqa: BLE001
        _model_error = f"faster_whisper not importable: {e} (pip install faster-whisper)"
        return
    try:
        os.makedirs(FW_DOWNLOAD_ROOT, exist_ok=True)
        _model = WhisperModel(
            FW_MODEL,
            device=FW_DEVICE,
            compute_type=FW_COMPUTE_TYPE,
            download_root=FW_DOWNLOAD_ROOT,
        )
        log(f"model loaded: {FW_MODEL} ({FW_DEVICE}/{FW_COMPUTE_TYPE})")
    except Exception as e:  # noqa: BLE001
        # GPU OOM / missing CUDA libs → degrade to CPU int8 rather than dying.
        if FW_DEVICE.startswith("cuda"):
            try:
                from faster_whisper import WhisperModel

                log(f"cuda load failed ({e}); falling back to cpu/int8")
                _model = WhisperModel(
                    FW_MODEL, device="cpu", compute_type="int8",
                    download_root=FW_DOWNLOAD_ROOT,
                )
                return
            except Exception as e2:  # noqa: BLE001
                _model_error = f"model load failed (cuda+cpu): {e2}"
                return
        _model_error = f"model load failed: {e}"


def _pcm16_to_float32_16k(pcm: bytes, sample_rate: int):
    """Decode PCM16-LE mono → float32 [-1,1] @ 16 kHz numpy array.

    Whisper front-end expects 16 kHz; 8 kHz telephony is linearly
    resampled. NumPy ships with torch, so no extra dependency."""
    import numpy as np

    if not pcm:
        return np.zeros(0, dtype="float32")
    audio = np.frombuffer(pcm, dtype="<i2").astype("float32") / 32768.0
    if sample_rate != 16000 and audio.size:
        n_out = int(round(audio.size * 16000 / sample_rate))
        if n_out > 0:
            x_old = np.linspace(0.0, 1.0, num=audio.size, endpoint=False)
            x_new = np.linspace(0.0, 1.0, num=n_out, endpoint=False)
            audio = np.interp(x_new, x_old, audio).astype("float32")
    return audio


def transcribe(params: dict) -> dict:
    _load_model()
    if _model is None:
        raise RuntimeError(_model_error or "faster-whisper model unavailable")

    b64 = params.get("audio_base64")
    if not isinstance(b64, str):
        raise ValueError("`audio_base64` must be a string")
    try:
        pcm = base64.b64decode(b64)
    except Exception as e:
        raise ValueError(f"audio_base64 decode failed: {e}") from e

    sample_rate = int(params.get("sample_rate") or 8000)
    controls = params.get("controls") or {}
    language = params.get("language") or controls.get("language") or FW_LANG
    if language == "auto":
        language = None
    beam_size = int(controls.get("beam_size", FW_BEAM_SIZE))

    audio = _pcm16_to_float32_16k(pcm, sample_rate)

    segments, info = _model.transcribe(
        audio,
        language=language,
        beam_size=beam_size,
        vad_filter=FW_VAD,
        # Short, independent utterances: don't carry prior text or the
        # decoder hallucinates filler/repeats on near-silent clips.
        condition_on_previous_text=False,
        no_speech_threshold=FW_NO_SPEECH_THRESHOLD,
        log_prob_threshold=FW_LOGPROB_THRESHOLD,
        initial_prompt=FW_INITIAL_PROMPT,
    )
    parts = []
    avg_logprobs = []
    for seg in segments:
        # Drop low-confidence non-speech segments (silence/line-noise) that
        # slipped past the internal gate — a frequent hallucination source.
        if (
            seg.no_speech_prob is not None
            and seg.avg_logprob is not None
            and seg.no_speech_prob > FW_NO_SPEECH_THRESHOLD
            and seg.avg_logprob < FW_LOGPROB_THRESHOLD
        ):
            continue
        parts.append(seg.text)
        if seg.avg_logprob is not None:
            avg_logprobs.append(seg.avg_logprob)
    text = "".join(parts).strip()

    # Blank known subtitle-credit hallucinations ("DimaTorzok" et al.).
    if text and _is_hallucination(text):
        log(f"dropped hallucination: {text!r}")
        text = ""

    if not text:
        log(f"empty hypothesis: {len(pcm)} bytes @ {sample_rate} Hz")

    confidence = None
    if avg_logprobs:
        # avg_logprob is roughly in [-1, 0]; map to a 0..1 confidence.
        import math

        confidence = round(math.exp(sum(avg_logprobs) / len(avg_logprobs)), 3)

    return {
        "text": text,
        "language": getattr(info, "language", None) or (language or "ru"),
        "confidence": confidence,
        "duration_s": round(getattr(info, "duration", len(audio) / 16000.0), 3),
    }


def _warmup() -> None:
    """Eager-load the model and run one decode at boot so the first real
    utterance doesn't pay the cold-start cost (model load + CUDA init)."""
    _load_model()
    if _model is None:
        log(f"warmup skipped: {_model_error}")
        return
    try:
        import numpy as np

        list(_model.transcribe(np.zeros(16000, dtype="float32"), language="ru")[0])
        log("warmup complete")
    except Exception as e:  # noqa: BLE001
        log(f"warmup failed: {e}")


def main() -> int:
    log(f"ready (model={FW_MODEL} device={FW_DEVICE} compute={FW_COMPUTE_TYPE})")
    # Warm up in the background: loading large-v3 on the GPU takes tens of
    # seconds, and blocking here would stall the engine's startup
    # `describe_capabilities` probe (30 s budget) → the plugin gets dropped
    # from the registry and every transcribe returns "plugin not found".
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
