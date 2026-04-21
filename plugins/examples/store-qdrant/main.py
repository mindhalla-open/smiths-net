#!/usr/bin/env python3
"""Qdrant-backed `storage.vector` sidecar — slice 3.4 reference.

Exposes the engine's vector-store contract over JSON-RPC:

  * `upsert(record)`  — upsert {id, vector, metadata}
  * `search(query, k)` — cosine top-k
  * `delete(id)`       — remove one point
  * `count()`          — total points in the collection

On first contact the sidecar ensures the Qdrant collection exists
with the configured vector size + distance; operators who want a
non-default size should set `QDRANT_VECTOR_SIZE` before boot.

Environment overrides:
  QDRANT_URL          — default `http://127.0.0.1:6333`
  QDRANT_COLLECTION   — default `smiths-calls`
  QDRANT_VECTOR_SIZE  — default 384 (common for all-MiniLM models)
  QDRANT_DISTANCE     — default `Cosine` (`Dot` / `Euclid` / `Manhattan`)
  QDRANT_API_KEY      — optional bearer-token header

Stdlib-only; no `qdrant-client` dep.
"""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request

QDRANT_URL = os.environ.get("QDRANT_URL", "http://127.0.0.1:6333").rstrip("/")
QDRANT_COLLECTION = os.environ.get("QDRANT_COLLECTION", "smiths-calls")
QDRANT_VECTOR_SIZE = int(os.environ.get("QDRANT_VECTOR_SIZE", "384"))
QDRANT_DISTANCE = os.environ.get("QDRANT_DISTANCE", "Cosine")
QDRANT_API_KEY = os.environ.get("QDRANT_API_KEY", "")

DESCRIPTOR = {
    "capability": "storage.vector",
    "plugin": "store-qdrant",
    "model_id": QDRANT_COLLECTION,
    "abi": "1.0",
    "description": f"Qdrant vector store at {QDRANT_URL} / collection `{QDRANT_COLLECTION}`.",
    "priority": 20,
    "vector_size": QDRANT_VECTOR_SIZE,
    "distance": QDRANT_DISTANCE,
    "latency_ms": {"p50": 30, "p95": 200},
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
    sys.stderr.write(f"[store-qdrant] {msg}\n")
    sys.stderr.flush()


def _headers() -> dict:
    h = {"Content-Type": "application/json"}
    if QDRANT_API_KEY:
        h["api-key"] = QDRANT_API_KEY
    return h


def _request(method: str, path: str, body: dict | None = None) -> dict:
    url = f"{QDRANT_URL}{path}"
    data = json.dumps(body).encode("utf-8") if body is not None else None
    req = urllib.request.Request(url, data=data, headers=_headers(), method=method)
    try:
        with urllib.request.urlopen(req, timeout=15) as resp:
            raw = resp.read()
            return json.loads(raw.decode("utf-8")) if raw else {}
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace") if hasattr(e, "read") else ""
        raise RuntimeError(f"qdrant HTTP {e.code}: {body[:400]}") from e


def _ensure_collection() -> None:
    # GET /collections/{name} → 200 if exists, 404 otherwise.
    try:
        _request("GET", f"/collections/{QDRANT_COLLECTION}")
        return
    except RuntimeError as e:
        if "HTTP 404" not in str(e):
            raise
    _request(
        "PUT",
        f"/collections/{QDRANT_COLLECTION}",
        {
            "vectors": {"size": QDRANT_VECTOR_SIZE, "distance": QDRANT_DISTANCE},
        },
    )
    log(f"created collection `{QDRANT_COLLECTION}` (size={QDRANT_VECTOR_SIZE}, distance={QDRANT_DISTANCE})")


def upsert(params: dict) -> dict:
    record = params.get("record") or params  # accept either shape
    point_id = record.get("id")
    vector = record.get("vector")
    metadata = record.get("metadata") or {}
    if not isinstance(point_id, str) or not point_id:
        raise ValueError("record.id must be a non-empty string")
    if not isinstance(vector, list) or not vector:
        raise ValueError("record.vector must be a non-empty array")
    _ensure_collection()
    # Qdrant wants an integer or UUID id; hash the string to a 63-bit int
    # if the caller gave us a free-form id. Preserving the original in
    # payload so callers can read it back on search hits.
    import hashlib
    pid = int(hashlib.sha1(point_id.encode("utf-8")).hexdigest()[:15], 16)
    payload = dict(metadata)
    payload["_smiths_id"] = point_id
    _request(
        "PUT",
        f"/collections/{QDRANT_COLLECTION}/points",
        {"points": [{"id": pid, "vector": vector, "payload": payload}]},
    )
    return {"id": point_id, "point_id": pid}


def search(params: dict) -> dict:
    query = params.get("query") or params.get("vector")
    k = int(params.get("k") or params.get("limit") or 5)
    if not isinstance(query, list) or not query:
        raise ValueError("`query`/`vector` must be a non-empty array")
    _ensure_collection()
    resp = _request(
        "POST",
        f"/collections/{QDRANT_COLLECTION}/points/search",
        {"vector": query, "limit": k, "with_payload": True},
    )
    hits = []
    for r in resp.get("result") or []:
        payload = r.get("payload") or {}
        hits.append({
            "id": payload.pop("_smiths_id", str(r.get("id"))),
            "score": r.get("score"),
            "metadata": payload,
        })
    return {"hits": hits}


def delete(params: dict) -> dict:
    point_id = params.get("id")
    if not isinstance(point_id, str) or not point_id:
        raise ValueError("`id` must be a non-empty string")
    import hashlib
    pid = int(hashlib.sha1(point_id.encode("utf-8")).hexdigest()[:15], 16)
    _request(
        "POST",
        f"/collections/{QDRANT_COLLECTION}/points/delete",
        {"points": [pid]},
    )
    return {"deleted": True}


def count(_params: dict) -> dict:
    resp = _request("POST", f"/collections/{QDRANT_COLLECTION}/points/count", {"exact": True})
    return {"count": (resp.get("result") or {}).get("count", 0)}


def main() -> int:
    log(f"ready (url={QDRANT_URL} collection={QDRANT_COLLECTION})")
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
        elif method == "upsert":
            try:
                reply(id_, result=upsert(params))
            except ValueError as e:
                reply(id_, error={"code": -32602, "message": str(e)})
            except Exception as e:  # noqa: BLE001
                reply(id_, error={"code": -32603, "message": f"upsert failed: {e}"})
        elif method == "search":
            try:
                reply(id_, result=search(params))
            except ValueError as e:
                reply(id_, error={"code": -32602, "message": str(e)})
            except Exception as e:  # noqa: BLE001
                reply(id_, error={"code": -32603, "message": f"search failed: {e}"})
        elif method == "delete":
            try:
                reply(id_, result=delete(params))
            except ValueError as e:
                reply(id_, error={"code": -32602, "message": str(e)})
            except Exception as e:  # noqa: BLE001
                reply(id_, error={"code": -32603, "message": f"delete failed: {e}"})
        elif method == "count":
            try:
                reply(id_, result=count(params))
            except Exception as e:  # noqa: BLE001
                reply(id_, error={"code": -32603, "message": f"count failed: {e}"})
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
