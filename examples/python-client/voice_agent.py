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
import subprocess
import sys
import threading
import time
from pathlib import Path

from smiths_client import SipUAC, pcm16_to_pcmu

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
        # Correlation for synchronous `call_tool`: id → Event + slot.
        self._pending: dict[int, tuple[threading.Event, list]] = {}
        self._pending_lock = threading.Lock()
        self._send_lock = threading.Lock()

    def _send(self, method: str, params=None, *, notify: bool = False) -> int | None:
        """Send a JSON-RPC frame. Returns the assigned id (or None for notifications)."""
        assert self.proc is not None and self.proc.stdin is not None
        with self._send_lock:
            frame: dict = {"jsonrpc": "2.0", "method": method}
            if params is not None:
                frame["params"] = params
            id_: int | None = None
            if not notify:
                self._next_id += 1
                id_ = self._next_id
                frame["id"] = id_
            self.proc.stdin.write((json.dumps(frame) + "\n").encode())
            self.proc.stdin.flush()
            return id_

    def call_tool(self, name: str, arguments: dict | None = None, *, timeout: float = 30.0) -> dict:
        """Synchronously invoke an MCP tool and return its result."""
        event = threading.Event()
        slot: list = []
        params = {"name": name, "arguments": arguments or {}}
        with self._pending_lock:
            id_ = self._send("tools/call", params)
            if id_ is None:
                raise RuntimeError("call_tool got no id")
            self._pending[id_] = (event, slot)
        if not event.wait(timeout):
            with self._pending_lock:
                self._pending.pop(id_, None)
            raise TimeoutError(f"MCP call_tool({name}) timed out after {timeout} s")
        frame = slot[0]
        if "error" in frame:
            err = frame["error"]
            raise RuntimeError(
                f"MCP error {err.get('code')}: {err.get('message')}"
            )
        return frame.get("result", {})

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
            # Route responses with an id to waiting callers; everything
            # else either is our `initialize` response or a notification.
            if "id" in frame:
                rid = frame.get("id")
                if rid == 1:
                    self._send("notifications/initialized", {}, notify=True)
                    self._ready.set()
                    continue
                with self._pending_lock:
                    slot = self._pending.pop(rid, None)
                if slot is not None:
                    event, bucket = slot
                    bucket.append(frame)
                    event.set()
                continue
            if "method" in frame and frame["method"].startswith("notifications/"):
                self.on_notification(frame["method"], frame.get("params") or {})
        # stdout closed → engine exited.
        # Wake up anybody still waiting so they don't hang forever.
        with self._pending_lock:
            for _id, (event, _) in self._pending.items():
                event.set()
            self._pending.clear()

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
# MCP-driven AI: every STT / LLM / TTS hop goes through the engine's
# tool registry → plugin sidecar. The agent owns zero inference code.
# ---------------------------------------------------------------------------


def mcp_transcribe(
    mcp: "McpStdioClient",
    pcmu_wire: bytes,
    language: str = "ru",
) -> str:
    """Decode μ-law → PCM16 locally (the wire format is PCM16 in this
    protocol), base64, hand to the engine's `transcribe` tool. Real
    replacement: `ai-asr-whisper` plugin (post-MVP P22)."""
    import base64

    from smiths_client import pcmu_to_pcm16

    pcm = pcmu_to_pcm16(pcmu_wire)
    b64 = base64.b64encode(pcm).decode("ascii")
    result = mcp.call_tool(
        "transcribe",
        {
            "plugin": "ai-asr-mock",
            "audio_base64": b64,
            "sample_rate": 8000,
            "language": language,
        },
        timeout=15.0,
    )
    if result.get("isError"):
        raise RuntimeError(f"transcribe failed: {result}")
    payload = result.get("structuredContent") or {}
    return payload.get("text", "")


def mcp_llm_chat(
    mcp: "McpStdioClient",
    user_text: str,
    system_prompt: str | None = None,
) -> str:
    """One-shot LLM chat via the engine. Returns the assistant's reply
    text. Real replacement: any `ai-llm-*` plugin."""
    messages: list[dict] = []
    if system_prompt:
        messages.append({"role": "system", "content": system_prompt})
    messages.append({"role": "user", "content": user_text})
    result = mcp.call_tool(
        "llm_chat",
        {
            "plugin": "ai-llm-mock",
            "messages": messages,
            "controls": {"temperature": 0.3, "max_tokens": 128},
        },
        timeout=15.0,
    )
    if result.get("isError"):
        raise RuntimeError(f"llm_chat failed: {result}")
    payload = result.get("structuredContent") or {}
    msg = (payload.get("message") or {}).get("content")
    if not msg:
        raise RuntimeError(f"llm_chat returned no message: {payload}")
    return msg


def mcp_synthesize(mcp: "McpStdioClient", text: str, voice: str = "irina") -> bytes:
    """Render `text` via the engine's `synthesize` MCP tool.

    Real engine-side TTS now flows through a loaded `ai.tts` plugin
    (see `plugins/examples/ai-tts-mock/`). Agent owns no TTS code; it
    just asks the engine which hands off to the plugin sidecar.
    Returns raw PCM16 LE @ 8 kHz bytes.
    """
    result = mcp.call_tool(
        "synthesize",
        {
            "plugin": "ai-tts-mock",
            "text": text,
            "voice": voice,
            "output": {"codec": "pcm_s16le", "sample_rate": 8000},
        },
        timeout=15.0,
    )
    payload = result.get("structuredContent") or {}
    if result.get("isError") or not payload:
        raise RuntimeError(f"synthesize failed: {result}")
    b64 = payload.get("audio_base64")
    if not b64:
        raise RuntimeError(f"synthesize returned no audio_base64: {payload}")
    import base64

    return base64.b64decode(b64)


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

    # -------- STT (engine plugin via MCP) --------
    try:
        transcript = mcp_transcribe(mcp, received, language="ru")
    except Exception as e:
        print(f"[agent] transcribe failed: {e}")
        uac.close()
        return
    print(f"[agent] STT: {transcript!r}")

    # -------- LLM (engine plugin via MCP) --------
    try:
        reply_text = mcp_llm_chat(
            mcp,
            user_text=transcript,
            system_prompt="You are Alice, a polite Russian phone operator. Answer briefly.",
        )
    except Exception as e:
        print(f"[agent] llm_chat failed: {e}")
        reply_text = AGENT_REPLY_TEXT
    print(f"[agent] LLM → {reply_text!r}")

    # -------- TTS (real, via the engine's `ai-tts-mock` plugin) --------
    try:
        reply_pcm = mcp_synthesize(mcp, reply_text, voice="irina")
    except Exception as e:
        print(f"[agent] TTS via MCP failed: {e}")
        uac.close()
        return
    print(
        f"[agent] TTS: {len(reply_pcm) // 2} samples "
        f"({len(reply_pcm) / 16000:.2f} s) -> streaming RTP"
    )
    # Small settle so the caller's recorder is armed.
    time.sleep(0.2)
    uac.stream_pcmu(pcm16_to_pcmu(reply_pcm))

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
