#!/usr/bin/env python3
"""Simulated inbound caller for the voice-agent demo.

Dials `sip:voicebot@engine` with a PCMU-only SDP offer, plays a
greeting WAV (or a generated tone) at the agent, records the agent's
reply to a WAV, and hangs up. Pairs with `voice_agent.py`.

Usage (from repo root, with the engine + agent already running):

    python3 examples/python-client/voice_caller.py \
        --wav tmp/smiths-hello.wav \
        --out tmp/voice-agent-reply.wav
"""

from __future__ import annotations

import argparse
import sys
import threading
import time

from smiths_client import (
    SipUAC,
    generate_sine_pcm16,
    pcm16_to_pcmu,
    pcmu_to_pcm16,
    read_wav_mono_pcm16_8k,
    upsample_pcm16,
    write_wav_mono_pcm16,
    write_wav_mono_pcm16_8k,
)

RENDEZVOUS = "voicebot"
DEFAULT_OUT = "tmp/voice-agent-reply.wav"


def parse_engine(s: str) -> tuple[str, int]:
    host, _, port = s.partition(":")
    if not host or not port.isdigit():
        raise argparse.ArgumentTypeError(f"--engine expects HOST:PORT, got {s!r}")
    return host, int(port)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--engine", type=parse_engine, default=("127.0.0.1", 5060))
    ap.add_argument(
        "--wav", help="16-bit mono 8 kHz WAV to play (default: 1 s of 440 Hz tone)"
    )
    ap.add_argument(
        "--out", default=DEFAULT_OUT, help=f"Reply WAV path (default: {DEFAULT_OUT})"
    )
    ap.add_argument(
        "--listen-secs",
        type=float,
        default=45.0,
        help="Max time to wait for the agent's reply (default: 45).",
    )
    ap.add_argument(
        "--quiet-secs",
        type=float,
        default=0.6,
        help="Stop recording after this much silence once audio arrives (default: 0.6).",
    )
    args = ap.parse_args()

    if args.wav:
        greeting = read_wav_mono_pcm16_8k(args.wav)
        print(f"[caller] loaded {len(greeting) // 2} samples from {args.wav}")
    else:
        greeting = generate_sine_pcm16(440.0, 1.0, amplitude=10000)
        print(f"[caller] generated 1 s of 440 Hz tone")

    uac = SipUAC(args.engine)
    print(f"[caller] INVITE sip:{RENDEZVOUS}@{args.engine[0]}:{args.engine[1]}")
    uac.invite(RENDEZVOUS)
    print(f"[caller] 200 OK, engine RTP: {uac.engine_rtp}")

    # Small delay so the agent's parked leg is definitely bridged.
    time.sleep(0.3)

    # Record in a background thread while we stream the greeting. The bot
    # needs several seconds for STT → LLM → TTS; if we only start
    # recording after we finish speaking, we'd miss the reply window.
    received: list[bytes] = []

    def _record() -> None:
        received.append(
            uac.record_pcmu(args.listen_secs, quiet_secs=args.quiet_secs)
        )

    rec = threading.Thread(target=_record, daemon=True)
    rec.start()
    time.sleep(0.05)

    print(f"[caller] streaming {len(greeting) // 2} samples as PCMU RTP…")
    uac.stream_pcmu(pcm16_to_pcmu(greeting))

    print(f"[caller] waiting for agent reply (up to {args.listen_secs:.1f} s)…")
    rec.join(timeout=args.listen_secs + 2.0)
    reply_wire = received[0] if received else b""
    print(
        f"[caller] captured {len(reply_wire)} μ-law bytes "
        f"({len(reply_wire) / 8000:.2f} s)"
    )

    if reply_wire:
        pcm = pcmu_to_pcm16(reply_wire)
        if len(pcm) < 3200:
            print(
                "[caller] warning: reply very short — "
                "is the bot running? try `--listen-secs 45` for OpenAI latency"
            )
        # 48 kHz — Ubuntu desktop players often won't play 8 kHz WAV.
        playback_hz = 48_000
        pcm_playback = upsample_pcm16(pcm, 8000, playback_hz)
        write_wav_mono_pcm16(args.out, pcm_playback, sample_rate=playback_hz)
        print(
            f"[caller] reply written to {args.out} "
            f"({len(pcm_playback) / (playback_hz * 2):.2f} s @ {playback_hz} Hz)"
        )
        print(f"[caller] play: aplay {args.out}")
    else:
        print("[caller] no audio received from the agent")

    try:
        uac.bye(RENDEZVOUS)
    except Exception as e:
        print(f"[caller] BYE failed (non-fatal): {e}")
    finally:
        uac.close()

    return 0


if __name__ == "__main__":
    sys.exit(main())
