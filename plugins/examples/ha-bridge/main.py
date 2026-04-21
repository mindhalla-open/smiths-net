#!/usr/bin/env python3
"""Home Assistant bridge sidecar — slice 4.5 / P21.

Talks to the Home Assistant REST API via a long-lived access
token. Three methods:

    emit_event(event_type, data)
        POST /api/events/<event_type> → fires a custom HA event
        that HA automations can match on.

    get_state(entity_id)
        GET /api/states/<entity_id> → returns the current state
        dict (state, attributes, last_changed).

    call_service(domain, service, data)
        POST /api/services/<domain>/<service> → triggers a service
        (turn on a light, play a media_player, notify a device).

Bidirectional use cases:

 * **IN**: Home Assistant triggers an automation that POSTs the
   doorbell event to smiths-net's webhook adapter (slice 4.4),
   which in turn calls `make_call`. The bridge isn't involved on
   the inbound leg.
 * **OUT**: agent code watches the MCP call-lifecycle
   notifications and calls this plugin's `emit_event("sip_call",
   { "call_id": ..., "from": ... })` so HA can drive side-effects
   (turn on the doorbell-hallway light on `DialogCreated`).

Environment:
  HA_BASE_URL      — required, e.g. `http://homeassistant.local:8123`
  HA_TOKEN         — required, long-lived access token
  HA_TIMEOUT_SECS  — default 10

Stdlib-only.
"""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.request

HA_BASE_URL = os.environ.get("HA_BASE_URL", "").rstrip("/")
HA_TOKEN = os.environ.get("HA_TOKEN", "")
HA_TIMEOUT = float(os.environ.get("HA_TIMEOUT_SECS", "10"))

DESCRIPTOR = {
    "capability": "bridge.ha",
    "plugin": "ha-bridge",
    "model_id": "ha-rest-v1",
    "abi": "1.0",
    "description": f"Home Assistant bridge at {HA_BASE_URL or '<unset>'}.",
    "priority": 30,
    "latency_ms": {"p50": 50, "p95": 400},
    "concurrency": {"max_in_flight": 4},
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
    sys.stderr.write(f"[ha-bridge] {msg}\n")
    sys.stderr.flush()


def _headers() -> dict:
    if not HA_TOKEN:
        raise RuntimeError("HA_TOKEN not set")
    if not HA_BASE_URL:
        raise RuntimeError("HA_BASE_URL not set")
    return {
        "Content-Type": "application/json",
        "Authorization": f"Bearer {HA_TOKEN}",
    }


def _request(method: str, path: str, body: dict | None = None) -> dict | list | None:
    url = f"{HA_BASE_URL}{path}"
    data = json.dumps(body).encode("utf-8") if body is not None else None
    req = urllib.request.Request(url, data=data, headers=_headers(), method=method)
    try:
        with urllib.request.urlopen(req, timeout=HA_TIMEOUT) as resp:
            raw = resp.read()
            if not raw:
                return None
            return json.loads(raw.decode("utf-8"))
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace") if hasattr(e, "read") else ""
        raise RuntimeError(f"home-assistant HTTP {e.code}: {body[:400]}") from e


def emit_event(params: dict) -> dict:
    event_type = params.get("event_type")
    if not isinstance(event_type, str) or not event_type:
        raise ValueError("`event_type` must be a non-empty string")
    data = params.get("data") or {}
    if not isinstance(data, dict):
        raise ValueError("`data` must be an object")
    resp = _request("POST", f"/api/events/{event_type}", data)
    return {"fired": True, "event_type": event_type, "response": resp}


def get_state(params: dict) -> dict:
    entity_id = params.get("entity_id")
    if not isinstance(entity_id, str) or not entity_id:
        raise ValueError("`entity_id` must be a non-empty string")
    resp = _request("GET", f"/api/states/{entity_id}")
    return resp or {}


def call_service(params: dict) -> dict:
    domain = params.get("domain")
    service = params.get("service")
    if not isinstance(domain, str) or not domain:
        raise ValueError("`domain` must be a non-empty string")
    if not isinstance(service, str) or not service:
        raise ValueError("`service` must be a non-empty string")
    data = params.get("data") or {}
    resp = _request("POST", f"/api/services/{domain}/{service}", data)
    return {"called": True, "domain": domain, "service": service, "response": resp}


def main() -> int:
    log(f"ready (base={HA_BASE_URL or '<unset>'} token={'set' if HA_TOKEN else 'unset'})")
    dispatch = {
        "emit_event": emit_event,
        "get_state": get_state,
        "call_service": call_service,
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
