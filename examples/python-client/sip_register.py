#!/usr/bin/env python3
"""Minimal SIP REGISTER client for outbound trunk registration.

Registers a Contact at a public address so the provider (e.g. Megafon
Multifon) can route inbound INVITEs to smiths-net listening on that port.

Pure stdlib — MD5 digest auth (RFC 2617), UDP or TCP transport.
"""

from __future__ import annotations

import hashlib
import os
import random
import re
import socket
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Optional


def load_env_file(path: Path) -> None:
    """Load KEY=VALUE lines into os.environ."""
    if not path.is_file():
        return
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, val = line.partition("=")
        key = key.strip()
        val = val.strip().strip("'\"")
        if key:
            os.environ[key] = val


def _next_cseq() -> int:
    global _SEQ
    with _SEQ_LOCK:
        _SEQ += 1
        return _SEQ


_SEQ = 0
_SEQ_LOCK = threading.Lock()


def _unique() -> int:
    return random.randint(1, 2**31 - 1)


_WWW_AUTH_RE = re.compile(
    r'(\w+)=("([^"]*)"|([^\s,]+))',
)


def _parse_auth_header(header: str) -> dict[str, str]:
    out: dict[str, str] = {}
    for m in _WWW_AUTH_RE.finditer(header):
        key = m.group(1).lower()
        val = m.group(3) if m.group(3) is not None else m.group(4)
        out[key] = val
    return out


def _md5_hex(s: str) -> str:
    return hashlib.md5(s.encode()).hexdigest()


def _digest_response(
    *,
    username: str,
    password: str,
    realm: str,
    nonce: str,
    method: str,
    uri: str,
    qop: Optional[str],
    nc: str,
    cnonce: str,
) -> str:
    ha1 = _md5_hex(f"{username}:{realm}:{password}")
    ha2 = _md5_hex(f"{method}:{uri}")
    if qop in ("auth", "auth-int"):
        return _md5_hex(f"{ha1}:{nonce}:{nc}:{cnonce}:{qop}:{ha2}")
    return _md5_hex(f"{ha1}:{nonce}:{ha2}")


def _status(msg: str) -> Optional[int]:
    line = msg.splitlines()[0] if msg else ""
    parts = line.split(" ", 2)
    if len(parts) < 2 or not parts[1].isdigit():
        return None
    return int(parts[1])


@dataclass
class SipRegisterConfig:
    username: str
    password: str
    number: str
    realm: str = "multifon.ru"
    registrar: str = "sbc.megafon.ru"
    registrar_port: int = 5060
    public_address: str = "127.0.0.1"
    contact_port: int = 5060
    expires: int = 180
    transport: str = "udp"
    local_ip: str = "0.0.0.0"

    @classmethod
    def from_env(cls) -> "SipRegisterConfig":
        return cls(
            username=os.environ.get("SIP_USERNAME", ""),
            password=os.environ.get("SIP_PASSWORD", ""),
            number=os.environ.get("SIP_NUMBER", os.environ.get("SIP_USERNAME", "")),
            realm=os.environ.get("SIP_REALM", "multifon.ru"),
            registrar=os.environ.get("SIP_REGISTRAR", "sbc.megafon.ru"),
            registrar_port=int(os.environ.get("SIP_REGISTRAR_PORT", "5060")),
            public_address=os.environ.get("SIP_PUBLIC_ADDRESS", "127.0.0.1"),
            contact_port=int(os.environ.get("SIP_CONTACT_PORT", "5060")),
            expires=int(os.environ.get("SIP_REGISTER_EXPIRES", "180")),
            transport=os.environ.get("SIP_TRANSPORT", "udp").lower(),
            local_ip=os.environ.get("SIP_LOCAL_IP", "0.0.0.0"),
        )

    def validate(self) -> None:
        missing = [
            name
            for name, val in (
                ("SIP_USERNAME", self.username),
                ("SIP_PASSWORD", self.password),
                ("SIP_NUMBER", self.number),
                ("SIP_PUBLIC_ADDRESS", self.public_address),
            )
            if not val
        ]
        if missing:
            raise ValueError(f"missing env: {', '.join(missing)}")


class SipRegisterClient:
    """Background SIP REGISTER refresher."""

    def __init__(self, cfg: SipRegisterConfig, *, on_log=print) -> None:
        self.cfg = cfg
        self._on_log = on_log
        self._stop = threading.Event()
        self._thread: Optional[threading.Thread] = None
        self._registered = threading.Event()
        self._sock: Optional[socket.socket] = None
        self.call_id = f"reg-{_unique()}@{cfg.public_address}"
        self.from_tag = f"reg-{_unique()}"
        self._cseq = 0

    def start(self) -> None:
        if self._thread and self._thread.is_alive():
            return
        self._stop.clear()
        self._thread = threading.Thread(target=self._run, daemon=True, name="sip-register")
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        if self._sock:
            try:
                self._sock.close()
            except OSError:
                pass
        if self._thread:
            self._thread.join(timeout=3.0)

    def wait_registered(self, timeout: float = 15.0) -> bool:
        return self._registered.wait(timeout)

    def _log(self, msg: str) -> None:
        self._on_log(f"[register] {msg}")

    def _open_socket(self) -> socket.socket:
        if self.cfg.transport == "tcp":
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.settimeout(10.0)
            sock.bind((self.cfg.local_ip, 0))
            sock.connect((self.cfg.registrar, self.cfg.registrar_port))
            return sock
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.bind((self.cfg.local_ip, 0))
        sock.settimeout(10.0)
        return sock

    def _send(self, msg: str) -> None:
        assert self._sock is not None
        data = msg.encode()
        if self.cfg.transport == "tcp":
            self._sock.sendall(data)
        else:
            self._sock.sendto(data, (self.cfg.registrar, self.cfg.registrar_port))

    def _recv(self) -> str:
        assert self._sock is not None
        if self.cfg.transport == "tcp":
            chunks: list[bytes] = []
            while True:
                part = self._sock.recv(8192)
                if not part:
                    break
                chunks.append(part)
                if b"\r\n\r\n" in part:
                    header_end = b"".join(chunks).find(b"\r\n\r\n")
                    header = b"".join(chunks)[:header_end]
                    cl_match = re.search(rb"(?im)^Content-Length:\s*(\d+)", header)
                    if cl_match:
                        total = header_end + 4 + int(cl_match.group(1))
                        buf = b"".join(chunks)
                        while len(buf) < total:
                            buf += self._sock.recv(8192)
                        return buf[:total].decode(errors="replace")
                    return b"".join(chunks).decode(errors="replace")
            return b"".join(chunks).decode(errors="replace")
        data, _ = self._sock.recvfrom(65535)
        return data.decode(errors="replace")

    def _build_register(
        self,
        *,
        auth: Optional[dict[str, str]] = None,
        expires: Optional[int] = None,
    ) -> str:
        cfg = self.cfg
        self._cseq = _next_cseq()
        cseq = self._cseq
        branch = f"z9hG4bK-reg-{_unique():x}"
        sip_host, sip_port = self._sock.getsockname()[:2] if self._sock else ("0.0.0.0", 0)
        transport = cfg.transport.upper()
        aor = f"sip:{cfg.username}@{cfg.realm}"
        request_uri = f"sip:{cfg.realm}"
        contact = (
            f"<sip:{cfg.number}@{cfg.public_address}:{cfg.contact_port}>;"
            f"expires={expires if expires is not None else cfg.expires}"
        )
        hdrs = [
            f"REGISTER {request_uri} SIP/2.0",
            f"Via: SIP/2.0/{transport} {sip_host}:{sip_port};branch={branch};rport",
            f"Max-Forwards: 70",
            f"From: <{aor}>;tag={self.from_tag}",
            f"To: <{aor}>",
            f"Call-ID: {self.call_id}",
            f"CSeq: {cseq} REGISTER",
            f"Contact: {contact}",
            f"Expires: {expires if expires is not None else cfg.expires}",
            f"User-Agent: smiths-net-asr-bot/0.1",
            "Allow: INVITE, ACK, CANCEL, BYE, OPTIONS",
        ]
        if auth:
            hdrs.append(f"Authorization: {auth['header']}")
        hdrs.append("Content-Length: 0")
        return "\r\n".join(hdrs) + "\r\n\r\n"

    def _auth_header(
        self,
        challenge: dict[str, str],
        method: str,
        uri: str,
    ) -> dict[str, str]:
        cfg = self.cfg
        realm = challenge.get("realm", cfg.realm)
        nonce = challenge["nonce"]
        qop = challenge.get("qop")
        if qop and "," in qop:
            qop = qop.split(",")[0].strip()
        algorithm = challenge.get("algorithm", "MD5").upper()
        if algorithm not in ("MD5", ""):
            raise RuntimeError(f"unsupported digest algorithm: {algorithm}")
        nc = "00000001"
        cnonce = f"{_unique():x}"
        username = challenge.get("username") or cfg.username
        pwd = cfg.password
        response = _digest_response(
            username=username,
            password=pwd,
            realm=realm,
            nonce=nonce,
            method=method,
            uri=uri,
            qop=qop,
            nc=nc,
            cnonce=cnonce,
        )
        parts = [
            f'Digest username="{username}"',
            f'realm="{realm}"',
            f'nonce="{nonce}"',
            f'uri="{uri}"',
            f'response="{response}"',
            f'algorithm=MD5',
        ]
        if qop:
            parts.extend([f"qop={qop}", f"nc={nc}", f'cnonce="{cnonce}"'])
        return {"header": ", ".join(parts)}

    def _register_once(self) -> bool:
        cfg = self.cfg
        request_uri = f"sip:{cfg.realm}"
        # Reuse one persistent socket across refreshes. Megafon delivers inbound
        # INVITEs to the rport (the NAT-mapped source addr:port of our REGISTER);
        # opening a fresh ephemeral socket each time — and closing it right after
        # — let the router's UDP mapping expire within seconds, so calls only
        # landed in the brief window after a REGISTER. Keeping the same socket
        # open and refreshing often (see _run) holds the NAT pinhole open.
        if self._sock is None:
            try:
                self._sock = self._open_socket()
            except OSError as e:
                self._log(f"socket error: {e}")
                return False

        try:
            msg = self._build_register()
            self._send(msg)
            resp = self._recv()
            status = _status(resp)
            if status is None:
                self._log("no valid response")
                return False
            if status == 401 or status == 407:
                auth_line = ""
                for line in resp.splitlines():
                    if line.lower().startswith("www-authenticate:") or line.lower().startswith(
                        "proxy-authenticate:"
                    ):
                        auth_line = line.split(":", 1)[1].strip()
                        break
                if not auth_line:
                    self._log(f"{status} without WWW-Authenticate")
                    return False
                challenge = _parse_auth_header(auth_line)
                auth = self._auth_header(challenge, "REGISTER", request_uri)
                msg = self._build_register(auth=auth)
                self._send(msg)
                resp = self._recv()
                status = _status(resp)
            if status == 200:
                self._log(f"200 OK — Contact {cfg.number}@{cfg.public_address}:{cfg.contact_port}")
                self._registered.set()
                return True
            first = resp.splitlines()[0] if resp else "(empty)"
            self._log(f"failed: {first}")
            return False
        except OSError as e:
            self._log(f"register error: {e}")
            # Drop the socket so the next attempt rebinds a fresh one.
            if self._sock:
                try:
                    self._sock.close()
                except OSError:
                    pass
                self._sock = None
            return False

    def _run(self) -> None:
        cfg = self.cfg
        # Refresh cadence doubles as a NAT keepalive: a router's UDP mapping for
        # our REGISTER source port typically expires in 30–60 s, and Megafon
        # routes inbound INVITEs to that mapping (rport). Refresh well inside
        # that window (cap at 25 s) so the pinhole — and thus inbound calling —
        # stays open continuously, not just for a few seconds after each REGISTER.
        refresh = max(15, min(cfg.expires // 2, 25))
        self._log(
            f"starting → {cfg.registrar}:{cfg.registrar_port} "
            f"as {cfg.username}@{cfg.realm} ({cfg.transport})"
        )
        while not self._stop.is_set():
            ok = self._register_once()
            if not ok:
                self._registered.clear()
                self._log("refresh failed — inbound calls may stop until next success")
                if self._stop.wait(15.0):
                    break
                continue
            if self._stop.wait(refresh):
                break
        self._log("stopped")


def main() -> int:
    import argparse

    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--once", action="store_true", help="register once and exit")
    ap.add_argument(
        "--env",
        type=Path,
        default=Path("examples/multifon.env"),
        help="env file with SIP_* variables (default: examples/multifon.env)",
    )
    args = ap.parse_args()
    load_env_file(args.env)
    cfg = SipRegisterConfig.from_env()
    cfg.validate()
    client = SipRegisterClient(cfg)
    if args.once:
        ok = client._register_once()
        return 0 if ok else 1
    client.start()
    try:
        if client.wait_registered(timeout=20.0):
            print("[register] registered, refreshing in background")
            while True:
                time.sleep(1)
        else:
            print("[register] registration timed out")
            return 1
    except KeyboardInterrupt:
        pass
    finally:
        client.stop()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
