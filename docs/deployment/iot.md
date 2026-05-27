# IoT bridges

Slice 4.5 / P21. Two reference sidecars land in this release —
`ha-bridge` (Home Assistant) and `mqtt-bridge` (generic MQTT
3.1.1) — plus the documented event-mapping patterns tying SIP
lifecycle to external automation. Both advertise the new
`bridge.*` capability namespace alongside the existing `ai.*`,
`media.*`, `storage.*`, and `routing.*` seams.

## Event flow shapes

Two directions matter:

```
         ┌───────────── external world ─────────────┐
         │                                           │
 doorbell press (HA)   ───►   smiths-net  ──►  SIP call (make_call)
         │                                           │
 call terminated (SIP) ───►   smiths-net  ──►  MQTT publish
         │                                           │
         └───────────────────────────────────────────┘
```

- **Inbound (IoT → smiths-net):** an external automation pokes
  the engine via the webhook adapter (slice 4.4 / P20) or the
  A2A JSON-RPC endpoint. The bridge sidecars don't participate
  on this leg — the webhook adapter speaks directly to the tool
  layer.
- **Outbound (smiths-net → IoT):** an agent subscribes to MCP
  notifications (`notifications/call/created`,
  `notifications/call/terminated`, …) and calls a bridge's
  provider method (`emit_event`, `publish`, `call_service`) to
  forward the signal into the external system.

Everything else in this doc is either a variation on those two
shapes or an operational recipe.

## Demo — smart doorbell triggers a SIP call

Goal: the doorbell button on a Home Assistant-managed device
rings Alice's SIP phone. Prereqs:

- smiths-net with the webhook adapter bound. For embedders today
  that means calling `smiths_mcp::webhook::serve_http` from the
  hosting binary; a CLI-wired `[webhook]` config lands in a
  follow-on.
- Home Assistant with a long-lived access token.

### 1. Generate a smiths-net webhook token

```bash
export SMITHS_WEBHOOK_TOKEN=$(openssl rand -hex 24)
# Pass this to serve_http's `bearer_token` arg.
```

### 2. Home Assistant configuration

```yaml
# configuration.yaml — minimal rest_command + automation
rest_command:
  smiths_call:
    url: "http://smiths-net.internal:7879/hook/make_call"
    method: POST
    headers:
      Authorization: "Bearer !secret smiths_webhook_token"
      Content-Type: "application/json"
    payload: '{"target": "{{ target }}"}'

automation:
  - alias: "Front doorbell → ring Alice"
    trigger:
      - platform: state
        entity_id: binary_sensor.front_doorbell
        to: "on"
    action:
      - service: rest_command.smiths_call
        data:
          target: "sip:alice@10.0.0.2:5060"
```

Put `smiths_webhook_token: <token>` in `secrets.yaml`.

### 3. Verify end-to-end

Press the doorbell; `smiths-net` logs:

```text
webhook tool invocation: make_call target=sip:alice@10.0.0.2:5060 actor=webhook-http outcome=ok
```

Fault the path by disconnecting the SIP UAC: the webhook now
returns `404 {"error": "no outbound-call originator configured"}`
and Home Assistant's `rest_command` surfaces the same error in
the automation trace.

## Demo — publish `call.ended` with duration

Goal: every time a dialog terminates, publish a JSON payload to
`smiths/call/ended/<call_id>` for downstream observability
(Grafana Loki via mqtt2loki, an in-house billing consumer, …).

### 1. Broker + sidecar

```bash
docker run -it --rm -p 1883:1883 eclipse-mosquitto
export MQTT_HOST=127.0.0.1
export MQTT_PORT=1883
# mqtt-bridge picks these up on load.
```

### 2. Agent wiring

The agent subscribes to MCP's `notifications/call/terminated`
and, for each event, issues an `ai_invoke` into `mqtt-bridge`.
The CDR store (slice 2.3) holds the duration, so agents that
want richer payloads issue `list_cdr` with the matching
`call_id` and include the duration on the published payload.

Python agent sketch:

```python
def on_call_terminated(call_id: str):
    cdr = mcp.call_tool("list_cdr", {"limit": 1,
                                     "from_like": call_id})
    row = cdr["structuredContent"]["rows"][0] if cdr else None
    payload = {"call_id": call_id, "ended_at": int(time.time())}
    if row:
        payload["duration_secs"] = row["duration_secs"]
        payload["result"] = row["result"]
    mcp.call_tool("ai_invoke", {
        "plugin": "mqtt-bridge",
        "method": "publish",
        "params": {
            "topic": f"smiths/call/ended/{call_id}",
            "payload": payload,
            "qos": 1,
        },
    })
```

### 3. Validate

```bash
mosquitto_sub -h 127.0.0.1 -t 'smiths/call/ended/#' -v
# smiths/call/ended/abc@smiths.local {"call_id":"abc@smiths.local","ended_at":...,"duration_secs":42,"result":"answered"}
```

## Event-mapping patterns

| Engine event                          | Typical outbound action                                           |
|---------------------------------------|-------------------------------------------------------------------|
| `notifications/call/created`          | HA: turn on hallway light; MQTT: `smiths/call/started`            |
| `notifications/call/terminated`       | HA: turn off hallway light; MQTT: `smiths/call/ended` + duration  |
| `notifications/plugin/ai.llm.partial` | Push text into a Matrix room / Slack thread for live transcripts  |
| `notifications/plugin/emit_partial`   | ASR partials → HA `input_text.live_transcript` for UI display     |

These are patterns, not hard-coded behaviours. The engine ships
the primitives; operators wire them into their specific flows.

## Security

- **Webhook bearer token.** Always set one on production
  webhooks. Without it, anyone who reaches the webhook port can
  invoke every MCP tool — including `make_call` to arbitrary
  SIP URIs.
- **MQTT auth.** The broker enforces ACLs; the bridge just
  publishes. For AWS IoT Core, use per-device certificates +
  topic ACLs; for a shared Mosquitto, at minimum set
  `MQTT_USERNAME` + `MQTT_PASSWORD` on the sidecar.
- **HA token rotation.** Rotate long-lived access tokens on
  operator departure + at a fixed cadence (monthly / quarterly
  per your policy). The bridge reads the env var on spawn, so
  rotation = restart.
- **Egress exposure.** Both bridges initiate outbound
  connections. If the engine's host has egress restrictions,
  carve out the HA + MQTT endpoints explicitly rather than
  blanket-allowing all outbound; a compromised sidecar
  otherwise becomes an exfil tool.

## Limits + follow-ons

- **No WebSocket subscriptions on `ha-bridge`.** HA exposes an
  event-stream over a WebSocket but the plugin protocol doesn't
  yet model push subscriptions. For now, HA → smiths-net flows
  through the webhook adapter (HA's `rest_command` fires into
  `smiths-net`); the bridge covers the other direction.
- **MQTT 3.1.1 only.** No MQTT 5 property bags. The bridge
  covers 90% of telemetry-publish flows; operators who need the
  5 feature set run a proper client.
- **TLS / mTLS for MQTT.** Sidecar uses a plain TCP socket.
  `stunnel` in front of the broker is the minimum for prod.
- **No in-tree Matrix / Slack / PagerDuty bridge.** The
  `bridge.*` namespace is open — write one following the
  `mqtt-bridge` shape and it'll register alongside `ha-bridge`
  with no engine changes.
