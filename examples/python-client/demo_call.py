#!/usr/bin/env python3
"""One-process demo: two Python UAs call through the engine, audio round-trip.

Prereq: `smiths-net` engine is running on `127.0.0.1:5060` (or pass
`--engine HOST:PORT`).

Usage:
    python3 demo_call.py
    python3 demo_call.py --wav ~/Music/sample.wav
    python3 demo_call.py --engine 192.168.1.10:5060 --room lab
"""

from __future__ import annotations

import argparse
import sys

from smiths_client import (
    generate_sine_pcm16,
    read_wav_mono_pcm16_8k,
    run_two_party,
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
    ap.add_argument("--room", default="demo-call")
    ap.add_argument(
        "--wav", help="16-bit mono 8 kHz WAV to stream (default: 1 kHz sine)"
    )
    ap.add_argument(
        "--seconds", type=float, default=1.0, help="Sine length when --wav is absent"
    )
    ap.add_argument(
        "--out", default=DEFAULT_OUT, help=f"Output WAV path (default: {DEFAULT_OUT})"
    )
    args = ap.parse_args()

    if args.wav:
        source = read_wav_mono_pcm16_8k(args.wav)
        print(f"streaming {len(source) // 2} samples from {args.wav}")
    else:
        source = generate_sine_pcm16(1_000.0, args.seconds)
        print(f"streaming generated 1 kHz sine ({args.seconds:.2f} s)")

    out = run_two_party(args.engine, args.room, source, args.out)
    print(f"done — received audio written to {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
