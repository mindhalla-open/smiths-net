#!/usr/bin/env python3
"""ASR voice bot — smiths-net + MCP plugins (fully offline stack).

Движок smiths-net запускается в режиме `--mcp stdio` и обслуживает
SIP/RTP. Вся AI-обработка идёт локально, на GPU, через MCP-инструменты:

    transcribe → ai-asr-faster-whisper  (Whisper large-v3, CTranslate2)
    llm_chat   → ai-llm-llamacpp        (llama.cpp + Gemma-2-9B-it)
    synthesize → ai-tts-silero          (Silero v5 Russian)

Плагины переопределяются через ASR_PLUGIN / LLM_PLUGIN / TTS_PLUGIN.

Режимы:

  **local** (по умолчанию для asr-bot.toml)
    Бот паркуется на rendezvous `voicebot`. Звонок из второго терминала:
      python3 examples/python-client/voice_caller.py --out tmp/asr-bot-reply.wav

  **trunk** (Megafon Multifon — multifon.toml + multifon.env)
    Исходящий REGISTER на sbc.megafon.ru, входящие INVITE на DID.
    Бот ждёт MCP `notifications/call/created`, подключается второй
    ногой к rendezvous-ключу (= SIP_NUMBER) и ведёт диалог.

    source examples/multifon.env
    PYTHONUNBUFFERED=1 python3 examples/python-client/asr_bot.py \\
        --mode trunk --config examples/multifon.toml
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import queue
import re
import subprocess
import sys
import threading
import time
import tomllib
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path

from sip_register import SipRegisterClient, SipRegisterConfig
from smiths_client import (
    SipUAC,
    pcm16_to_wire,
    wire_to_pcm16,
    write_wav_mono_pcm16,
)

RENDEZVOUS = "voicebot"
_RENDEZVOUS_LOG = re.compile(
    r"call_id=([^\s}]+).*rendezvous leg parked.*rendezvous=(\S+)"
)
_BRIDGE_LOG = re.compile(
    r"call_id=([^\s}]+).*rendezvous bridge established.*rendezvous=(\S+)"
)
# Megafon Multifon inbound call-ids always start with "SD".
_MEGAFON_CALL_RE = re.compile(r"^SD", re.IGNORECASE)


def resolve_plugins() -> dict[str, str]:
    """Select the MCP plugins that run the offline AI pipeline.

    The shipped stack is fully local/GPU:
        STT  → ai-asr-faster-whisper   (Whisper large-v3, CTranslate2)
        LLM  → ai-llm-llamacpp          (llama.cpp + Gemma-2-9B-it)
        TTS  → ai-tts-silero            (Silero v5 Russian)

    Each leg can be overridden via ASR_PLUGIN / LLM_PLUGIN / TTS_PLUGIN to
    swap in another sidecar (e.g. ai-tts-piper) without code changes.
    """
    return {
        "asr": os.environ.get("ASR_PLUGIN", "ai-asr-faster-whisper").strip(),
        "llm": os.environ.get("LLM_PLUGIN", "ai-llm-llamacpp").strip(),
        "tts": os.environ.get("TTS_PLUGIN", "ai-tts-silero").strip(),
    }


def vad_record_kwargs() -> dict:
    """Energy-based end-of-utterance detection for RTP capture."""
    return {
        "vad": os.environ.get("VAD_ENABLED", "1") != "0",
        "silence_secs": float(os.environ.get("VAD_SILENCE_SECS", "0.8")),
        "vad_threshold": float(os.environ.get("VAD_THRESHOLD", "250")),
        "min_speech_secs": float(os.environ.get("VAD_MIN_SPEECH_SECS", "0.20")),
        "pre_speech_secs": float(os.environ.get("VAD_PRE_SPEECH_SECS", "0.30")),
    }


def load_env_file(path: Path) -> None:
    """Load KEY=VALUE lines into os.environ (skip comments / blanks)."""
    if not path.is_file():
        return
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if "=" not in line:
            continue
        key, _, val = line.partition("=")
        key = key.strip()
        val = val.strip().strip("'\"")
        if key and key not in os.environ:
            os.environ[key] = val


@dataclass(frozen=True)
class BotConfig:
    gender: str
    greeting: str
    system_prompt: str


_BOT_CFG: BotConfig | None = None

_DEFAULT_PROMPT = (
    "Ты — вежливая молодая женщина, голосовой ассистент. Говоришь с человеком "
    "по телефону на русском языке от своего лица — всегда в женском роде. "
    "Веди живой, естественный диалог короткими фразами. "
    "Приветствие уже прозвучало — не здоровайся повторно. "
    "Для завершения разговора добавь [ОТБОЙ] в конец ответа."
)


def _norm_prompt(text: str) -> str:
    return re.sub(r"\s+", " ", (text or "").strip())


def load_bot_config(path: Path) -> BotConfig:
    """Load assistant persona from examples/bot.toml (or BOT_CONFIG path)."""
    gender = "female"
    greeting = "Здравствуйте! Чем могу помочь?"
    system_prompt = _DEFAULT_PROMPT

    if path.is_file():
        with path.open("rb") as f:
            data = tomllib.load(f)
        bot = data.get("bot") or {}
        gender = str(bot.get("gender", gender)).strip().lower() or gender
        greeting = str(bot.get("greeting", greeting)).strip() or greeting

        prompt = data.get("prompt") or {}
        base = _norm_prompt(str(prompt.get("base", "")))
        gender_blocks = data.get("gender") or {}
        block = gender_blocks.get(gender) or {}
        instruction = _norm_prompt(str(block.get("instruction", "")))
        # Legacy single-field prompt (system = ...) still supported.
        legacy = _norm_prompt(str(prompt.get("system", "")))

        if base and instruction:
            system_prompt = f"{instruction} {base}"
        elif legacy:
            system_prompt = legacy
        elif base:
            system_prompt = base
        elif instruction:
            system_prompt = instruction

    return BotConfig(
        gender=gender,
        greeting=greeting,
        system_prompt=system_prompt,
    )


def init_bot_config(path: Path | None = None) -> BotConfig:
    """Load bot.toml once; env overrides apply at read time."""
    global _BOT_CFG
    cfg_path = path or Path(os.environ.get("BOT_CONFIG", "examples/bot.toml"))
    _BOT_CFG = load_bot_config(cfg_path)
    return _BOT_CFG


def bot_config() -> BotConfig:
    if _BOT_CFG is None:
        return init_bot_config()
    return _BOT_CFG


def system_prompt() -> str:
    env = os.environ.get("BOT_SYSTEM_PROMPT", "").strip()
    if env:
        return env
    return bot_config().system_prompt


def greeting_text() -> str:
    return os.environ.get("BOT_GREETING", "").strip() or bot_config().greeting


def _feminize_enabled() -> bool:
    env = os.environ.get("BOT_FEMINIZE", "").strip().lower()
    if env in ("0", "false", "no", "off"):
        return False
    if env in ("1", "true", "yes", "on"):
        return True
    return bot_config().gender == "female"


def preferred_codec() -> str:
    return os.environ.get("SIP_PREFERRED_CODEC", "PCMU").lower()


def payload_type_for_codec(codec: str) -> int:
    return 8 if codec in ("pcma", "alaw", "g711a") else 0


class Mcp:
    """MCP stdio-клиент с фоновым чтением notifications."""

    def __init__(self, cmd: list[str], on_notification=None) -> None:
        self.proc = subprocess.Popen(
            cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE
        )
        self._id = 0
        self._send_lock = threading.Lock()
        self._pending: dict[int, tuple[threading.Event, list]] = {}
        self._pending_lock = threading.Lock()
        self._ready = threading.Event()
        self._stop = threading.Event()
        self.on_notification = on_notification
        self.rendezvous_by_call: dict[str, str] = {}
        threading.Thread(target=self._drain_stderr, daemon=True).start()
        threading.Thread(target=self._read_stdout, daemon=True).start()

    def _drain_stderr(self) -> None:
        assert self.proc.stderr is not None
        for line in self.proc.stderr:
            text = line.decode(errors="replace").rstrip()
            if text:
                m = _RENDEZVOUS_LOG.search(text)
                if m:
                    self.rendezvous_by_call[m.group(1)] = m.group(2)
                m = _BRIDGE_LOG.search(text)
                if m:
                    self.rendezvous_by_call[m.group(1)] = m.group(2)
                print(f"[engine] {text}")

    def _read_stdout(self) -> None:
        assert self.proc.stdout is not None
        while not self._stop.is_set():
            line = self.proc.stdout.readline()
            if not line:
                break
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if "id" in msg:
                rid = msg.get("id")
                if rid == 1:
                    self._notify("notifications/initialized")
                    self._ready.set()
                with self._pending_lock:
                    slot = self._pending.pop(rid, None)
                if slot is not None:
                    event, bucket = slot
                    bucket.append(msg)
                    event.set()
                continue
            method = msg.get("method", "")
            if method.startswith("notifications/") and self.on_notification:
                self.on_notification(method, msg.get("params") or {})

    def wait_ready(self, timeout: float = 10.0) -> bool:
        self._rpc("initialize", {
            "protocolVersion": "2024-11-05",
            "clientInfo": {"name": "asr_bot.py", "version": "0.2"},
            "capabilities": {},
        })
        return self._ready.wait(timeout)

    def _notify(self, method: str) -> None:
        assert self.proc.stdin
        with self._send_lock:
            self.proc.stdin.write(
                (json.dumps({"jsonrpc": "2.0", "method": method}) + "\n").encode()
            )
            self.proc.stdin.flush()

    def _rpc(self, method: str, params: dict | None = None, *, timeout: float = 120.0) -> dict:
        assert self.proc.stdin
        event = threading.Event()
        bucket: list = []
        with self._send_lock:
            self._id += 1
            rid = self._id
            frame = {"jsonrpc": "2.0", "id": rid, "method": method}
            if params is not None:
                frame["params"] = params
            self.proc.stdin.write((json.dumps(frame) + "\n").encode())
            self.proc.stdin.flush()
        with self._pending_lock:
            self._pending[rid] = (event, bucket)
        if not event.wait(timeout):
            with self._pending_lock:
                self._pending.pop(rid, None)
            raise TimeoutError(f"MCP {method} timed out after {timeout}s")
        msg = bucket[0]
        if "error" in msg:
            raise RuntimeError(f"MCP error: {msg['error']}")
        return msg.get("result", {})

    def tool(self, name: str, arguments: dict) -> dict:
        result = self._rpc("tools/call", {"name": name, "arguments": arguments})
        if result.get("isError"):
            raise RuntimeError(f"tool {name} failed: {result}")
        return result.get("structuredContent") or {}

    def close(self) -> None:
        self._stop.set()
        if self.proc.stdin:
            try:
                self.proc.stdin.close()
            except BrokenPipeError:
                pass
        try:
            self.proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.proc.kill()


# Pre-rendered greeting audio, keyed by RTP payload type (0=PCMU, 8=PCMA).
# Filled by preload_greeting() at startup so the first turn has zero TTS
# latency — the caller hears the prompt the instant the bridge is up.
_GREETING_WIRE: dict[int, bytes] = {}


def preload_greeting(mcp: Mcp, plugins: dict[str, str], pt: int) -> None:
    """Synthesize the greeting once at startup and cache the wire bytes."""
    text = greeting_text()
    try:
        wire = synthesize_wire(mcp, text, pt=pt, plugins=plugins)
    except Exception as e:
        print(f"[bot] greeting preload failed ({e}) — will synth on first call")
        return
    if wire:
        _GREETING_WIRE[pt] = wire
        print(f"[bot] greeting preloaded: {len(wire) / 8000:.2f} s pt={pt} {text!r}")


# Pre-rendered short "filler" / backchannel phrases, keyed by payload
# type. Played instantly (zero network latency) right after the caller
# finishes, masking the LLM + TTS-connect pause behind a natural
# acknowledgement instead of dead silence.
_FILLER_WIRES: dict[int, list[bytes]] = {}
_filler_idx = 0


def _fillers_enabled() -> bool:
    # Off by default: with a local GPU LLM the answer arrives in ~0.1-0.4 s,
    # so a "Секундочку…" filler only adds an awkward, often-irrelevant delay.
    return os.environ.get("BOT_FILLERS_ENABLE", "0").lower() in ("1", "true", "yes", "on")


def filler_phrases() -> list[str]:
    raw = os.environ.get("BOT_FILLERS", "Секундочку.|Так, минутку.|Сейчас уточню.")
    return [p.strip() for p in raw.split("|") if p.strip()]


def preload_fillers(mcp: Mcp, plugins: dict[str, str], pt: int) -> None:
    """Synthesize filler phrases once at startup and cache the wire bytes."""
    if not _fillers_enabled():
        return
    wires: list[bytes] = []
    for phrase in filler_phrases():
        try:
            w = synthesize_wire(mcp, phrase, pt=pt, plugins=plugins)
        except Exception as e:  # noqa: BLE001
            print(f"[bot] filler preload failed ({e})")
            continue
        if w:
            wires.append(w)
    if wires:
        _FILLER_WIRES[pt] = wires
        total = sum(len(w) for w in wires) / 8000.0
        print(f"[bot] fillers preloaded: {len(wires)} phrases ({total:.1f}s total) pt={pt}")


def play_filler(uac: SipUAC) -> bool:
    """Play the next cached filler (rotating). Returns True if audio sent."""
    global _filler_idx
    wires = _FILLER_WIRES.get(uac.payload_type)
    if not wires or not uac.engine_rtp:
        return False
    wire = wires[_filler_idx % len(wires)]
    _filler_idx += 1
    try:
        uac.stream_wire(wire)
        return True
    except OSError as e:
        print(f"[bot] filler send failed ({e})")
        return False


_GOODBYE_RE = re.compile(
    r"(до\s*свидан|до\s*встреч|всего\s*добр|спасибо.{0,6}(всё|все|до)|отбой|прощай|кладу трубк)",
    re.IGNORECASE,
)


def _is_goodbye(text: str) -> bool:
    return bool(_GOODBYE_RE.search(text or ""))


# Hang-up skill: the LLM appends this marker to its reply when it decides the
# conversation is over (caller said bye / thanked off / asked to end the call,
# or the task is clearly done). The bot strips the marker from the spoken text,
# plays the farewell, then drops the call via SIP BYE. Tolerant of casing,
# surrounding punctuation/whitespace, and a few natural variants the model emits.
_END_CALL_RE = re.compile(
    r"\s*[\[<«(]?\s*(?:ОТБОЙ|КОНЕЦ[ _]?РАЗГОВОРА|END[ _]?CALL|HANGUP|"
    r"ЗАВЕРШИТЬ[ _]?ЗВОНОК)"
    r"\s*[\]>».)]?\s*",
    re.IGNORECASE,
)


def _split_end_signal(text: str) -> tuple[str, bool]:
    """Return (spoken_text_without_marker, should_hang_up)."""
    if not text:
        return "", False
    if not _END_CALL_RE.search(text):
        return text, False
    cleaned = _END_CALL_RE.sub(" ", text)
    cleaned = re.sub(r"\s+", " ", cleaned).strip()
    return cleaned, True


def _stt_once(mcp: Mcp, audio: bytes, *, codec: str | None, plugin: str) -> str:
    params: dict = {
        "plugin": plugin,
        "audio_base64": base64.b64encode(audio).decode("ascii"),
        "sample_rate": 8000,
        "language": "ru",
    }
    if codec:
        params["codec"] = codec
    stt = mcp.tool("transcribe", params)
    return (stt.get("text") or "").strip()


def stt_transcribe(mcp: Mcp, wire: bytes, *, pt: int, plugins: dict[str, str]) -> str:
    """Recognize captured G.711 audio.

    Always send PCM16 — the representation the recognizer handles most
    reliably (the native PCMA→PCM16 decode is lossless). SaluteSpeech
    still blanks occasionally, so one cheap retry on the same buffer
    covers transient empties without burning a slow extra round-trip on
    a guaranteed-doomed alternate format.
    """
    pcm16 = wire_to_pcm16(wire, payload_type=pt)
    retries = int(os.environ.get("STT_RETRIES", "1"))

    for i in range(retries + 1):
        try:
            text = _stt_once(mcp, pcm16, codec=None, plugin=plugins["asr"])
        except Exception as e:
            print(f"[bot] STT attempt {i + 1} error: {e}")
            continue
        if text:
            if i:
                print(f"[bot] STT recovered on attempt {i + 1}")
            return text
    return ""


def _llm_messages(history: list[dict], plugins: dict[str, str]) -> list[dict]:
    """Adapt dialogue history for the active LLM backend.

    Gemma (llama.cpp) chat templates require strict user/assistant
    alternation — merge duplicate system lines and drop stray leading
    assistant turns (should not happen; greeting is not in history).
    """
    if plugins.get("llm") != "ai-llm-llamacpp":
        return history

    sys_parts: list[str] = []
    rest: list[dict] = []
    for m in history:
        if m.get("role") == "system":
            c = (m.get("content") or "").strip()
            if c:
                sys_parts.append(c)
        else:
            rest.append(dict(m))

    while rest and rest[0].get("role") == "assistant":
        rest.pop(0)

    out: list[dict] = []
    if sys_parts:
        out.append({"role": "system", "content": "\n\n".join(sys_parts)})
    out.extend(rest)
    return out


def _trim_history(history: list[dict]) -> list[dict]:
    """Keep system + the last N dialogue turns.

    A short, stable prefix keeps llama.cpp prompt-cache reuse effective
    (fast warm prefill) and bounds latency growth over a long call.
    N counts user/assistant *messages* (default 6 ≈ 3 exchanges).
    """
    keep = int(os.environ.get("LLM_HISTORY_MSGS", "6"))
    system = [m for m in history if m.get("role") == "system"]
    convo = [m for m in history if m.get("role") != "system"]
    if keep > 0 and len(convo) > keep:
        convo = convo[-keep:]
    return system + convo


# A greeting already played at call start, so a second "Здравствуйте" from the
# model sounds odd. The system prompt forbids it, but the model still slips one
# in, so we strip a leading greeting clause (and a trailing "чем могу помочь"
# style opener) deterministically — belt and braces.
_GREETING_LEAD_RE = re.compile(
    r"^[\s,!.…—–-]*(?:"
    r"здравствуйте|здравствуй|"
    r"привет(?:ствую(?:\s+вас)?)?|"
    r"доброе\s+утро|"
    r"добрый\s+(?:день|вечер)|"
    r"доброго\s+(?:утра|дня|вечера|времени\s+суток)|"
    r"рад[ао]?\s+(?:вас\s+)?(?:приветствовать|слышать)(?:\s+вас)?"
    r")[\s,!.…—–-]*",
    re.IGNORECASE,
)
_OPENING_FILLER_RE = re.compile(
    r"^[\s,!.…—–-]*(?:"
    r"чем(?:\s+(?:я|вам))?\s+могу\s+(?:вам\s+)?(?:помочь|быть\s+полезн[аыо]й?)|"
    r"как(?:\s+я)?\s+могу\s+(?:вам\s+)?помочь|"
    r"чем\s+могу\s+быть\s+полезн[аыо]й?|"
    r"слушаю\s+вас"
    r")[\s,!.…?—–-]*",
    re.IGNORECASE,
)


def _strip_leading_greeting(text: str) -> str:
    """Remove a redundant opening greeting / "чем могу помочь" from a reply."""
    out = text
    changed = True
    while changed:
        changed = False
        m = _GREETING_LEAD_RE.match(out)
        if m and m.end() > 0:
            out = out[m.end():]
            changed = True
        m = _OPENING_FILLER_RE.match(out)
        if m and m.end() > 0:
            out = out[m.end():]
            changed = True
    out = out.strip()
    if not out:
        # Reply was nothing but a greeting — keep the original so we still say
        # something rather than falling through to an error apology.
        return text.strip()
    # Re-capitalize: we chopped off the original first sentence.
    return out[0].upper() + out[1:]


# Gemma (and other LLMs) still slip masculine self-reference despite the prompt.
# Fix the common telephony patterns deterministically before TTS — only forms
# that refer to the assistant herself (first person), not the caller.
_FEM_ADJ_AFTER_YA = re.compile(
    r"(?<=\bя\s)(готов|рад|согласен|уверен|должен|вынужден|способен|"
    r"обязан|расположен)\b",
    re.IGNORECASE,
)
_FEM_ADJ_FUTURE = re.compile(
    r"(?<=\bбуду\s)(готов|рад)\b",
    re.IGNORECASE,
)
_FEM_ADJ_MAP = {
    "готов": "готова",
    "рад": "рада",
    "согласен": "согласна",
    "уверен": "уверена",
    "должен": "должна",
    "вынужден": "вынуждена",
    "способен": "способна",
    "обязан": "обязана",
    "расположен": "расположена",
}
_FEM_PAST_AFTER_YA = re.compile(r"(?<=\bя\s)(\w+)л\b", re.IGNORECASE)
_FEM_IRREGULAR = [
    (re.compile(r"(?<=\bя\s)был\b", re.I), "была"),
    (re.compile(r"(?<=\bя\s)мог\b", re.I), "могла"),
    (re.compile(r"(?<=\bя\s)пошёл\b", re.I), "пошла"),
    (re.compile(r"(?<=\bя\s)пошел\b", re.I), "пошла"),
    (re.compile(r"(?<=\bя\s)хотел\b", re.I), "хотела"),
    (re.compile(r"(?<=\b)рад(\s+помочь)", re.I), r"рада\1"),
    (re.compile(r"(?<=\b)готов(\s+помочь)", re.I), r"готова\1"),
]


def _cap_like(orig: str, new: str) -> str:
    """Preserve capitalisation of a replaced token."""
    if orig[:1].isupper():
        return new[:1].upper() + new[1:]
    return new


def _feminize_self_ref(text: str) -> str:
    """Rewrite masculine first-person forms to feminine for the assistant."""
    if not _feminize_enabled():
        return text

    for pat, repl in _FEM_IRREGULAR:
        text = pat.sub(repl, text)

    def _adj(m: re.Match) -> str:
        w = m.group(1)
        fem = _FEM_ADJ_MAP.get(w.lower(), w)
        return _cap_like(w, fem)

    text = _FEM_ADJ_AFTER_YA.sub(_adj, text)
    text = _FEM_ADJ_FUTURE.sub(_adj, text)

    def _past(m: re.Match) -> str:
        stem = m.group(1)
        # Already feminine or not a typical past-tense stem — leave alone.
        if stem.lower().endswith(("ла", "ло", "ли", "на", "та", "да")):
            return m.group(0)
        return _cap_like(stem + "л", stem + "ла")

    text = _FEM_PAST_AFTER_YA.sub(_past, text)
    return text


def _clean_llm_reply(text: str) -> str:
    """Strip emoji / odd chars unsuitable for telephony TTS."""
    text = (text or "").strip()
    if not text:
        return ""
    text = re.sub(
        r"[\U0001F300-\U0001FAFF\U00002600-\U000027BF\U0000FE00-\U0000FE0F]",
        "",
        text,
    )
    text = re.sub(r"\s+", " ", text).strip()
    text = _strip_leading_greeting(text)
    text = _feminize_self_ref(text)
    if text and text[0].islower():
        text = text[0].upper() + text[1:]
    return text


def llm_reply(mcp: Mcp, history: list[dict], *, plugins: dict[str, str]) -> str:
    # Cap output tokens — shorter answers mean less LLM time AND a shorter
    # TTS render, both of which cut perceived latency on the call.
    max_tokens = int(os.environ.get("LLM_MAX_TOKENS", "96"))
    chat = mcp.tool("llm_chat", {
        "plugin": plugins["llm"],
        "messages": _llm_messages(_trim_history(history), plugins),
        "controls": {"temperature": 0.3, "max_tokens": max_tokens},
    })
    return _clean_llm_reply((chat.get("message") or {}).get("content") or "")


def warmup_llm(mcp: Mcp, plugins: dict[str, str]) -> None:
    """Fire a tiny chat at startup so the first real turn isn't cold.

    The Vulkan/CUDA backend compiles shaders + fills KV on the first
    request (~4 s cold); subsequent warm calls are ~80 ms. Paying that
    cost before any call lands keeps turn 1 snappy.
    """
    if os.environ.get("LLM_WARMUP", "1").lower() not in ("1", "true", "yes", "on"):
        return
    try:
        t0 = time.monotonic()
        mcp.tool("llm_chat", {
            "plugin": plugins["llm"],
            "messages": [
                {"role": "system", "content": system_prompt()},
                {"role": "user", "content": "привет"},
            ],
            "controls": {"temperature": 0.3, "max_tokens": 8},
        })
        print(f"[bot] LLM warmup done in {(time.monotonic() - t0) * 1000:.0f} ms")
    except Exception as e:  # noqa: BLE001
        print(f"[bot] LLM warmup skipped ({e})")


def _tts_voice(plugins: dict[str, str]) -> str:
    tts = plugins["tts"]
    if tts == "ai-tts-silero":
        return os.environ.get("SILERO_SPEAKER", "xenia")
    if tts == "ai-tts-piper":
        return os.environ.get("PIPER_VOICE_ID", "irina")
    return "irina"


def synthesize_wire(mcp: Mcp, text: str, *, pt: int, plugins: dict[str, str]) -> bytes | None:
    tts = mcp.tool("synthesize", {
        "plugin": plugins["tts"],
        "text": text,
        "voice": _tts_voice(plugins),
        "output": {"codec": "pcm_s16le", "sample_rate": 8000},
    })
    audio = base64.b64decode(tts.get("audio_base64") or "")
    if not audio:
        return None
    return pcm16_to_wire(audio, payload_type=pt)


def _barge_in_enabled() -> bool:
    return os.environ.get("BARGE_IN", "1").lower() in ("1", "true", "yes", "on")


def speak(
    uac: SipUAC, mcp: Mcp, text: str, *, plugins: dict[str, str], allow_barge_in: bool = False
) -> float:
    """Synthesize `text` and stream it to the caller as RTP.

    Returns playback duration in seconds (0 if nothing played). Sets
    ``uac.last_barge_in`` when the caller interrupts.
    """
    uac.last_barge_in = False
    if not text:
        return 0.0
    if not uac.engine_rtp:
        print("[bot] cannot speak: no negotiated RTP endpoint")
        return 0.0
    try:
        wire = synthesize_wire(mcp, text, pt=uac.payload_type, plugins=plugins)
    except Exception as e:
        print(f"[bot] TTS failed: {e}")
        return 0.0
    if not wire:
        print("[bot] TTS produced no audio")
        return 0.0
    secs = len(wire) / 8000.0
    print(f"[bot] TTS  : {secs:.2f} s → RTP {uac.engine_rtp}")
    try:
        interrupted = uac.stream_wire(
            wire, allow_barge_in=allow_barge_in and _barge_in_enabled()
        )
        if interrupted:
            print("[bot] barge-in: caller started speaking, stopped playback")
    except OSError as e:
        print(f"[bot] RTP send failed ({e}) → {uac.engine_rtp}")
        return 0.0
    return secs


def _split_sentences(text: str) -> list[str]:
    """Split a reply into sentence-ish chunks for pipelined synthesis.

    Short tails are merged into the previous chunk so we don't waste a
    full edge-tts connect (~1.8 s) on a 2-word fragment.
    """
    text = (text or "").strip()
    if not text:
        return []
    raw = re.split(r"(?<=[.!?…])\s+", text)
    parts: list[str] = []
    for p in raw:
        p = p.strip()
        if not p:
            continue
        if parts and (len(parts[-1]) < 16 or len(p) < 16):
            parts[-1] = f"{parts[-1]} {p}"
        else:
            parts.append(p)
    return parts


def speak_streaming(
    uac: SipUAC, mcp: Mcp, text: str, *, plugins: dict[str, str], max_workers: int = 4,
    allow_barge_in: bool = False,
) -> float:
    """Stream a reply with low time-to-first-audio.

    Returns total playback duration in seconds. Sets ``uac.last_barge_in``
    and stops sending remaining chunks if the caller interrupts.
    """
    uac.last_barge_in = False
    if not text:
        return 0.0
    if not uac.engine_rtp:
        print("[bot] cannot speak: no negotiated RTP endpoint")
        return 0.0
    sents = _split_sentences(text)
    if len(sents) <= 1:
        return speak(uac, mcp, text, plugins=plugins, allow_barge_in=allow_barge_in)

    barge = allow_barge_in and _barge_in_enabled()
    pt = uac.payload_type
    print(f"[bot] TTS streaming {len(sents)} chunks → RTP {uac.engine_rtp}")
    t0 = time.monotonic()
    played_secs = 0.0
    with ThreadPoolExecutor(max_workers=min(max_workers, len(sents))) as ex:
        futures = [
            ex.submit(synthesize_wire, mcp, s, pt=pt, plugins=plugins) for s in sents
        ]
        for i, fut in enumerate(futures):
            try:
                wire = fut.result()
            except Exception as e:  # noqa: BLE001
                print(f"[bot] TTS chunk {i + 1}/{len(sents)} failed: {e}")
                continue
            if not wire:
                continue
            if i == 0:
                print(f"[bot] first chunk ready in {(time.monotonic() - t0) * 1000:.0f} ms")
            try:
                interrupted = uac.stream_wire(wire, allow_barge_in=barge)
                played_secs += len(wire) / 8000.0
                if interrupted:
                    print("[bot] barge-in: caller interrupted, stopped reply")
                    break
            except OSError as e:
                print(f"[bot] RTP send failed ({e})")
                break
    return played_secs


def _pause_after_playback(uac: SipUAC, playback_secs: float, *, kind: str = "tts") -> None:
    """Brief pause with RTP keepalive — do not drain inbound packets (caller may already be speaking)."""
    if kind == "greeting":
        base = float(os.environ.get("POST_GREETING_PAUSE_SECS", "0.5"))
    else:
        base = float(os.environ.get("POST_PLAYBACK_PAUSE_SECS", "0.3"))
    secs = base + min(0.2, playback_secs * 0.05)
    uac.pause_with_keepalive(secs)
    print(f"[bot] pause after {kind}: {secs:.1f}s (RTP keepalive)")


def _dump_wire(wire: bytes, pt: int, path: str) -> None:
    try:
        Path(path).parent.mkdir(parents=True, exist_ok=True)
        write_wav_mono_pcm16(path, wire_to_pcm16(wire, payload_type=pt), sample_rate=8000)
        print(f"[bot] saved capture → {path}")
    except Exception as e:
        print(f"[bot] capture dump failed: {e}")


class _SpeculativeReply:
    """Prepare a turn's answer during the end-of-utterance silence.

    Fired when the caller has paused (the provisional VAD threshold) but
    before the pause is long enough to commit the turn. Runs STT → LLM →
    TTS-presynth on the captured-so-far audio in a background thread so the
    answer is ready the instant the pause becomes a real end-of-turn. If the
    caller resumes talking the work is aborted (``abort``) and discarded —
    the snapshot was a partial phrase. Reused only when the VAD reports
    ``last_speculation_valid`` (fired and not invalidated by a resume).
    """

    def __init__(self, mcp: Mcp, uac: SipUAC, history: list[dict], plugins: dict[str, str]) -> None:
        self._mcp = mcp
        self._uac = uac
        self._history = list(history)
        self._plugins = plugins
        self._cancel = threading.Event()
        self.done = threading.Event()
        self.transcript: str | None = None
        self.reply: str | None = None
        self.tts_wire: bytes | None = None
        self.is_goodbye = False
        self.wants_end = False
        self._thread: threading.Thread | None = None

    def start(self, snapshot_wire: bytes) -> None:
        self._thread = threading.Thread(
            target=self._work, args=(snapshot_wire,), daemon=True
        )
        self._thread.start()

    def abort(self) -> None:
        self._cancel.set()

    @property
    def aborted(self) -> bool:
        return self._cancel.is_set()

    def _work(self, wire: bytes) -> None:
        try:
            if self._cancel.is_set():
                return
            try:
                transcript = stt_transcribe(
                    self._mcp, wire, pt=self._uac.payload_type, plugins=self._plugins
                )
            except Exception as e:  # noqa: BLE001
                print(f"[bot] speculative STT failed: {e}")
                return
            if self._cancel.is_set():
                return
            self.transcript = transcript
            if not transcript:
                return
            if _is_goodbye(transcript):
                self.is_goodbye = True
                return
            msgs = self._history + [{"role": "user", "content": transcript}]
            try:
                reply = llm_reply(self._mcp, msgs, plugins=self._plugins)
            except Exception as e:  # noqa: BLE001
                print(f"[bot] speculative LLM failed: {e}")
                return
            if self._cancel.is_set():
                return
            # Strip the [ОТБОЙ] hang-up marker before synth so it's never spoken.
            spoken, self.wants_end = _split_end_signal(reply or "")
            self.reply = spoken
            if self._cancel.is_set() or not self.reply:
                return
            # Pre-render the audio too, so commit → playback is instantaneous.
            try:
                self.tts_wire = synthesize_wire(
                    self._mcp, self.reply, pt=self._uac.payload_type, plugins=self._plugins
                )
            except Exception as e:  # noqa: BLE001
                print(f"[bot] speculative TTS failed: {e}")
                self.tts_wire = None
        finally:
            self.done.set()


def _speculate_silence_secs() -> float:
    """Provisional-pause threshold for speculative reply prep (0 = disabled)."""
    return float(os.environ.get("VAD_SPECULATE_SILENCE_SECS", "0.35"))


def _silence_wire(pt: int, secs: float, frame_samples: int = 160) -> bytes:
    """G.711 digital-silence payload of `secs` (A-law 0xD5 / µ-law 0xFF)."""
    if secs <= 0:
        return b""
    byte = 0xD5 if pt == 8 else 0xFF
    frames = max(1, int(secs * 8000.0 / frame_samples))
    return bytes([byte]) * (frames * frame_samples)


def _play_prerendered_wire(uac: SipUAC, wire: bytes) -> float:
    """Stream an already-synthesized reply (from speculative prep) as RTP."""
    uac.last_barge_in = False
    if not wire or not uac.engine_rtp:
        return 0.0
    secs = len(wire) / 8000.0
    print(f"[bot] TTS* : {secs:.2f} s (pre-rendered) → RTP {uac.engine_rtp}")
    try:
        interrupted = uac.stream_wire(wire, allow_barge_in=_barge_in_enabled())
        if interrupted:
            print("[bot] barge-in: caller interrupted, stopped reply")
    except OSError as e:
        print(f"[bot] RTP send failed ({e})")
        return 0.0
    return secs


def converse(
    uac: SipUAC,
    mcp: Mcp,
    *,
    plugins: dict[str, str],
    record_secs: float,
    greet_first: bool,
    max_turns: int | None = None,
    idle_turns_max: int | None = None,
    debug_prefix: str | None = None,
) -> None:
    """Full-duplex-ish dialogue loop: (greeting) → [VAD record → STT → LLM → TTS]*."""
    # Turn cap was 8 — real conversations ran past it and the bot "stopped
    # answering" mid-call. Default high so the call ends on goodbye/hang-up/idle,
    # not an arbitrary counter; still bounded to avoid a runaway loop.
    if max_turns is None:
        max_turns = int(os.environ.get("BOT_MAX_TURNS", "200"))
    # How many consecutive silent turns before giving up ("вас не слышно").
    # 2 was too trigger-happy on a shaky start; allow one more grace turn.
    if idle_turns_max is None:
        idle_turns_max = int(os.environ.get("BOT_IDLE_TURNS_MAX", "3"))

    history: list[dict] = [{"role": "system", "content": system_prompt()}]
    vad_kwargs = vad_record_kwargs()
    thr = float(vad_kwargs.get("vad_threshold", 400.0))
    speculate_secs = _speculate_silence_secs()

    greeting_secs = 0.0
    if greet_first:
        greeting = greeting_text()
        # Lead-in silence: on symmetric-RTP trunks the relay/carrier pinhole may
        # still be latching when the greeting starts, clipping the first word
        # ("иногда не слышно начала"). A short silence pad absorbs that clip so
        # the greeting itself is always heard intact. Tunable; 0 to disable.
        lead = _silence_wire(
            uac.payload_type, float(os.environ.get("GREETING_LEAD_SILENCE_SECS", "0.3"))
        )
        cached = _GREETING_WIRE.get(uac.payload_type)
        if cached and uac.engine_rtp:
            greeting_secs = len(cached) / 8000.0
            print(f"[bot] greeting (preloaded): {greeting!r}")
            try:
                if uac.stream_wire(lead + cached, allow_barge_in=_barge_in_enabled()):
                    print("[bot] barge-in: caller spoke over greeting")
            except OSError as e:
                print(f"[bot] greeting send failed ({e})")
        else:
            print(f"[bot] greeting (live TTS): {greeting!r}")
            if lead:
                try:
                    uac.stream_wire(lead)
                except OSError:
                    pass
            greeting_secs = speak(uac, mcp, greeting, plugins=plugins, allow_barge_in=True)
        if not uac.last_barge_in:
            _pause_after_playback(uac, greeting_secs, kind="greeting")

    idle = 0
    for turn in range(1, max_turns + 1):
        print(f"[bot] turn {turn}: listening (VAD, thr={thr:.0f})…")

        # Speculative turn-taking: prepare the reply during the pause, discard
        # it if the caller keeps talking. The callbacks run inside the RTP
        # capture loop, so they must be cheap (spawn/abort a worker only).
        spec_box: dict[str, _SpeculativeReply | None] = {"cur": None}

        def _on_speculate(snapshot_wire: bytes) -> None:
            prev = spec_box["cur"]
            if prev is not None and not prev.done.is_set():
                prev.abort()
            snap_secs = len(snapshot_wire) / 8000.0
            spec = _SpeculativeReply(mcp, uac, history, plugins)
            spec_box["cur"] = spec
            spec.start(snapshot_wire)
            print(f"[bot] speculate: drafting reply on {snap_secs:.2f}s of speech…")

        def _on_resume() -> None:
            cur = spec_box["cur"]
            if cur is not None:
                cur.abort()
            print("[bot] speculate: caller resumed — discarding draft")

        wire = uac.record_wire(
            max_seconds=record_secs,
            speculate_silence_secs=speculate_secs,
            on_speculate=_on_speculate if speculate_secs > 0 else None,
            on_resume=_on_resume if speculate_secs > 0 else None,
            **vad_kwargs,
        )
        secs = len(wire) / 8000.0
        print(
            f"[bot] VAD  : speech={uac.last_speech_detected} "
            f"speech_dur={uac.last_speech_secs:.2f}s captured={secs:.2f}s "
            f"rtp_pkts={uac.last_rtp_pkts} peak={uac.last_peak:.0f} rms={uac.last_rms:.0f} "
            f"spec={uac.last_speculation_valid}"
        )
        if debug_prefix and wire:
            _dump_wire(wire, uac.payload_type, f"{debug_prefix}-turn{turn}.wav")

        # Reuse the speculative draft only if the pause committed without the
        # caller resuming (the snapshot then matches the final utterance).
        spec = spec_box["cur"]
        used_spec = False
        transcript = ""
        reply: str | None = None
        pre_wire: bytes | None = None
        wants_end = False
        if spec is not None and uac.last_speculation_valid and not spec.aborted:
            spec.done.wait(timeout=record_secs)
            if not spec.aborted and spec.transcript is not None:
                transcript = spec.transcript
                reply = spec.reply
                pre_wire = spec.tts_wire
                wants_end = spec.wants_end
                used_spec = True
                print(f"[bot] STT* : {transcript!r} (speculative)")

        if not used_spec:
            speech_ok = uac.last_speech_detected and secs >= 0.2
            if speech_ok:
                try:
                    transcript = stt_transcribe(mcp, wire, pt=uac.payload_type, plugins=plugins)
                except Exception as e:
                    print(f"[bot] STT failed: {e}")
                print(f"[bot] STT  : {transcript!r}")

        if not transcript:
            idle += 1
            reason = "no speech" if not (used_spec or (uac.last_speech_detected and secs >= 0.2)) else "empty transcript"
            print(f"[bot] {reason} ({idle}/{idle_turns_max})")
            if idle >= idle_turns_max:
                speak(uac, mcp, "Кажется, вас не слышно. До свидания.", plugins=plugins)
                break
            reprompt = "Простите, я вас не расслышал. Повторите, пожалуйста."
            played = speak(uac, mcp, reprompt, plugins=plugins)
            _pause_after_playback(uac, played)
            continue
        idle = 0

        history.append({"role": "user", "content": transcript})
        if _is_goodbye(transcript):
            speak(uac, mcp, "Спасибо за звонок. До свидания!", plugins=plugins)
            break

        # Local GPU LLM answers in ~0.1-0.4 s; with speculation the reply was
        # already drafted during the pause, so this is usually a no-op.
        if not (used_spec and reply is not None):
            try:
                reply = llm_reply(mcp, history, plugins=plugins)
            except Exception as e:  # noqa: BLE001
                print(f"[bot] LLM failed: {e}")
                speak(uac, mcp, "Извините, возникла техническая ошибка. Попробуйте позже.", plugins=plugins)
                continue
            # The assistant may choose to end the call: strip the [ОТБОЙ]
            # marker from the spoken text and remember the intent.
            reply, wants_end = _split_end_signal(reply)
            pre_wire = None
        print(f"[bot] LLM  : {reply!r}{'  [hang up]' if wants_end else ''}")
        if not reply:
            reply = "Спасибо за обращение. До свидания!" if wants_end else "Извините, я не смог сформулировать ответ."
        history.append({"role": "assistant", "content": reply})
        if pre_wire:
            played = _play_prerendered_wire(uac, pre_wire)
        else:
            played = speak_streaming(uac, mcp, reply, plugins=plugins, allow_barge_in=True)
        # Assistant decided to end the call: drop the dialog via SIP BYE (done by
        # the caller in handle_*_call's finally). But if the caller barged in over
        # the farewell, they want to keep talking — cancel the hang-up.
        if wants_end and not uac.last_barge_in:
            print("[bot] assistant ending call (BYE)")
            break
        # On barge-in the caller is already talking — go straight to
        # listening without the post-playback settle pause.
        if not uac.last_barge_in:
            _pause_after_playback(uac, played)

    print("[bot] conversation ended")


def handle_local_call(
    mcp: Mcp,
    engine: tuple[str, int],
    record_secs: float,
    codec: str,
    plugins: dict[str, str],
) -> None:
    uac = SipUAC(engine, codec=codec, rtp_host=engine[0])
    print(f"[bot] parking on sip:{RENDEZVOUS}@{engine[0]}:{engine[1]}")
    try:
        uac.invite(RENDEZVOUS)
    except Exception as e:
        print(f"[bot] INVITE failed: {e}")
        uac.close()
        return

    print(f"[bot] bridged, RTP → {uac.engine_rtp}")
    try:
        converse(
            uac,
            mcp,
            plugins=plugins,
            record_secs=record_secs,
            greet_first=False,
            debug_prefix="tmp/asr-bot-local",
        )
    finally:
        # Tear down the dialog via signaling before closing sockets.
        try:
            uac.bye(RENDEZVOUS)
        except Exception:
            pass
        uac.close()
    print("[bot] done")


def handle_trunk_call(
    mcp: Mcp,
    engine: tuple[str, int],
    rendezvous_key: str,
    call_id: str,
    record_secs: float,
    codec: str,
    plugins: dict[str, str],
) -> None:
    """Join inbound trunk leg parked on rendezvous_key."""
    uac = SipUAC(engine, codec=codec, rtp_host=engine[0])
    bridged = False
    print(f"[bot] inbound call_id={call_id!r}, joining sip:{rendezvous_key}@{engine[0]}:{engine[1]}")
    time.sleep(0.15)
    try:
        uac.invite(rendezvous_key)
        bridged = True
    except Exception as e:
        print(f"[bot] bridge INVITE failed: {e}")
        return
    finally:
        if not bridged:
            uac.close()

    # IMPORTANT: do NOT wait for inbound RTP before greeting. The engine's
    # RTP relay latches the bot's media address from the FIRST packet the
    # bot sends; until we transmit, it can't deliver inbound audio. So we
    # prime the path with a short silence burst first — this latches the
    # relay (opening the inbound direction) and warms the carrier's
    # symmetric-RTP pinhole so the greeting that follows is heard cleanly.
    print(f"[bot] bridged, RTP → {uac.engine_rtp}")
    try:
        # Longer prime (was 0.25) gives flaky symmetric-RTP trunks more time to
        # latch the relay before the greeting, improving start-of-call audio.
        prime_secs = float(os.environ.get("RTP_PRIME_SECS", "0.4"))
        uac.prime_rtp(prime_secs)
        print(f"[bot] RTP primed {prime_secs:.2f}s (relay latched)")
    except Exception as e:  # noqa: BLE001
        print(f"[bot] RTP prime failed ({e})")
    try:
        converse(
            uac,
            mcp,
            plugins=plugins,
            record_secs=record_secs,
            greet_first=True,
            debug_prefix="tmp/asr-bot",
        )
        print(f"[bot] call {call_id!r} done")
    finally:
        try:
            uac.bye(rendezvous_key)
        except Exception:
            pass
        uac.close()


def preflight_network(engine_port: int = 5060) -> None:
    """Warn when public IP / local SIP / router setup look wrong."""
    pub = os.environ.get("SIP_PUBLIC_ADDRESS", "").strip()
    try:
        import urllib.request

        seen = urllib.request.urlopen("https://ifconfig.me", timeout=5).read().decode().strip()
        print(f"[bot] public IP (ifconfig.me): {seen}")
        if pub and pub != seen:
            print(
                f"[bot] WARNING: SIP_PUBLIC_ADDRESS={pub} != ifconfig.me={seen} "
                "— Megafon RTP/signaling may fail; update multifon.env + restart"
            )
    except OSError as e:
        print(f"[bot] could not check public IP: {e}")

    import socket

    msg = (
        f"OPTIONS sip:ping@127.0.0.1 SIP/2.0\r\n"
        f"Via: SIP/2.0/UDP 127.0.0.1:5099;branch=z9hG4bK-preflight\r\n"
        f"Max-Forwards: 70\r\n"
        f"From: <sip:ping@127.0.0.1>;tag=1\r\n"
        f"To: <sip:ping@127.0.0.1>\r\n"
        f"Call-ID: preflight@127.0.0.1\r\n"
        f"CSeq: 1 OPTIONS\r\n"
        f"Content-Length: 0\r\n\r\n"
    )
    status = None
    last_err: OSError | None = None
    for attempt in range(8):
        if attempt:
            time.sleep(0.4)
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.settimeout(2.0)
        try:
            s.sendto(msg.encode(), ("127.0.0.1", engine_port))
            data, _ = s.recvfrom(65535)
            status = data.decode(errors="replace").split("\r\n", 1)[0]
            break
        except OSError as e:
            last_err = e
        finally:
            s.close()
    if status:
        print(f"[bot] local SIP probe 127.0.0.1:{engine_port} → {status}")
    elif last_err:
        print(
            f"[bot] WARNING: smiths-net не отвечает на :{engine_port} ({last_err}) "
            "— входящие INVITE не дойдут; проверьте: ss -ulnp | grep 5060"
        )

    if pub:
        print(
            f"[bot] inbound INVITEs must reach this PC on UDP/TCP {engine_port} "
            f"(router → LAN IP or DMZ). Contact in REGISTER: {pub}:{engine_port}"
        )


def sync_public_ip() -> str:
    """Resolve the SIP/SDP public IP (media.advertise_ip).

    IMPORTANT: an explicitly configured SIP_PUBLIC_ADDRESS wins. The
    inbound-reachable IP (the one Megafon delivers INVITEs to, i.e. the
    REGISTER Contact) is often NOT what ifconfig.me reports — a
    multi-homed / port-forwarded box has a different outbound SNAT
    address. Overriding advertise_ip with the outbound IP silently
    breaks RTP (carrier sends media to an address that never arrives).
    So we only fall back to ifconfig.me when nothing is configured, and
    otherwise just warn on a mismatch.
    """
    configured = os.environ.get("SIP_PUBLIC_ADDRESS", "").strip()
    seen = ""
    try:
        import urllib.request

        seen = urllib.request.urlopen("https://ifconfig.me", timeout=5).read().decode().strip()
    except OSError as e:
        print(f"[bot] could not detect outbound IP: {e}")

    if configured:
        if seen and seen != configured:
            print(
                f"[bot] note: outbound IP {seen} != SIP_PUBLIC_ADDRESS {configured}. "
                "Keeping configured value (it's the inbound-reachable one). "
                "If RTP is one-way, verify which public IP forwards UDP to this host."
            )
        os.environ["SMITHS__MEDIA__ADVERTISE_IP"] = configured
        return configured

    if seen:
        print(f"[bot] no SIP_PUBLIC_ADDRESS set — using detected outbound IP {seen}")
        os.environ["SIP_PUBLIC_ADDRESS"] = seen
        os.environ["SMITHS__MEDIA__ADVERTISE_IP"] = seen
    return seen


def _accept_trunk_call(call_id: str, rendezvous_key: str, rendezvous_map: dict[str, str]) -> bool:
    """Return True only for our DID / Megafon inbound — ignore scanner spam.

    Silent: callers log the final decision once (the accept loop polls
    this repeatedly while waiting for the rendezvous map to populate).
    """
    if not call_id or call_id.startswith("pyclient-"):
        return False
    # Megafon Multifon call-ids always start with "SD".
    if _MEGAFON_CALL_RE.match(call_id):
        return True
    return rendezvous_map.get(call_id) == rendezvous_key


def locate_binary() -> Path:
    for p in (Path("target/release/smiths-net"), Path("target/debug/smiths-net")):
        if p.exists():
            return p
    raise SystemExit("build first: cargo build --release -p smiths-cli")


def run_local_mode(
    mcp: Mcp,
    engine: tuple[str, int],
    args: argparse.Namespace,
    codec: str,
    plugins: dict[str, str],
) -> None:
    print(f"[bot] local mode — waiting on sip:{RENDEZVOUS}@{engine[0]}:{engine[1]}")
    calls = args.calls
    while True:
        handle_local_call(mcp, engine, args.record_secs, codec, plugins)
        if calls > 0:
            calls -= 1
            if calls == 0:
                break
        print("[bot] ready for next call…")


def run_trunk_mode(
    mcp: Mcp,
    engine: tuple[str, int],
    args: argparse.Namespace,
    codec: str,
    rendezvous_key: str,
    plugins: dict[str, str],
) -> None:
    pt = payload_type_for_codec(codec)
    call_q: queue.Queue[str] = queue.Queue()
    handling = threading.Event()

    def on_notif(method: str, params: dict) -> None:
        if method == "notifications/call/created":
            cid = params.get("call_id", "")
            if not cid or cid.startswith("pyclient-") or handling.is_set():
                return
            print(f"[mcp ] {method}  {params}")
            # Megafon inbound call-ids always start with SD — queue immediately.
            if _MEGAFON_CALL_RE.match(cid):
                call_q.put(cid)
                return
            # Scanner spam on the shared rendezvous: wait briefly for the
            # engine's parked-leg log to reveal the rendezvous key, then
            # accept only if it matches our DID. Log the decision once.
            for _ in range(20):
                if _accept_trunk_call(cid, rendezvous_key, mcp.rendezvous_by_call):
                    call_q.put(cid)
                    return
                if mcp.rendezvous_by_call.get(cid) is not None:
                    break
                time.sleep(0.05)
            rv = mcp.rendezvous_by_call.get(cid)
            print(f"[bot] skip call {cid!r} (rendezvous={rv}, DID={rendezvous_key})")
        elif method == "notifications/call/terminated":
            print(f"[mcp ] {method}  {params}")

    # Install the handler BEFORE the slow preloads and the REGISTER so no
    # inbound call is dropped during startup (the engine answers INVITEs
    # the moment it boots; a call arriving mid-preload must still queue).
    mcp.on_notification = on_notif

    preload_greeting(mcp, plugins, pt)
    preload_fillers(mcp, plugins, pt)
    warmup_llm(mcp, plugins)

    # REGISTER last — opens the DID to the carrier only once we're fully
    # ready to bridge and speak.
    reg_cfg = SipRegisterConfig.from_env()
    reg_cfg.validate()
    registrar = SipRegisterClient(reg_cfg, on_log=print)
    registrar.start()
    if not registrar.wait_registered(timeout=25.0):
        print("[bot] SIP REGISTER failed — check multifon.env and firewall")
        registrar.stop()
        return
    print(f"[bot] trunk mode — DID {rendezvous_key}, waiting for inbound calls…")

    calls = args.calls
    try:
        while True:
            call_id = call_q.get()
            handling.set()
            bridge_key = mcp.rendezvous_by_call.get(call_id, rendezvous_key)
            try:
                handle_trunk_call(
                    mcp, engine, bridge_key, call_id, args.record_secs, codec, plugins,
                )
            finally:
                handling.clear()
            if calls > 0:
                calls -= 1
                if calls == 0:
                    break
            print("[bot] ready for next inbound call…")
    finally:
        registrar.stop()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", type=Path, help="smiths-net binary")
    ap.add_argument("--config", type=Path, default=Path("examples/multifon.toml"))
    ap.add_argument("--env", type=Path, help="dotenv file (default: examples/multifon.env)")
    ap.add_argument("--engine", default="127.0.0.1:5060")
    ap.add_argument(
        "--bot-config",
        type=Path,
        default=Path(os.environ.get("BOT_CONFIG", "examples/bot.toml")),
        help="assistant prompt/persona config (default: examples/bot.toml)",
    )
    ap.add_argument(
        "--record-secs",
        type=float,
        default=float(os.environ.get("RECORD_SECS", "8.0")),
    )
    ap.add_argument(
        "--mode",
        choices=("local", "trunk", "auto"),
        default="auto",
        help="local=rendezvous voicebot; trunk=Megafon REGISTER+inbound; auto=trunk if SIP_REGISTRAR set",
    )
    ap.add_argument(
        "--calls",
        type=int,
        default=0,
        help="calls to handle (0 = loop forever, default: 0)",
    )
    args = ap.parse_args()

    env_path = args.env or Path("examples/multifon.env")
    load_env_file(env_path)
    load_env_file(Path("examples/offline.env"))
    bot = init_bot_config(args.bot_config)
    plugins = resolve_plugins()

    mode = args.mode
    if mode == "auto":
        mode = "trunk" if os.environ.get("SIP_REGISTRAR") else "local"

    if mode == "trunk":
        sync_public_ip()
    pub = os.environ.get("SIP_PUBLIC_ADDRESS", "").strip()
    if pub:
        os.environ["SMITHS__MEDIA__ADVERTISE_IP"] = pub

    host, _, port = args.engine.partition(":")
    engine = (host, int(port))
    codec = preferred_codec()
    pt = payload_type_for_codec(codec)

    rendezvous_key = os.environ.get("SIP_NUMBER", RENDEZVOUS)
    if mode == "local":
        rendezvous_key = RENDEZVOUS

    binary = args.bin or locate_binary()
    cmd = [str(binary), "--config", str(args.config), "--mcp", "stdio"]
    print(f"[bot] mode={mode} codec={codec.upper()} pt={pt}")
    print(f"[bot] persona gender={bot.gender} config={args.bot_config}")
    print(f"[bot] plugins asr={plugins['asr']} llm={plugins['llm']} tts={plugins['tts']}")
    print(f"[bot] spawning: {' '.join(cmd)}")

    mcp = Mcp(cmd)
    try:
        if not mcp.wait_ready(timeout=15.0):
            print("[bot] MCP handshake timed out")
            return 1
        print("[bot] MCP ready")
        if mode == "trunk":
            preflight_network(engine[1])
        if mode == "trunk":
            run_trunk_mode(mcp, engine, args, codec, rendezvous_key, plugins)
        else:
            run_local_mode(mcp, engine, args, codec, plugins)
    except KeyboardInterrupt:
        print("\n[bot] interrupted")
    finally:
        mcp.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
