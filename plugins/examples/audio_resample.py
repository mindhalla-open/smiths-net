"""Shared PCM16 resampler for sidecar TTS plugins.

Anti-aliased sample-rate conversion so a wideband neural voice (e.g. the
24 kHz Silero output) lands cleanly on the 8 kHz G.711 telephony wire —
the difference between a natural and a "robotic", aliasing-buzz voice.

Pure stdlib (no numpy), so the plugins stay dependency-light.
"""

from __future__ import annotations

import math
import os
import struct


def _design_lowpass(num_taps: int, fc_norm: float) -> list[float]:
    """Hamming-windowed sinc low-pass. fc_norm = cutoff / input_rate."""
    m = num_taps - 1
    taps: list[float] = []
    for n in range(num_taps):
        x = n - m / 2.0
        sinc = 2 * fc_norm if x == 0 else math.sin(2 * math.pi * fc_norm * x) / (math.pi * x)
        window = 0.54 - 0.46 * math.cos(2 * math.pi * n / m)
        taps.append(sinc * window)
    total = sum(taps)
    return [t / total for t in taps]


def resample_pcm16(pcm: bytes, from_hz: int, to_hz: int) -> bytes:
    """Resample PCM16 mono. Downsampling applies an anti-aliasing low-pass
    (telephony band ~3.4 kHz) before decimation so the neural voice lands on
    the narrowband wire without aliasing buzz. Integer upsampling repeats
    samples. Number of FIR taps is tunable via RESAMPLE_TAPS (default 47).
    """
    if from_hz == to_hz or not pcm:
        return pcm

    n = len(pcm) // 2
    x = struct.unpack(f"<{n}h", pcm)

    if from_hz < to_hz:
        ratio = to_hz // from_hz
        if ratio * from_hz == to_hz:
            out = bytearray()
            for s in x:
                out.extend(struct.pack(f"<{ratio}h", *([s] * ratio)))
            return bytes(out)

    cutoff = min(3400.0, 0.45 * to_hz)
    fc_norm = cutoff / from_hz
    num_taps = int(os.environ.get("RESAMPLE_TAPS", "47"))
    taps = _design_lowpass(num_taps, fc_norm)
    half = num_taps // 2
    step = from_hz / to_hz
    out_n = int(n * to_hz / from_hz)

    out = [0] * out_n
    for m_idx in range(out_n):
        center = int(m_idx * step)
        acc = 0.0
        for k in range(num_taps):
            idx = center + k - half
            if 0 <= idx < n:
                acc += taps[k] * x[idx]
        v = int(acc)
        out[m_idx] = -32768 if v < -32768 else (32767 if v > 32767 else v)
    return struct.pack(f"<{out_n}h", *out)
