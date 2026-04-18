#!/usr/bin/env python3
"""Standalone UA that places a call and streams audio into the engine.

Pairs with `listener.py` through a shared rendezvous key. Start the
listener first so the engine already has one leg parked before this
side joins.

Usage:
    python3 speaker.py --engine 127.0.0.1:5060 --room hello
    python3 speaker.py --room hello --wav /tmp/hello.wav
"""

from __future__ import annotations

import argparse
import sys
import time

from smiths_client import (
    SipUAC,
    generate_sine_pcm16,
    pcm16_to_pcmu,
    read_wav_mono_pcm16_8k,
)


def parse_engine(s: str) -> tuple[str, int]:
    host, _, port = s.partition(":")
    if not host or not port.isdigit():
        raise argparse.ArgumentTypeError(f"--engine expects HOST:PORT, got {s!r}")
    return host, int(port)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--engine", type=parse_engine, default=("127.0.0.1", 5060))
    ap.add_argument("--room", required=True, help="rendezvous key (same on both sides)")
    ap.add_argument(
        "--wav", help="16-bit mono 8 kHz WAV (default: generated 1 kHz sine)"
    )
    ap.add_argument(
        "--seconds", type=float, default=2.0, help="sine length if --wav is absent"
    )
    ap.add_argument("--settle-ms", type=int, default=250, help="pause before streaming")
    args = ap.parse_args()

    if args.wav:
        pcm = read_wav_mono_pcm16_8k(args.wav)
        print(f"loaded {len(pcm) // 2} samples from {args.wav}")
    else:
        pcm = generate_sine_pcm16(1_000.0, args.seconds)
        print(f"generated 1 kHz sine ({args.seconds:.2f} s)")

    uac = SipUAC(args.engine)
    print(f"speaker: sip:{args.room}@{args.engine[0]}:{args.engine[1]}")
    uac.invite(args.room)
    print(f"  200 OK, engine RTP: {uac.engine_rtp}")

    # Give the listener a chance to be fully ready.
    time.sleep(args.settle_ms / 1000.0)

    wire = pcm16_to_pcmu(pcm)
    print(f"streaming {len(wire)} μ-law bytes ({len(wire) / 8000:.2f} s wall-time)…")
    uac.stream_pcmu(wire)

    try:
        uac.bye(args.room)
    finally:
        uac.close()
    print("done")
    return 0


if __name__ == "__main__":
    sys.exit(main())
