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
import time

from smiths_client import (
    SipUAC,
    generate_sine_pcm16,
    pcm16_to_pcmu,
    pcmu_to_pcm16,
    read_wav_mono_pcm16_8k,
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
        default=8.0,
        help="Max time to wait for the agent's reply.",
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

    print(f"[caller] streaming {len(greeting) // 2} samples as PCMU RTP…")
    uac.stream_pcmu(pcm16_to_pcmu(greeting))

    print(f"[caller] listening for agent reply for up to {args.listen_secs:.1f} s…")
    reply_wire = uac.record_pcmu(max_seconds=args.listen_secs)
    print(
        f"[caller] captured {len(reply_wire)} μ-law bytes "
        f"({len(reply_wire) / 8000:.2f} s)"
    )

    try:
        uac.bye(RENDEZVOUS)
    finally:
        uac.close()

    if reply_wire:
        pcm = pcmu_to_pcm16(reply_wire)
        write_wav_mono_pcm16_8k(args.out, pcm)
        print(f"[caller] reply written to {args.out}")
    else:
        print("[caller] no audio received from the agent")

    return 0


if __name__ == "__main__":
    sys.exit(main())
