#!/usr/bin/env python3
"""Standalone UA that places a call and records received RTP to a WAV.

Pairs with `speaker.py` through a shared rendezvous key. Start this
side first so the engine parks one leg and then bridges when the
speaker arrives.

Usage:
    python3 listener.py --room hello --seconds 5
    python3 listener.py --engine 127.0.0.1:5060 --room hello --out /tmp/rx.wav
"""

from __future__ import annotations

import argparse
import sys

from smiths_client import (
    SipUAC,
    pcmu_to_pcm16,
    write_wav_mono_pcm16_8k,
)

DEFAULT_OUT = "tmp/smiths-py-received.wav"


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
        "--out", default=DEFAULT_OUT, help=f"output WAV (default: {DEFAULT_OUT})"
    )
    ap.add_argument(
        "--seconds", type=float, default=5.0, help="hard cap on recording time"
    )
    args = ap.parse_args()

    uac = SipUAC(args.engine)
    print(f"listener: sip:{args.room}@{args.engine[0]}:{args.engine[1]}")
    uac.invite(args.room)
    print(f"  200 OK, engine RTP: {uac.engine_rtp}")
    print(f"  recording up to {args.seconds:.1f} s…")

    wire = uac.record_pcmu(max_seconds=args.seconds)
    print(f"  captured {len(wire)} μ-law bytes ({len(wire) / 8000:.2f} s)")

    try:
        uac.bye(args.room)
    finally:
        uac.close()

    pcm = pcmu_to_pcm16(wire)
    write_wav_mono_pcm16_8k(args.out, pcm)
    print(f"wrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
