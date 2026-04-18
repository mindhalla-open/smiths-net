#!/usr/bin/env python3
"""Voice-agent demo: MCP control plane + real audio pipeline.

A single Python process that plays the "voice agent" role behind
smiths-net:

  1. Spawns the engine with `--mcp stdio`, receives
     `notifications/call/created` / `notifications/call/terminated`
     frames in real time.
  2. In parallel, runs a minimal SIP UA that parks on rendezvous key
     `voicebot` so any caller dialling `sip:voicebot@engine` gets
     bridged to us.
  3. Collects the caller's RTP, runs the **STT → LLM → TTS** pipeline,
     and streams the reply back through the same bridge.

What's real here:

  * MCP stdio notifications.
  * SIP INVITE/ACK/BYE, SDP negotiation, rendezvous bridge (engine).
  * RTP in and out, μ-law codec (Python).
  * TTS — macOS `say` shells out to a real WAV.

What's mocked (and where real plugins will slot in — see
`docs/architecture/02-plugin-system.md`):

  * STT — we don't yet have a plugin system. Stub returns a fixed
    caller transcript after the audio is collected.
    Real plan: `ai-asr-whisper` sidecar (post-MVP P22) reads RTP
    directly off the leg and returns text over MCP.
  * LLM — stub returns a hard-coded reply. Real plan: `ai-llm-*`
    sidecar plugin invoked through `ai_invoke("ai.llm.completion", ...)`.
  * Engine-side TTS — today the Python client synthesises and streams
    RTP itself. In the target architecture the engine's `ai-tts-piper`
    plugin takes `speak_text(call_id, text)` over MCP and does the
    RTP injection in-process.

Run from the repo root:

    cargo build --release
    python3 examples/python-client/voice_agent.py

Then, in a second terminal, call into the rendezvous:

    python3 examples/python-client/voice_caller.py --wav tmp/smiths-hello.wav

The agent answers ≈ "Алло, Алиса слушает вас" (via macOS `say`).
The caller saves the reply to `tmp/voice-agent-reply.wav`.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

from smiths_client import (
    SipUAC,
    generate_sine_pcm16,
    pcm16_to_pcmu,
    read_wav_mono_pcm16_8k,
    write_wav_mono_pcm16_8k,
)
from smiths_client import RtpPacket  # noqa: F401 — documented re-export

RENDEZVOUS = "voicebot"
AGENT_REPLY_TEXT = "Алло, Алиса слушает вас"


# ---------------------------------------------------------------------------
# MCP subprocess client — spawns the engine and reads the JSON-RPC wire.
# ---------------------------------------------------------------------------


class McpStdioClient(threading.Thread):
    """Background thread: drives MCP handshake and drains notifications."""

    def __init__(
        self,
        binary: Path,
        config: Path,
        on_notification,
    ) -> None:
        super().__init__(daemon=True)
        self.binary = binary
        self.config = config
        self.on_notification = on_notification
        self.proc: subprocess.Popen | None = None
        self._next_id = 0
        self._ready = threading.Event()
        self._stop = threading.Event()

    def _send(self, method: str, params=None, *, notify: bool = False) -> None:
        assert self.proc is not None and self.proc.stdin is not None
        frame: dict = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            frame["params"] = params
        if not notify:
            self._next_id += 1
            frame["id"] = self._next_id
        self.proc.stdin.write((json.dumps(frame) + "\n").encode())
        self.proc.stdin.flush()

    def run(self) -> None:
        cmd = [str(self.binary), "--config", str(self.config), "--mcp", "stdio"]
        self.proc = subprocess.Popen(
            cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
        )
        # Handshake.
        self._send(
            "initialize",
            {
                "protocolVersion": "2024-11-05",
                "clientInfo": {"name": "voice_agent.py", "version": "0.1"},
                "capabilities": {},
            },
        )
        # Drain response + notifications.
        assert self.proc.stdout is not None
        while not self._stop.is_set():
            line = self.proc.stdout.readline()
            if not line:
                break
            try:
                frame = json.loads(line)
            except json.JSONDecodeError:
                continue
            if "id" in frame and frame.get("id") == 1:
                # initialize response
                self._send("notifications/initialized", {}, notify=True)
                self._ready.set()
                continue
            if "method" in frame and frame["method"].startswith("notifications/"):
                self.on_notification(frame["method"], frame.get("params") or {})
        # stdout closed → engine exited.

    def wait_ready(self, timeout: float = 5.0) -> bool:
        return self._ready.wait(timeout)

    def shutdown(self) -> None:
        self._stop.set()
        if self.proc and self.proc.stdin:
            try:
                self.proc.stdin.close()
            except BrokenPipeError:
                pass
        if self.proc:
            try:
                self.proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.proc.kill()


# ---------------------------------------------------------------------------
# Stub STT / LLM + real TTS (macOS `say` if present, fallback sine otherwise).
# ---------------------------------------------------------------------------


def stub_stt(received_wire: bytes) -> str:
    """Pretend to transcribe caller audio. Real impl: Whisper sidecar."""
    duration_s = len(received_wire) / 8000.0
    if duration_s < 0.3:
        return "(silence)"
    return f"[mock STT: caller said ~{duration_s:.1f} s of audio]"


def stub_llm(transcript: str) -> str:
    """Pretend to run an LLM. Real impl: `ai.llm.completion` capability."""
    # The demo always responds with the same greeting. A real agent
    # would build a prompt around `transcript` and call e.g. Ollama.
    _ = transcript
    return AGENT_REPLY_TEXT


def tts_to_pcm16(text: str, out_path: Path) -> bytes:
    """Synthesize `text` as mono 16-bit PCM @ 8 kHz.

    Real engine-side TTS lands as a plugin (P22). Here we shell out to
    macOS `say`, or fall back to a noticeable beep so the demo still
    produces audio on Linux.
    """
    if shutil.which("say"):
        voice = os.environ.get("SMITHS_TTS_VOICE", "Milena")  # Russian macOS voice
        subprocess.run(
            [
                "say",
                "-v",
                voice,
                "--file-format=WAVE",
                "--data-format=LEI16@8000",
                "-o",
                str(out_path),
                text,
            ],
            check=True,
        )
        return read_wav_mono_pcm16_8k(str(out_path))
    # Fallback: 2 s of 600 Hz sine so there's at least *something* audible.
    pcm = generate_sine_pcm16(600.0, 2.0, amplitude=10000)
    write_wav_mono_pcm16_8k(str(out_path), pcm)
    return pcm


# ---------------------------------------------------------------------------
# Agent wiring.
# ---------------------------------------------------------------------------


def locate_binary() -> Path:
    for candidate in (
        Path("target/release/smiths-net"),
        Path("target/debug/smiths-net"),
    ):
        if candidate.exists():
            return candidate
    raise SystemExit("smiths-net binary not found — run `cargo build --release` first")


def run_agent(engine_sip: tuple[str, int], mcp: McpStdioClient) -> None:
    """Park a UA on the rendezvous key and run STT → LLM → TTS once."""
    uac = SipUAC(engine_sip)
    print(
        f"[agent] SIP UA at {uac.sip.getsockname()}, "
        f"parking on sip:{RENDEZVOUS}@{engine_sip[0]}:{engine_sip[1]}"
    )
    try:
        uac.invite(RENDEZVOUS)
    except Exception as e:
        print(f"[agent] INVITE failed: {e}")
        uac.close()
        return
    print(f"[agent] 200 OK, engine RTP at {uac.engine_rtp}")

    # Wait for the caller to arrive — we know that happens when the
    # engine emits notifications/call/created for a *second* leg on
    # the same rendezvous.
    print("[agent] waiting for caller to join the bridge…")
    # Practical heuristic: start recording RTP; the caller's audio
    # will arrive through the bridge once the engine pairs the legs.
    # Give them up to 15 s to dial in.
    received = uac.record_pcmu(max_seconds=15.0)
    if not received:
        print("[agent] no audio received; exiting")
        uac.close()
        return

    # -------- STT (mock) --------
    transcript = stub_stt(received)
    print(f"[agent] STT: {transcript}")

    # -------- LLM (mock) --------
    reply_text = stub_llm(transcript)
    print(f"[agent] LLM → {reply_text!r}")

    # -------- TTS (real, via `say`) --------
    with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as fh:
        wav_path = Path(fh.name)
    try:
        reply_pcm = tts_to_pcm16(reply_text, wav_path)
        print(
            f"[agent] TTS: {len(reply_pcm) // 2} samples "
            f"({len(reply_pcm) / 16000:.2f} s) -> streaming RTP"
        )
        # Small settle so the caller's recorder is armed.
        time.sleep(0.2)
        uac.stream_pcmu(pcm16_to_pcmu(reply_pcm))
    finally:
        wav_path.unlink(missing_ok=True)

    # Let the BYE initiate from the caller's side (simpler flow).
    # The MCP notification will tell us when the dialog ends.
    uac.close()
    print("[agent] media reply sent; UA closed. Waiting for caller BYE via MCP.")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--bin", type=Path, help="smiths-net binary (auto if omitted)")
    ap.add_argument("--config", type=Path, default=Path("examples/config.toml"))
    ap.add_argument(
        "--engine",
        default="127.0.0.1:5060",
        help="engine SIP address (must match config's bind)",
    )
    args = ap.parse_args()

    binary = args.bin or locate_binary()
    if not args.config.exists():
        raise SystemExit(f"config not found: {args.config}")

    host, _, port_s = args.engine.partition(":")
    if not port_s.isdigit():
        raise SystemExit("--engine expects HOST:PORT")
    engine_sip = (host, int(port_s))

    # Collect call events so we can log the full lifecycle.
    call_log: list[tuple[str, dict]] = []
    lock = threading.Lock()

    def on_notif(method: str, params: dict) -> None:
        with lock:
            call_log.append((method, params))
        print(f"[mcp ] {method}  {params}")

    mcp = McpStdioClient(binary, args.config, on_notif)
    mcp.start()
    if not mcp.wait_ready(timeout=5.0):
        print("mcp handshake timed out")
        mcp.shutdown()
        return 1
    print("[mcp ] initialize ok — ready to receive notifications")

    # One-shot agent run: park on the rendezvous, handle one call.
    try:
        run_agent(engine_sip, mcp)
    except KeyboardInterrupt:
        print("\n[agent] interrupted")
    finally:
        # Keep MCP open briefly so we catch the caller's BYE notification.
        time.sleep(1.0)
        mcp.shutdown()
        print("\n=== call event log (via MCP notifications) ===")
        for method, params in call_log:
            print(f"  {method}  {params}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
