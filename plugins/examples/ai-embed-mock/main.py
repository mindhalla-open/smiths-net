#!/usr/bin/env python3
"""Reference sidecar — `ai.embed` capability, vectors are hash-derived.

Produces a fixed-dimension embedding for each input string by seeding a
reproducible pseudo-random stream from the SHA-256 of the input. That
keeps the tests deterministic and the plugin self-contained (no model
weights, no pip deps). A real replacement swaps `fake_embed()` for a
call into Sentence-Transformers / OpenAI / Cohere; the descriptor
`controls` and I/O shape stay the same.
"""

from __future__ import annotations

import hashlib
import json
import math
import struct
import sys

DIMENSION = 128

DESCRIPTOR = {
    "capability": "ai.embed",
    "plugin": "ai-embed-mock",
    "model_id": "mock-embed-v1",
    "abi": "1.0",
    "description": "Mock embedding model (deterministic, normalized).",
    "dimension": DIMENSION,
    "input_formats": ["text"],
    "max_input_tokens": 2048,
    "controls": {
        "normalize": {"type": "boolean", "default": True},
    },
    "latency_ms": {"p50": 15, "p95": 60},
    "concurrency": {"max_in_flight": 8},
}


def reply(id_: int | None, *, result=None, error=None) -> None:
    frame: dict = {"jsonrpc": "2.0"}
    if id_ is not None:
        frame["id"] = id_
    if error is not None:
        frame["error"] = error
    else:
        frame["result"] = result
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()


def log(msg: str) -> None:
    sys.stderr.write(f"[ai-embed-mock] {msg}\n")
    sys.stderr.flush()


def vector_from(text: str, normalize: bool) -> list[float]:
    """Deterministic PRNG seeded from sha256(text), emitting `DIMENSION`
    floats in ~[-1, 1]. Optional L2 normalization so downstream cosine
    comparisons behave."""
    seed = hashlib.sha256(text.encode("utf-8")).digest()
    # Expand seed with SHA-256 chaining until we have 4*DIMENSION bytes.
    stream = bytearray()
    current = seed
    while len(stream) < DIMENSION * 4:
        stream.extend(current)
        current = hashlib.sha256(current).digest()
    raw = bytes(stream[: DIMENSION * 4])
    ints = struct.unpack(f">{DIMENSION}I", raw)
    # Scale each u32 into [-1, 1].
    vec = [((i / 0xFFFFFFFF) * 2.0) - 1.0 for i in ints]
    if normalize:
        norm = math.sqrt(sum(v * v for v in vec))
        if norm > 0:
            vec = [v / norm for v in vec]
    return vec


def fake_embed(params: dict) -> dict:
    inputs = params.get("inputs")
    if not isinstance(inputs, list) or not inputs:
        raise ValueError("`inputs` must be a non-empty array of strings")
    controls = params.get("controls") or {}
    normalize = bool(controls.get("normalize", True))
    embeddings = []
    for item in inputs:
        if not isinstance(item, str):
            raise ValueError("`inputs` items must be strings")
        embeddings.append(vector_from(item, normalize))
    return {
        "embeddings": embeddings,
        "dimension": DIMENSION,
        "normalized": normalize,
        "count": len(embeddings),
    }


def main() -> int:
    log("ready")
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

        if method == "describe_capabilities":
            reply(id_, result=[DESCRIPTOR])
        elif method == "embed":
            try:
                reply(id_, result=fake_embed(req.get("params") or {}))
            except ValueError as e:
                reply(id_, error={"code": -32602, "message": str(e)})
            except Exception as e:  # noqa: BLE001
                reply(id_, error={"code": -32603, "message": f"embed failed: {e}"})
        elif method == "shutdown":
            reply(id_, result=None)
            log("shutdown requested — exiting")
            return 0
        elif method == "ping":
            reply(id_, result={"ok": True})
        else:
            reply(id_, error={"code": -32601, "message": f"method `{method}` not implemented"})
    return 0


if __name__ == "__main__":
    sys.exit(main())
