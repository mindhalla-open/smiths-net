#!/usr/bin/env python3
"""Generic MQTT broker bridge — slice 4.5 / P21.

Advertises `bridge.mqtt` with one method, `publish(topic, payload,
qos?, retain?)`. Opens a short-lived TCP connection per request
(CONNECT → PUBLISH → DISCONNECT), so there's no long-lived
connection state to manage across the JSON-RPC boundary. That
costs ~1 RTT per call — fine for event-type traffic (one publish
per call-start / call-ended), not right for high-rate telemetry.

Environment:
  MQTT_HOST      — broker host (default `127.0.0.1`)
  MQTT_PORT      — broker port (default `1883`)
  MQTT_USERNAME  — optional
  MQTT_PASSWORD  — optional
  MQTT_CLIENT_ID — default `smiths-net-bridge-<pid>`
  MQTT_TIMEOUT_SECS — default 5

Stdlib-only. Implements MQTT 3.1.1 CONNECT + PUBLISH QoS 0/1 in
~100 LOC (packet format is genuinely that small).
"""

from __future__ import annotations

import json
import os
import socket
import struct
import sys
import time

MQTT_HOST = os.environ.get("MQTT_HOST", "127.0.0.1")
MQTT_PORT = int(os.environ.get("MQTT_PORT", "1883"))
MQTT_USERNAME = os.environ.get("MQTT_USERNAME", "")
MQTT_PASSWORD = os.environ.get("MQTT_PASSWORD", "")
MQTT_CLIENT_ID = os.environ.get("MQTT_CLIENT_ID", f"smiths-net-bridge-{os.getpid()}")
MQTT_TIMEOUT = float(os.environ.get("MQTT_TIMEOUT_SECS", "5"))

DESCRIPTOR = {
    "capability": "bridge.mqtt",
    "plugin": "mqtt-bridge",
    "model_id": "mqtt-3.1.1",
    "abi": "1.0",
    "description": f"MQTT 3.1.1 bridge → {MQTT_HOST}:{MQTT_PORT}.",
    "priority": 30,
    "latency_ms": {"p50": 20, "p95": 150},
    "concurrency": {"max_in_flight": 8},
}


def reply(id_, *, result=None, error=None):
    frame = {"jsonrpc": "2.0"}
    if id_ is not None:
        frame["id"] = id_
    if error is not None:
        frame["error"] = error
    else:
        frame["result"] = result
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()


def log(msg: str) -> None:
    sys.stderr.write(f"[mqtt-bridge] {msg}\n")
    sys.stderr.flush()


# ---------------------------------------------------------------------
# MQTT 3.1.1 packet encoding (stdlib-only).
# ---------------------------------------------------------------------


def _encode_remaining_length(n: int) -> bytes:
    """MQTT's variable-length encoding for the "remaining length"
    field. 1-4 bytes, 7 data bits + 1 continuation bit each."""
    out = bytearray()
    while True:
        byte = n % 128
        n //= 128
        if n > 0:
            byte |= 0x80
        out.append(byte)
        if n == 0:
            return bytes(out)


def _encode_string(s: str) -> bytes:
    data = s.encode("utf-8")
    return struct.pack("!H", len(data)) + data


def _build_connect() -> bytes:
    # Variable header: protocol name "MQTT", level 4 (3.1.1),
    # connect flags, keepalive.
    flags = 0x02  # clean session
    if MQTT_USERNAME:
        flags |= 0x80
    if MQTT_PASSWORD:
        flags |= 0x40
    var_hdr = _encode_string("MQTT") + struct.pack("!BBH", 4, flags, 60)
    payload = _encode_string(MQTT_CLIENT_ID)
    if MQTT_USERNAME:
        payload += _encode_string(MQTT_USERNAME)
    if MQTT_PASSWORD:
        payload += _encode_string(MQTT_PASSWORD)
    body = var_hdr + payload
    return bytes([0x10]) + _encode_remaining_length(len(body)) + body


def _build_publish(topic: str, payload: bytes, qos: int, retain: bool, pkt_id: int) -> bytes:
    fixed = 0x30 | ((qos & 0x03) << 1) | (0x01 if retain else 0)
    var_hdr = _encode_string(topic)
    if qos > 0:
        var_hdr += struct.pack("!H", pkt_id)
    body = var_hdr + payload
    return bytes([fixed]) + _encode_remaining_length(len(body)) + body


def _build_disconnect() -> bytes:
    return bytes([0xE0, 0x00])


def _read_fixed_header(sock: socket.socket) -> tuple[int, int]:
    """Read one MQTT fixed header, return (control_byte,
    remaining_length)."""
    hdr = sock.recv(1)
    if not hdr:
        raise RuntimeError("broker closed before CONNACK")
    control = hdr[0]
    multiplier = 1
    value = 0
    for _ in range(4):
        b = sock.recv(1)
        if not b:
            raise RuntimeError("broker closed mid remaining-length")
        value += (b[0] & 0x7F) * multiplier
        if b[0] & 0x80 == 0:
            return control, value
        multiplier *= 128
    raise RuntimeError("malformed remaining-length")


def _read_exact(sock: socket.socket, n: int) -> bytes:
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise RuntimeError("broker closed mid body")
        buf.extend(chunk)
    return bytes(buf)


def _connect() -> socket.socket:
    sock = socket.create_connection((MQTT_HOST, MQTT_PORT), timeout=MQTT_TIMEOUT)
    sock.sendall(_build_connect())
    control, remaining = _read_fixed_header(sock)
    if control & 0xF0 != 0x20:
        raise RuntimeError(f"expected CONNACK (0x20), got 0x{control:02x}")
    body = _read_exact(sock, remaining)
    if len(body) < 2:
        raise RuntimeError("CONNACK body too short")
    rc = body[1]
    if rc != 0:
        raise RuntimeError(f"broker rejected CONNECT (rc={rc})")
    return sock


# ---------------------------------------------------------------------
# JSON-RPC methods
# ---------------------------------------------------------------------


def publish(params: dict) -> dict:
    topic = params.get("topic")
    payload = params.get("payload")
    qos = int(params.get("qos", 0))
    retain = bool(params.get("retain", False))
    if not isinstance(topic, str) or not topic:
        raise ValueError("`topic` must be a non-empty string")
    if qos not in (0, 1):
        raise ValueError("`qos` must be 0 or 1 (QoS 2 not supported)")
    if isinstance(payload, (dict, list)):
        body = json.dumps(payload).encode("utf-8")
    elif isinstance(payload, str):
        body = payload.encode("utf-8")
    elif payload is None:
        body = b""
    else:
        raise ValueError("`payload` must be string / object / array / null")

    started = time.monotonic()
    sock = _connect()
    try:
        pkt_id = 1
        sock.sendall(_build_publish(topic, body, qos, retain, pkt_id))
        if qos == 1:
            control, remaining = _read_fixed_header(sock)
            if control & 0xF0 != 0x40:
                raise RuntimeError(f"expected PUBACK (0x40), got 0x{control:02x}")
            _read_exact(sock, remaining)
        sock.sendall(_build_disconnect())
    finally:
        try:
            sock.close()
        except OSError:
            pass
    return {
        "topic": topic,
        "qos": qos,
        "retain": retain,
        "bytes": len(body),
        "elapsed_ms": int((time.monotonic() - started) * 1000),
    }


def main() -> int:
    log(
        f"ready (host={MQTT_HOST}:{MQTT_PORT} "
        f"auth={'on' if MQTT_USERNAME else 'off'})"
    )
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError as e:
            reply(None, error={"code": -32700, "message": f"parse error: {e}"})
            continue

        id_ = req.get("id")
        method = req.get("method")
        params = req.get("params") or {}

        if method == "describe_capabilities":
            reply(id_, result=[DESCRIPTOR])
        elif method == "publish":
            try:
                reply(id_, result=publish(params))
            except ValueError as e:
                reply(id_, error={"code": -32602, "message": str(e)})
            except Exception as e:  # noqa: BLE001
                reply(id_, error={"code": -32603, "message": f"publish failed: {e}"})
        elif method == "shutdown":
            reply(id_, result=None)
            log("shutdown — exiting")
            return 0
        elif method == "ping":
            reply(id_, result={"ok": True})
        else:
            reply(id_, error={"code": -32601, "message": f"method `{method}` not implemented"})
    return 0


if __name__ == "__main__":
    sys.exit(main())
