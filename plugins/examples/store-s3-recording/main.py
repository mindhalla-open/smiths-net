#!/usr/bin/env python3
"""S3-compatible `storage.recording` sidecar — slice 3.4 reference.

Delegates to the `aws` CLI so SigV4 signing + credential discovery
(env vars, IAM role, SSO) stays in one well-tested place. Works
against AWS S3 out of the box; MinIO / R2 / B2 need `--endpoint-url`
supplied via `S3_ENDPOINT_URL`.

Environment:
  S3_BUCKET         — required; blobs land at s3://$S3_BUCKET/$S3_PREFIX/<call_id>.wav
  S3_PREFIX         — default `recordings`
  S3_ENDPOINT_URL   — optional; set for non-AWS providers
  S3_REGION         — optional (falls back to AWS SDK's own defaults)
  AWS_CLI_BIN       — default `aws`

Stdlib-only; no `boto3` dep.
"""

from __future__ import annotations

import base64
import json
import os
import shutil
import subprocess
import sys
import tempfile
from datetime import datetime, timezone, timedelta

S3_BUCKET = os.environ.get("S3_BUCKET", "")
S3_PREFIX = os.environ.get("S3_PREFIX", "recordings").strip("/")
S3_ENDPOINT_URL = os.environ.get("S3_ENDPOINT_URL", "")
S3_REGION = os.environ.get("S3_REGION", "")
AWS_CLI_BIN = os.environ.get("AWS_CLI_BIN", "aws")

DESCRIPTOR = {
    "capability": "storage.recording",
    "plugin": "store-s3-recording",
    "model_id": f"s3://{S3_BUCKET}/{S3_PREFIX}" if S3_BUCKET else "s3://<unset>",
    "abi": "1.0",
    "description": "S3-compatible object-store recording sidecar via the `aws` CLI.",
    "priority": 20,
    "latency_ms": {"p50": 150, "p95": 800},
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
    sys.stderr.write(f"[store-s3-recording] {msg}\n")
    sys.stderr.flush()


def _require_cli() -> None:
    if not shutil.which(AWS_CLI_BIN):
        raise RuntimeError(f"`{AWS_CLI_BIN}` not on PATH")
    if not S3_BUCKET:
        raise RuntimeError("S3_BUCKET not set")


def _common_args() -> list[str]:
    out = []
    if S3_ENDPOINT_URL:
        out += ["--endpoint-url", S3_ENDPOINT_URL]
    if S3_REGION:
        out += ["--region", S3_REGION]
    return out


def _s3_uri(call_id: str) -> str:
    safe = call_id.replace("/", "_")
    return f"s3://{S3_BUCKET}/{S3_PREFIX}/{safe}.wav"


def _run_aws(argv: list[str], *, stdin: bytes | None = None) -> subprocess.CompletedProcess:
    cmd = [AWS_CLI_BIN, *argv, *_common_args()]
    return subprocess.run(
        cmd,
        input=stdin,
        capture_output=True,
        check=False,
    )


def put(params: dict) -> dict:
    _require_cli()
    call_id = params.get("call_id")
    b64 = params.get("audio_base64")
    if not isinstance(call_id, str) or not call_id:
        raise ValueError("`call_id` must be a non-empty string")
    if not isinstance(b64, str):
        raise ValueError("`audio_base64` must be a string")
    try:
        audio = base64.b64decode(b64)
    except Exception as e:
        raise ValueError(f"audio_base64 decode failed: {e}") from e
    # Stream via stdin so we don't pay a second tempfile.
    p = _run_aws(["s3", "cp", "-", _s3_uri(call_id)], stdin=audio)
    if p.returncode != 0:
        raise RuntimeError(
            f"aws s3 cp exited {p.returncode}: "
            f"{p.stderr.decode('utf-8', errors='replace')[:400]}"
        )
    return {"call_id": call_id, "uri": _s3_uri(call_id), "bytes": len(audio)}


def get(params: dict) -> dict:
    _require_cli()
    call_id = params.get("call_id")
    if not isinstance(call_id, str) or not call_id:
        raise ValueError("`call_id` must be a non-empty string")
    with tempfile.TemporaryDirectory() as d:
        local = os.path.join(d, "blob")
        p = _run_aws(["s3", "cp", _s3_uri(call_id), local])
        if p.returncode != 0:
            err = p.stderr.decode("utf-8", errors="replace")
            if "does not exist" in err.lower() or "not found" in err.lower():
                raise RuntimeError(f"not-found: {call_id}")
            raise RuntimeError(
                f"aws s3 cp exited {p.returncode}: {err[:400]}"
            )
        with open(local, "rb") as fh:
            audio = fh.read()
    return {
        "call_id": call_id,
        "audio_base64": base64.b64encode(audio).decode("ascii"),
        "bytes": len(audio),
    }


def delete(params: dict) -> dict:
    _require_cli()
    call_id = params.get("call_id")
    if not isinstance(call_id, str) or not call_id:
        raise ValueError("`call_id` must be a non-empty string")
    p = _run_aws(["s3", "rm", _s3_uri(call_id)])
    # S3 rm is idempotent — no error when the key was missing.
    ok = p.returncode == 0
    return {"call_id": call_id, "deleted": ok}


def list_recordings(_params: dict) -> dict:
    _require_cli()
    p = _run_aws(["s3", "ls", f"s3://{S3_BUCKET}/{S3_PREFIX}/", "--recursive"])
    if p.returncode != 0:
        raise RuntimeError(
            f"aws s3 ls exited {p.returncode}: "
            f"{p.stderr.decode('utf-8', errors='replace')[:400]}"
        )
    ids: list[str] = []
    for raw in p.stdout.decode("utf-8", errors="replace").splitlines():
        parts = raw.strip().split()
        if not parts:
            continue
        key = parts[-1]
        if not key.endswith(".wav"):
            continue
        base = os.path.basename(key)[: -len(".wav")]
        ids.append(base.replace("_", "/"))
    return {"call_ids": ids, "count": len(ids)}


def prune_older_than(params: dict) -> dict:
    _require_cli()
    max_age_secs = int(params.get("max_age_secs") or 0)
    if max_age_secs <= 0:
        raise ValueError("`max_age_secs` must be > 0")
    cutoff = datetime.now(timezone.utc) - timedelta(seconds=max_age_secs)
    # `aws s3 ls` prints `YYYY-MM-DD HH:MM:SS <size> <key>`; parse that.
    p = _run_aws(["s3", "ls", f"s3://{S3_BUCKET}/{S3_PREFIX}/", "--recursive"])
    if p.returncode != 0:
        raise RuntimeError(
            f"aws s3 ls exited {p.returncode}: "
            f"{p.stderr.decode('utf-8', errors='replace')[:400]}"
        )
    removed = 0
    for raw in p.stdout.decode("utf-8", errors="replace").splitlines():
        parts = raw.split(None, 3)
        if len(parts) < 4 or not parts[-1].endswith(".wav"):
            continue
        ts = f"{parts[0]} {parts[1]}"
        try:
            mtime = datetime.strptime(ts, "%Y-%m-%d %H:%M:%S").replace(tzinfo=timezone.utc)
        except ValueError:
            continue
        if mtime >= cutoff:
            continue
        rm = _run_aws(["s3", "rm", f"s3://{S3_BUCKET}/{parts[-1]}"])
        if rm.returncode == 0:
            removed += 1
    return {"removed": removed}


def main() -> int:
    log(
        f"ready (bucket={S3_BUCKET or '<unset>'} prefix={S3_PREFIX} "
        f"endpoint={S3_ENDPOINT_URL or '<default>'})"
    )
    dispatch = {
        "put": put,
        "get": get,
        "delete": delete,
        "list": list_recordings,
        "prune_older_than": prune_older_than,
    }
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
        elif method in dispatch:
            try:
                reply(id_, result=dispatch[method](params))
            except ValueError as e:
                reply(id_, error={"code": -32602, "message": str(e)})
            except Exception as e:  # noqa: BLE001
                reply(id_, error={"code": -32603, "message": f"{method} failed: {e}"})
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
