#!/usr/bin/env python3
"""A sidecar speaking smiths-net's length-prefixed binary framing.

Stdlib only, like every other sample here: the binary format is a
fixed-offset layout precisely so it can be read without a schema
compiler or a code generator.

Each frame on stdin and stdout is:

    4 bytes   big-endian uint32 length of the body
    N bytes   an Envelope in the fixed-offset layout

and the Envelope itself is:

    offset  size  field
    0       4     magic = b"SMEV"
    4       2     version = 1            (little-endian)
    6       1     kind: 1=request 2=response 3=notification
    7       1     reserved
    8       8     id                      (little-endian)
    16      4     error_code              (little-endian, responses)
    20      4     method_len              (little-endian)
    24      4     params_len
    28      4     result_len
    32      4     err_msg_len
    36      ...   method ++ params ++ result ++ err_msg, all UTF-8

Every length is little-endian; only the outer frame prefix is big-
endian. Fields a given kind does not use are zero-length.

Run the engine against this directory and call the `echo` method.
"""

import json
import struct
import sys

MAGIC = b"SMEV"
VERSION = 1
HEADER = 36
KIND_REQUEST, KIND_RESPONSE, KIND_NOTIFICATION = 1, 2, 3


def read_exact(n):
    """Read exactly `n` bytes, or None once the engine closes."""
    buf = b""
    while len(buf) < n:
        chunk = sys.stdin.buffer.read(n - len(buf))
        if not chunk:
            return None
        buf += chunk
    return buf


def decode(body):
    """Return (kind, id, method, params) from one envelope body."""
    if len(body) < HEADER or body[:4] != MAGIC:
        raise ValueError("not an envelope")
    kind = body[6]
    ident = struct.unpack_from("<q", body, 8)[0]
    m_len, p_len, _r_len, _e_len = struct.unpack_from("<IIII", body, 20)
    at = HEADER
    method = body[at:at + m_len].decode()
    at += m_len
    params = body[at:at + p_len].decode()
    return kind, ident, method, params


def encode(kind, ident=0, method="", params="", result="", err_msg="", err_code=0):
    m, p, r, e = (s.encode() for s in (method, params, result, err_msg))
    head = MAGIC + struct.pack("<HBBqi", VERSION, kind, 0, ident, err_code)
    head += struct.pack("<IIII", len(m), len(p), len(r), len(e))
    return head + m + p + r + e


def send(body):
    sys.stdout.buffer.write(struct.pack(">I", len(body)) + body)
    sys.stdout.buffer.flush()


def log(msg):
    # Anything on stderr is logged by the engine against this plugin.
    sys.stderr.write(f"[binary-frames-py] {msg}\n")
    sys.stderr.flush()


DESCRIPTOR = [
    {
        "capability": "ai.echo",
        "plugin": "binary-frames-py",
        "model_id": "echo",
        "abi": "1.0",
        "description": "Echoes its params back; demonstrates binary framing.",
    }
]


def main():
    log("ready")
    while True:
        prefix = read_exact(4)
        if prefix is None:
            break
        (n,) = struct.unpack(">I", prefix)
        body = read_exact(n)
        if body is None:
            break
        try:
            kind, ident, method, params = decode(body)
        except ValueError as e:
            log(f"bad frame: {e}")
            break
        if kind != KIND_REQUEST:
            continue

        if method == "describe_capabilities":
            send(encode(KIND_RESPONSE, ident, result=json.dumps(DESCRIPTOR)))
        elif method == "invoke":
            # A notification first, to show the id-less path.
            send(encode(KIND_NOTIFICATION, method="progress", params=params))
            send(encode(KIND_RESPONSE, ident, result=json.dumps({"echo": params})))
        else:
            send(
                encode(
                    KIND_RESPONSE,
                    ident,
                    err_msg=f"unknown method `{method}`",
                    err_code=-32601,
                )
            )


if __name__ == "__main__":
    main()
