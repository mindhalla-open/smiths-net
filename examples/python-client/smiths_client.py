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
import os
import random
import re
import socket
import struct
import threading
import time
import wave
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Optional


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


# G.711 A-law (PCMA, payload type 8) — Megafon preferred codec.
_ALAW_AEX = 0x55
_ALAW_ASE = 0xD5


def linear_to_alaw(sample: int) -> int:
    sign = sample >> 8
    if sign < 0:
        sample = -sample - 8
        mask = _ALAW_ASE
    else:
        mask = _ALAW_AEX
    if sample > 0x7FFF:
        sample = 0x7FFF
    seg = 7
    for shift in (14, 12, 10, 8, 6, 4, 2):
        if sample >= (1 << shift):
            break
        seg -= 1
    aval = (sample >> (seg + 3)) & 0x0F
    return (aval | (seg << 4)) ^ mask


def alaw_to_linear(a: int) -> int:
    a ^= 0x55
    sign = a & 0x80
    seg = (a & 0x70) >> 4
    aval = a & 0x0F
    t = (aval << 4) + 8
    if seg >= 1:
        t += 0x100
    if seg > 1:
        t <<= seg - 1
    return -t if sign else t


def pcm16_to_pcma(samples: bytes) -> bytes:
    n = len(samples) // 2
    shorts = struct.unpack_from(f"<{n}h", samples)
    return bytes(linear_to_alaw(s) for s in shorts)


def pcma_to_pcm16(data: bytes) -> bytes:
    return b"".join(struct.pack("<h", alaw_to_linear(b)) for b in data)


def wire_to_pcm16(data: bytes, *, payload_type: int) -> bytes:
    if payload_type == 8:
        return pcma_to_pcm16(data)
    return pcmu_to_pcm16(data)


def payload_energy(payload: bytes, *, payload_type: int) -> float:
    """Mean absolute linear sample value for one G.711 RTP frame (~20 ms)."""
    if not payload:
        return 0.0
    if payload_type == 8:
        return sum(abs(alaw_to_linear(b)) for b in payload) / len(payload)
    return sum(abs(ulaw_to_linear(b)) for b in payload) / len(payload)


def pcm16_to_wire(samples: bytes, *, payload_type: int) -> bytes:
    if payload_type == 8:
        return pcm16_to_pcma(samples)
    return pcm16_to_pcmu(samples)


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
    write_wav_mono_pcm16(path, samples, sample_rate=8000)


def upsample_pcm16(pcm: bytes, from_hz: int, to_hz: int) -> bytes:
    """Duplicate samples — good enough for telephony → desktop playback."""
    if from_hz >= to_hz:
        return pcm
    ratio = to_hz // from_hz
    if ratio <= 1:
        return pcm
    samples = struct.unpack(f"<{len(pcm) // 2}h", pcm)
    out = bytearray()
    for s in samples:
        out.extend(struct.pack(f"<{ratio}h", *([s] * ratio)))
    return bytes(out)


def write_wav_mono_pcm16(path: str, samples: bytes, *, sample_rate: int = 8000) -> None:
    """Write raw PCM16 LE bytes as a mono WAV at ``sample_rate`` Hz."""
    with wave.open(path, "wb") as f:
        f.setnchannels(1)
        f.setsampwidth(2)
        f.setframerate(sample_rate)
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

    def __init__(
        self,
        engine: tuple[str, int],
        local_ip: str = "127.0.0.1",
        *,
        codec: str = "pcmu",
        rtp_host: Optional[str] = None,
    ) -> None:
        self.engine = engine
        self.local_ip = local_ip
        # When the engine advertises a public/wildcard media IP in SDP
        # (e.g. media.advertise_ip for an external trunk), a co-located
        # client bound to loopback cannot sendto() that address (EINVAL).
        # Default: send RTP to the engine's signaling host and take only
        # the port from the SDP answer.
        if rtp_host is None and local_ip in ("127.0.0.1", "::1", "localhost"):
            rtp_host = engine[0]
        self.rtp_host = rtp_host
        codec = codec.lower()
        if codec in ("pcma", "alaw", "g711a"):
            self.payload_type = 8
            self.codec_name = "PCMA"
        else:
            self.payload_type = 0
            self.codec_name = "PCMU"
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
        # Diagnostics from the most recent record_wire() call.
        self.last_peak = 0.0
        self.last_rms = 0.0
        self.last_total_secs = 0.0
        self.last_speech_secs = 0.0
        self.last_speech_detected = False
        self.last_rtp_pkts = 0
        self.last_barge_in = False
        self.last_speculation_valid = False
        # Continuous silence keepalive state (symmetric-RTP trunks).
        self._ka_seq = random.getrandbits(16)
        self._ka_ts = 0
        self._ka_ssrc = random.getrandbits(32)

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
                sdp_ip = c.group(1)
                sdp_port = int(m.group(1))
                host = self.rtp_host or sdp_ip
                # A wildcard / unspecified media IP is never a valid
                # sendto() target — fall back to the engine host.
                if sdp_ip in ("0.0.0.0", "::") and not self.rtp_host:
                    host = self.engine[0]
                if sdp_port <= 0 or sdp_port > 65535:
                    raise RuntimeError(f"invalid RTP port in SDP: {sdp_port}")
                self.engine_rtp = (host, sdp_port)
                if host != sdp_ip:
                    print(f"[sip] RTP target {host}:{sdp_port} (SDP c={sdp_ip})")
            break

        self._send(self._frame("ACK", room, cseq, body=""))

    def bye(self, room: str) -> None:
        self._cseq += 1
        self._send(self._frame("BYE", room, self._cseq, body=""))
        msg = self._recv_sip()
        if _status(msg) != 200:
            raise RuntimeError(f"BYE not acked: {msg.splitlines()[0]}")

    def prime_rtp(self, seconds: float = 0.25, frame_samples: int = 160) -> None:
        """Send a short burst of codec silence to latch the RTP relay.

        The engine's RTP relay learns the bot's media source address from
        the FIRST packet the bot transmits; only then can it forward the
        caller's audio back to us and (for symmetric-RTP carriers) accept
        our outbound media. Sending silence up-front opens that path
        immediately so the greeting that follows is never clipped while
        the relay is still latching.
        """
        if self.engine_rtp is None:
            raise RuntimeError("invite() must succeed before prime_rtp()")
        # G.711 digital silence: A-law 0xD5, µ-law 0xFF.
        byte = 0xD5 if self.payload_type == 8 else 0xFF
        frames = max(1, int(seconds * 8000.0 / frame_samples))
        silence = bytes([byte]) * (frames * frame_samples)
        self.stream_wire(silence, frame_samples=frame_samples)

    def _rtp_keepalive_enabled(self) -> bool:
        return os.environ.get("RTP_KEEPALIVE", "1").lower() in ("1", "true", "yes", "on")

    def send_rtp_keepalive_frame(self, frame_samples: int = 160) -> None:
        """Send one 20 ms G.711 silence frame (keeps symmetric-RTP trunks open)."""
        if not self._rtp_keepalive_enabled() or self.engine_rtp is None:
            return
        byte = 0xD5 if self.payload_type == 8 else 0xFF
        payload = bytes([byte]) * frame_samples
        raw = RtpPacket(
            marker=False,
            payload_type=self.payload_type,
            sequence=self._ka_seq & 0xFFFF,
            timestamp=self._ka_ts & 0xFFFFFFFF,
            ssrc=self._ka_ssrc,
            payload=payload,
        ).encode()
        self._ka_seq += 1
        self._ka_ts += frame_samples
        try:
            self.rtp.sendto(raw, self.engine_rtp)
        except OSError:
            pass

    def pause_with_keepalive(self, seconds: float, frame_samples: int = 160) -> None:
        """Wait while sending RTP silence so the carrier keeps forwarding inbound audio."""
        if seconds <= 0:
            return
        frame_secs = frame_samples / 8000.0
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            self.send_rtp_keepalive_frame(frame_samples)
            time.sleep(frame_secs)

    def stream_pcmu(self, wire_bytes: bytes, frame_samples: int = 160) -> None:
        """Send G.711 wire bytes as 20 ms RTP frames to the engine."""
        self.stream_wire(wire_bytes, frame_samples=frame_samples)

    def stream_wire(
        self,
        wire_bytes: bytes,
        frame_samples: int = 160,
        *,
        allow_barge_in: bool = False,
        barge_threshold: Optional[float] = None,
        barge_min_frames: int = 10,
    ) -> bool:
        """Send codec wire bytes (PCMU/PCMA) as 20 ms RTP frames.

        Packets are encoded up-front and paced against an absolute clock
        so per-frame Python work never accumulates into timing drift.
        Sleep-after-send (the naive approach) makes the inter-packet gap
        ``20 ms + encode/sendto cost`` which slowly starves the remote
        jitter buffer and produces choppy / "robotic" audio.

        With ``allow_barge_in=True`` the inbound RTP is monitored during
        the pacing gaps; if the caller speaks above ``barge_threshold``
        for ``barge_min_frames`` consecutive frames, playback aborts
        early. Returns ``True`` when interrupted, ``False`` otherwise.
        """
        if self.engine_rtp is None:
            raise RuntimeError("invite() must succeed before stream_wire()")
        ssrc = random.getrandbits(32)
        seq = random.getrandbits(16)
        ts = 0

        frame_secs = frame_samples / 8000.0  # G.711 is always 8 kHz
        packets: list[bytes] = []
        for i in range(0, len(wire_bytes), frame_samples):
            chunk = wire_bytes[i : i + frame_samples]
            packets.append(
                RtpPacket(
                    marker=(i == 0),
                    payload_type=self.payload_type,
                    sequence=seq & 0xFFFF,
                    timestamp=ts & 0xFFFFFFFF,
                    ssrc=ssrc,
                    payload=chunk,
                ).encode()
            )
            seq += 1
            ts += frame_samples

        if barge_threshold is None:
            barge_threshold = float(os.environ.get("BARGE_THRESHOLD", "900"))

        loud_run = 0
        interrupted = False
        start = time.monotonic()
        for n, raw in enumerate(packets):
            self.rtp.sendto(raw, self.engine_rtp)
            target = start + (n + 1) * frame_secs

            if not allow_barge_in:
                delay = target - time.monotonic()
                if delay > 0:
                    time.sleep(delay)
                continue

            # Use the pacing gap to listen for caller speech (barge-in).
            while True:
                delay = target - time.monotonic()
                if delay <= 0:
                    break
                self.rtp.settimeout(min(delay, 0.02))
                try:
                    data, _ = self.rtp.recvfrom(2048)
                except socket.timeout:
                    continue
                except OSError:
                    break
                pkt = RtpPacket.decode(data)
                if pkt is None:
                    continue
                energy = payload_energy(pkt.payload, payload_type=pkt.payload_type)
                if energy >= barge_threshold:
                    loud_run += 1
                    if loud_run >= barge_min_frames:
                        interrupted = True
                        break
                else:
                    loud_run = 0
            if interrupted:
                break

        self.last_barge_in = interrupted
        return interrupted

    def record_pcmu(
        self,
        max_seconds: float,
        *,
        quiet_secs: float = 0.2,
        vad: bool = True,
        **vad_kwargs: float | bool,
    ) -> bytes:
        """Collect G.711 payloads from RTP (PCMU or PCMA per packet PT)."""
        return self.record_wire(
            max_seconds,
            quiet_secs=quiet_secs,
            vad=vad,
            **vad_kwargs,  # type: ignore[arg-type]
        )

    def record_wire(
        self,
        max_seconds: float,
        *,
        quiet_secs: float = 0.2,
        vad: bool = True,
        silence_secs: float = 0.8,
        vad_threshold: float = 400.0,
        min_speech_secs: float = 0.25,
        pre_speech_secs: float = 0.15,
        speculate_silence_secs: float = 0.0,
        on_speculate: Optional[Callable[[bytes], None]] = None,
        on_resume: Optional[Callable[[], None]] = None,
    ) -> bytes:
        """Collect G.711 RTP payloads until timeout or end-of-utterance.

        With ``vad=True`` (default), stop after ``silence_secs`` of low-energy
        frames once the caller has spoken for at least ``min_speech_secs``.
        Telephony RTP keeps flowing during silence (comfort noise), so idle
        socket timeout alone is not enough to detect pauses.

        Speculative turn-taking: when ``speculate_silence_secs`` (< ``silence_secs``)
        is set, ``on_speculate(snapshot_wire)`` fires once after that shorter
        pause so the caller's reply can be prepared early; if the caller resumes
        talking before the full ``silence_secs`` commit, ``on_resume()`` fires so
        the speculative work can be discarded. ``last_speculation_valid`` is True
        on return iff a speculation fired and was not invalidated by a resume.
        """
        if vad:
            return self._record_wire_vad(
                max_seconds,
                silence_secs=silence_secs,
                vad_threshold=vad_threshold,
                min_speech_secs=min_speech_secs,
                pre_speech_secs=pre_speech_secs,
                speculate_silence_secs=speculate_silence_secs,
                on_speculate=on_speculate,
                on_resume=on_resume,
            )
        return self._record_wire_idle(max_seconds, quiet_secs=quiet_secs)

    def _record_wire_idle(
        self,
        max_seconds: float,
        *,
        quiet_secs: float,
    ) -> bytes:
        deadline = time.monotonic() + max_seconds
        quiet_until: Optional[float] = None
        packets: list[RtpPacket] = []
        frame_secs = 0.020
        next_keepalive = time.monotonic()
        while True:
            now = time.monotonic()
            if now >= deadline:
                break
            if now >= next_keepalive:
                self.send_rtp_keepalive_frame()
                next_keepalive = now + frame_secs
            remaining = min(deadline, quiet_until or deadline) - now
            self.rtp.settimeout(max(0.01, remaining))
            try:
                data, _ = self.rtp.recvfrom(2048)
            except socket.timeout:
                break
            pkt = RtpPacket.decode(data)
            if pkt is not None:
                packets.append(pkt)
                quiet_until = time.monotonic() + quiet_secs
        packets.sort(key=lambda p: p.sequence)
        self.last_rtp_pkts = len(packets)
        return b"".join(p.payload for p in packets)

    def _record_wire_vad(
        self,
        max_seconds: float,
        *,
        silence_secs: float,
        vad_threshold: float,
        min_speech_secs: float,
        pre_speech_secs: float,
        speculate_silence_secs: float = 0.0,
        on_speculate: Optional[Callable[[bytes], None]] = None,
        on_resume: Optional[Callable[[], None]] = None,
    ) -> bytes:
        deadline = time.monotonic() + max_seconds
        frame_secs = 0.020
        silence_frames_needed = max(1, int(silence_secs / frame_secs))
        min_speech_frames = max(1, int(min_speech_secs / frame_secs))
        pre_frames_max = max(1, int(pre_speech_secs / frame_secs))
        # Provisional end-of-utterance: prepare the reply after a short pause,
        # but only commit once `silence_frames_needed` is reached. Must be
        # strictly shorter than the commit threshold to buy any head start.
        speculate_frames = 0
        if on_speculate is not None and 0.0 < speculate_silence_secs < silence_secs:
            speculate_frames = max(1, int(speculate_silence_secs / frame_secs))

        packets: list[RtpPacket] = []
        pre_roll: list[RtpPacket] = []
        speech_started = False
        speech_frames = 0
        silent_run = 0
        loud_run = 0
        speculated = False
        self.last_speculation_valid = False

        def _maybe_speculate() -> None:
            nonlocal speculated
            if (
                speculate_frames
                and not speculated
                and speech_frames >= min_speech_frames
                and silent_run >= speculate_frames
            ):
                speculated = True
                snapshot = b"".join(
                    p.payload for p in sorted(packets, key=lambda p: p.sequence)
                )
                try:
                    on_speculate(snapshot)  # type: ignore[misc]
                except Exception:
                    pass

        def _maybe_resume() -> None:
            nonlocal speculated
            if speculated:
                speculated = False
                self.last_speculation_valid = False
                if on_resume is not None:
                    try:
                        on_resume()
                    except Exception:
                        pass
        # A single loud frame amid silence (line-noise/echo spike) must not
        # reset the end-of-utterance timer — require this many *consecutive*
        # loud frames before we treat the caller as still speaking.
        spike_tolerance = max(1, int(os.environ.get("VAD_SPIKE_TOLERANCE", "2")))
        noise_energies: list[float] = []
        eff_threshold = vad_threshold

        peak = 0.0
        energy_sum = 0.0
        energy_n = 0
        next_keepalive = time.monotonic()

        while True:
            now = time.monotonic()
            if now >= deadline:
                break
            if now >= next_keepalive:
                self.send_rtp_keepalive_frame()
                next_keepalive = now + frame_secs
            self.rtp.settimeout(max(0.01, min(0.05, deadline - now)))
            try:
                data, _ = self.rtp.recvfrom(2048)
            except socket.timeout:
                if speech_started:
                    silent_run += 1
                    if (
                        speech_frames >= min_speech_frames
                        and silent_run >= silence_frames_needed
                    ):
                        if speculated:
                            self.last_speculation_valid = True
                        break
                    _maybe_speculate()
                continue

            pkt = RtpPacket.decode(data)
            if pkt is None:
                continue

            energy = payload_energy(pkt.payload, payload_type=pkt.payload_type)
            peak = max(peak, energy)
            energy_sum += energy
            energy_n += 1

            if not speech_started:
                noise_energies.append(energy)
                if len(noise_energies) >= 10:
                    noise_floor = sum(noise_energies) / len(noise_energies)
                    eff_threshold = max(vad_threshold, noise_floor * 3.5)
                pre_roll.append(pkt)
                if len(pre_roll) > pre_frames_max:
                    pre_roll.pop(0)
                if energy >= eff_threshold:
                    speech_started = True
                    packets.extend(pre_roll)
                    pre_roll.clear()
                    packets.append(pkt)
                    speech_frames = 1
                    silent_run = 0
                continue

            packets.append(pkt)
            if energy >= eff_threshold:
                speech_frames += 1
                loud_run += 1
                # Only an actual run of speech (not a lone spike) resets the
                # silence countdown — kills the 9 s "runaway capture" caused
                # by sporadic line noise after the caller stopped talking.
                if loud_run >= spike_tolerance:
                    silent_run = 0
                    # Caller picked the phrase back up after a pause — the
                    # speculative reply was based on a partial utterance, so
                    # discard it and keep listening.
                    _maybe_resume()
            else:
                loud_run = 0
                silent_run += 1
                if (
                    speech_frames >= min_speech_frames
                    and silent_run >= silence_frames_needed
                ):
                    if speculated:
                        self.last_speculation_valid = True
                    break
                _maybe_speculate()

        packets.sort(key=lambda p: p.sequence)
        self.last_peak = peak
        self.last_rms = energy_sum / energy_n if energy_n else 0.0
        self.last_total_secs = energy_n * frame_secs
        self.last_speech_secs = speech_frames * frame_secs
        self.last_speech_detected = speech_started and speech_frames >= min_speech_frames
        self.last_rtp_pkts = energy_n
        return b"".join(p.payload for p in packets)

    def drain_rtp(self, seconds: float = 0.2) -> int:
        """Discard buffered inbound RTP (e.g. our own echo tail) before
        recording the next turn. Returns the number of packets dropped."""
        dropped = 0
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            self.rtp.settimeout(0.01)
            try:
                self.rtp.recvfrom(2048)
                dropped += 1
            except socket.timeout:
                continue
            except OSError:
                break
        return dropped

    def wait_for_remote_rtp(self, timeout: float = 25.0) -> bool:
        """Block until the first inbound RTP packet (remote party bridged).

        On trunk calls the bot often joins the rendezvous *before* the
        carrier connects the caller. Playing TTS before remote media is
        up means the caller hears silence — wait here first.
        """
        deadline = time.monotonic() + timeout
        self.rtp.settimeout(0.2)
        while time.monotonic() < deadline:
            try:
                self.rtp.recvfrom(2048)
                return True
            except socket.timeout:
                continue
            except OSError:
                break
        return False

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
        pt = self.payload_type
        return (
            "v=0\r\n"
            f"o=pyclient {_unique()} 1 IN IP4 {self.local_ip}\r\n"
            "s=-\r\n"
            f"c=IN IP4 {self.local_ip}\r\n"
            "t=0 0\r\n"
            f"m=audio {port} RTP/AVP {pt}\r\n"
            f"a=rtpmap:{pt} {self.codec_name}/8000\r\n"
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
        received.append(listener.record_pcmu(max_seconds=record_seconds, vad=False))

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
