"""Tiny SIP UAC + RTP toolkit for demos against a smiths-net engine.

Pure stdlib. Not a general-purpose SIP library — it implements just
enough to:

* place an `INVITE sip:<room>@engine` with a PCMU-only SDP offer,
* handle the engine's `100 Trying` / `200 OK` / `ACK`,
* send G.711 μ-law RTP packets generated from a PCM16 mono 8 kHz source,
* receive RTP packets, decode μ-law, collect PCM16 samples,
* tear the dialog down with `BYE`.

Two instances talking to the same `room` will get their media bridged
by the engine (see `crates/smiths-sip/src/uas.rs` for the rendezvous
logic).
"""

from __future__ import annotations

import itertools
import math
import random
import re
import socket
import struct
import threading
import time
import wave
from dataclasses import dataclass
from pathlib import Path
from typing import Optional


# ---------------------------------------------------------------------------
# G.711 μ-law codec (bit-exact; avoids the deprecated `audioop` stdlib module).
# ---------------------------------------------------------------------------

_MU_BIAS = 0x84
_MU_CLIP = 32635


def linear_to_ulaw(sample: int) -> int:
    sign = 0x80 if sample < 0 else 0x00
    if sample < 0:
        sample = -sample
    if sample > _MU_CLIP:
        sample = _MU_CLIP
    sample += _MU_BIAS
    exponent = 7
    mask = 0x4000
    while (sample & mask) == 0 and exponent > 0:
        exponent -= 1
        mask >>= 1
    mantissa = (sample >> (exponent + 3)) & 0x0F
    return (~(sign | (exponent << 4) | mantissa)) & 0xFF


def ulaw_to_linear(u: int) -> int:
    u = (~u) & 0xFF
    sign = u & 0x80
    exponent = (u >> 4) & 0x07
    mantissa = u & 0x0F
    magnitude = ((mantissa << 3) + _MU_BIAS) << exponent
    sample = magnitude - _MU_BIAS
    return -sample if sign else sample


def pcm16_to_pcmu(samples: bytes) -> bytes:
    """Encode a PCM16 little-endian byte buffer to μ-law bytes."""
    n = len(samples) // 2
    shorts = struct.unpack_from(f"<{n}h", samples)
    return bytes(linear_to_ulaw(s) for s in shorts)


def pcmu_to_pcm16(data: bytes) -> bytes:
    return b"".join(struct.pack("<h", ulaw_to_linear(b)) for b in data)


# ---------------------------------------------------------------------------
# WAV helpers.
# ---------------------------------------------------------------------------


def read_wav_mono_pcm16_8k(path: str) -> bytes:
    """Load a WAV file as raw PCM16 LE bytes. Enforces mono / 8 kHz / 16-bit."""
    with wave.open(path, "rb") as f:
        if f.getnchannels() != 1:
            raise ValueError(f"{path}: must be mono, got {f.getnchannels()} channels")
        if f.getsampwidth() != 2:
            raise ValueError(
                f"{path}: must be 16-bit PCM, got {f.getsampwidth() * 8}-bit"
            )
        if f.getframerate() != 8000:
            raise ValueError(f"{path}: must be 8 kHz, got {f.getframerate()} Hz")
        return f.readframes(f.getnframes())


def write_wav_mono_pcm16_8k(path: str, samples: bytes) -> None:
    """Write raw PCM16 LE bytes as a mono 8 kHz WAV."""
    with wave.open(path, "wb") as f:
        f.setnchannels(1)
        f.setsampwidth(2)
        f.setframerate(8000)
        f.writeframes(samples)


def generate_sine_pcm16(
    freq_hz: float, duration_s: float, amplitude: int = 10000
) -> bytes:
    """Return raw PCM16 LE bytes of a mono 8 kHz sine wave."""
    sample_rate = 8000
    total = round(sample_rate * duration_s)
    out = bytearray(total * 2)
    for n in range(total):
        v = int(amplitude * math.sin(2.0 * math.pi * freq_hz * n / sample_rate))
        struct.pack_into("<h", out, n * 2, max(-32768, min(32767, v)))
    return bytes(out)


# ---------------------------------------------------------------------------
# RTP (RFC 3550) — minimal subset: v=2, no extensions, no CSRCs.
# ---------------------------------------------------------------------------


@dataclass
class RtpPacket:
    marker: bool
    payload_type: int
    sequence: int
    timestamp: int
    ssrc: int
    payload: bytes

    def encode(self) -> bytes:
        b0 = 0b1000_0000  # V=2, P=0, X=0, CC=0
        b1 = ((1 if self.marker else 0) << 7) | (self.payload_type & 0x7F)
        return (
            struct.pack("!BBHII", b0, b1, self.sequence, self.timestamp, self.ssrc)
            + self.payload
        )

    @classmethod
    def decode(cls, buf: bytes) -> Optional["RtpPacket"]:
        if len(buf) < 12:
            return None
        b0, b1, seq, ts, ssrc = struct.unpack("!BBHII", buf[:12])
        if (b0 >> 6) != 2 or (b0 & 0b0011_1111) != 0:
            return None
        return cls(
            marker=(b1 & 0x80) != 0,
            payload_type=b1 & 0x7F,
            sequence=seq,
            timestamp=ts,
            ssrc=ssrc,
            payload=buf[12:],
        )


# ---------------------------------------------------------------------------
# SIP UAC — the bare minimum to INVITE / ACK / BYE against smiths-net.
# ---------------------------------------------------------------------------


_seq = itertools.count(1)


def _unique() -> int:
    return next(_seq) * 10_000 + random.randint(0, 9_999)


_TO_TAG_RE = re.compile(r"(?im)^To:.*;tag=([^\s;]+)")
_M_AUDIO_RE = re.compile(r"(?m)^m=audio\s+(\d+)\s+")
_C_IP_RE = re.compile(r"(?m)^c=IN\s+IP4\s+([0-9.]+)")


class SipUAC:
    """A single-dialog SIP user agent suitable for demo / test harness use."""

    def __init__(self, engine: tuple[str, int], local_ip: str = "127.0.0.1") -> None:
        self.engine = engine
        self.local_ip = local_ip
        self.sip = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sip.bind((local_ip, 0))
        self.sip.settimeout(3.0)
        self.rtp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.rtp.bind((local_ip, 0))
        self.rtp.settimeout(2.0)

        self.call_id = f"pyclient-{_unique()}@{local_ip}"
        self.from_tag = f"py-{_unique()}"
        self.to_tag: Optional[str] = None
        self.engine_rtp: Optional[tuple[str, int]] = None
        self._cseq = 0

    # ----- public ops -----

    def invite(self, room: str) -> None:
        """INVITE sip:<room>@engine, read responses, send ACK on 200."""
        self._cseq += 1
        cseq = self._cseq
        offer = self._sdp_offer()
        req = self._frame(
            "INVITE",
            room,
            cseq,
            {"Content-Type": "application/sdp", "Content-Length": str(len(offer))},
            body=offer,
        )
        self._send(req)

        while True:
            msg = self._recv_sip()
            status = _status(msg)
            if status is None:
                raise RuntimeError(f"bad response:\n{msg}")
            if 100 <= status < 200:
                continue  # provisional
            if status != 200:
                raise RuntimeError(f"INVITE rejected: {msg.splitlines()[0]}")
            self.to_tag = _first(_TO_TAG_RE.search(msg))
            body = msg.split("\r\n\r\n", 1)[1] if "\r\n\r\n" in msg else ""
            m = _M_AUDIO_RE.search(body)
            c = _C_IP_RE.search(body)
            if m and c:
                self.engine_rtp = (c.group(1), int(m.group(1)))
            break

        self._send(self._frame("ACK", room, cseq, body=""))

    def bye(self, room: str) -> None:
        self._cseq += 1
        self._send(self._frame("BYE", room, self._cseq, body=""))
        msg = self._recv_sip()
        if _status(msg) != 200:
            raise RuntimeError(f"BYE not acked: {msg.splitlines()[0]}")

    def stream_pcmu(self, wire_bytes: bytes, frame_samples: int = 160) -> None:
        """Send μ-law bytes as 20 ms RTP frames to the engine's RTP address."""
        if self.engine_rtp is None:
            raise RuntimeError("invite() must succeed before stream_pcmu()")
        ssrc = random.getrandbits(32)
        seq = random.getrandbits(16)
        ts = 0
        for i in range(0, len(wire_bytes), frame_samples):
            chunk = wire_bytes[i : i + frame_samples]
            pkt = RtpPacket(
                marker=(i == 0),
                payload_type=0,  # PCMU
                sequence=seq & 0xFFFF,
                timestamp=ts & 0xFFFFFFFF,
                ssrc=ssrc,
                payload=chunk,
            )
            self.rtp.sendto(pkt.encode(), self.engine_rtp)
            seq += 1
            ts += frame_samples
            time.sleep(0.020)

    def record_pcmu(self, max_seconds: float) -> bytes:
        """Collect μ-law payloads from RTP packets until quiet or deadline."""
        deadline = time.monotonic() + max_seconds
        quiet_until: Optional[float] = None
        packets: list[RtpPacket] = []
        while True:
            now = time.monotonic()
            if now >= deadline:
                break
            remaining = min(deadline, quiet_until or deadline) - now
            self.rtp.settimeout(max(0.01, remaining))
            try:
                data, _ = self.rtp.recvfrom(2048)
            except socket.timeout:
                break
            pkt = RtpPacket.decode(data)
            if pkt is not None:
                packets.append(pkt)
                quiet_until = time.monotonic() + 0.200
        packets.sort(key=lambda p: p.sequence)
        return b"".join(p.payload for p in packets)

    def close(self) -> None:
        self.sip.close()
        self.rtp.close()

    # ----- plumbing -----

    def _send(self, msg: str) -> None:
        self.sip.sendto(msg.encode(), self.engine)

    def _recv_sip(self) -> str:
        data, _ = self.sip.recvfrom(8192)
        return data.decode(errors="replace")

    def _sdp_offer(self) -> str:
        _, port = self.rtp.getsockname()
        return (
            "v=0\r\n"
            f"o=pyclient {_unique()} 1 IN IP4 {self.local_ip}\r\n"
            "s=-\r\n"
            f"c=IN IP4 {self.local_ip}\r\n"
            "t=0 0\r\n"
            f"m=audio {port} RTP/AVP 0\r\n"
            "a=rtpmap:0 PCMU/8000\r\n"
            "a=sendrecv\r\n"
        )

    def _frame(
        self,
        method: str,
        room: str,
        cseq: int,
        extra: Optional[dict[str, str]] = None,
        body: str = "",
    ) -> str:
        eng_host, eng_port = self.engine
        sip_host, sip_port = self.sip.getsockname()
        branch = f"z9hG4bK-{_unique():x}"
        hdrs = [
            f"{method} sip:{room}@{eng_host}:{eng_port} SIP/2.0",
            f"Via: SIP/2.0/UDP {sip_host}:{sip_port};branch={branch};rport",
            f"From: PyClient <sip:pyclient@{sip_host}:{sip_port}>;tag={self.from_tag}",
            self._to_header(room),
            f"Call-ID: {self.call_id}",
            f"CSeq: {cseq} {method}",
            "Max-Forwards: 70",
            f"Contact: <sip:pyclient@{sip_host}:{sip_port}>",
        ]
        for k, v in (extra or {}).items():
            hdrs.append(f"{k}: {v}")
        if "Content-Length" not in (extra or {}):
            hdrs.append(f"Content-Length: {len(body)}")
        return "\r\n".join(hdrs) + "\r\n\r\n" + body

    def _to_header(self, room: str) -> str:
        eng_host, eng_port = self.engine
        base = f"To: Target <sip:{room}@{eng_host}:{eng_port}>"
        return f"{base};tag={self.to_tag}" if self.to_tag else base


# ---------------------------------------------------------------------------
# Helpers that orchestrate one-process two-party calls, used by demo_call.py.
# ---------------------------------------------------------------------------


def run_two_party(
    engine: tuple[str, int],
    room: str,
    source_pcm: bytes,
    out_wav: str,
    settle_ms: int = 300,
    tail_grace_s: float = 1.0,
) -> Path:
    """Start listener + speaker UAs, exchange audio through the engine.

    Returns the path to the written WAV.

    - ``settle_ms`` — pause after both INVITEs complete but before the
      speaker starts streaming. Gives the engine time to fully wire the
      media bridge (the second INVITE triggers bridge setup; 200 OK
      returns *after* that, but the kernel still needs a beat to route
      the first RTP packet).
    - ``tail_grace_s`` — how long past the end of the streamed audio
      the listener keeps recording. `record_pcmu` stops on 200 ms of
      silence, but we need a hard cap so a lost final packet doesn't
      leave the listener hanging.
    """
    speaker = SipUAC(engine)
    listener = SipUAC(engine)

    # Listener's INVITE must land first so the engine parks its leg.
    # When the speaker's INVITE arrives, the bridge is built and
    # packets can flow immediately. Parallel invites race; serial is
    # predictable.
    listener.invite(room)
    speaker.invite(room)

    # Let the bridge settle on the engine side.
    time.sleep(settle_ms / 1000.0)

    # Recorder runs in its own thread; speaker streams from the main thread.
    received: list[bytes] = []
    src_seconds = len(source_pcm) / 16000.0  # mono PCM16 @ 8 kHz → 16 000 B/s
    record_seconds = src_seconds + tail_grace_s

    def _record() -> None:
        received.append(listener.record_pcmu(max_seconds=record_seconds))

    rec = threading.Thread(target=_record, daemon=True)
    rec.start()

    # Give the recorder thread a moment to arm its first recvfrom before
    # we start blasting packets at the engine.
    time.sleep(0.05)

    speaker.stream_pcmu(pcm16_to_pcmu(source_pcm))
    rec.join(timeout=record_seconds + 1.0)

    try:
        speaker.bye(room)
    except Exception:
        pass
    try:
        listener.bye(room)
    except Exception:
        pass
    speaker.close()
    listener.close()

    write_wav_mono_pcm16_8k(out_wav, pcmu_to_pcm16(received[0] if received else b""))
    return Path(out_wav)


def _status(msg: str) -> Optional[int]:
    line = msg.splitlines()[0] if msg else ""
    parts = line.split(" ", 2)
    if len(parts) < 2 or not parts[1].isdigit():
        return None
    return int(parts[1])


def _first(m: Optional[re.Match[str]]) -> Optional[str]:
    return m.group(1) if m else None
