# mqtt-bridge

Reference `bridge.mqtt` sidecar — publishes to any MQTT 3.1.1
broker (Mosquitto, HiveMQ, EMQX, AWS IoT Core). Stdlib-only — no
`paho-mqtt` / `asyncio-mqtt` dep. Ships its own minimal CONNECT /
PUBLISH / DISCONNECT encoder in ~100 LOC.

## Setup

```bash
# 1. Start a local broker (Docker is the fastest path):
docker run -it --rm -p 1883:1883 eclipse-mosquitto

# 2. Point the sidecar at it.
export MQTT_HOST=127.0.0.1
export MQTT_PORT=1883
# Optional:
# export MQTT_USERNAME=smiths
# export MQTT_PASSWORD=s3cret
# export MQTT_CLIENT_ID=smiths-net-prod-01
# export MQTT_TIMEOUT_SECS=5
```

Drop this directory under `plugins.dir` and start smiths-net.
`list_ai_providers` shows `mqtt-bridge`.

## Method

Only one method today: `publish(topic, payload, qos?, retain?)`.

| field   | type                              | notes                              |
|---------|-----------------------------------|------------------------------------|
| topic   | `string`                          | required                           |
| payload | `string \| object \| array \| null` | objects/arrays are JSON-encoded  |
| qos     | `0 \| 1`                          | default `0`; QoS 2 not supported   |
| retain  | `bool`                            | default `false`                    |

Response:

```json
{"topic": "smiths/call/ended", "qos": 1, "retain": false,
 "bytes": 83, "elapsed_ms": 14}
```

## Pattern — publish `call.ended` with duration

Agent side:

```python
# 1. subscribe to MCP notifications
on("notifications/call/terminated", lambda params: mcp.call_tool(
    "ai_invoke",
    {
        "plugin": "mqtt-bridge",
        "method": "publish",
        "params": {
            "topic": f"smiths/call/ended/{params['call_id']}",
            "payload": {
                "call_id": params["call_id"],
                "ended_at": time.time(),
                # Duration comes from the CDR the engine wrote on
                # BYE — query via `list_cdr` if the agent needs it
                # surfaced on the MQTT payload.
            },
            "qos": 1,
        },
    },
))
```

Subscribers on `smiths/call/ended/+` get one message per call end
with at-least-once delivery (QoS 1).

## Design notes

* **Short-lived connections.** Each `publish` opens a TCP socket,
  sends CONNECT → PUBLISH → DISCONNECT, closes. Adds ~1 RTT per
  call vs keeping a persistent connection, but avoids having to
  run a reconnect / keepalive loop across the JSON-RPC boundary.
  Fine for event-rate traffic (one publish per call-lifecycle
  transition); wrong for high-frequency telemetry.
* **QoS 2 not supported.** Needs four-packet PUBREC/PUBREL/PUBCOMP
  handshake + persistent publish-id state across reconnects.
  Operators who genuinely need exactly-once MQTT should use the
  broker's QoS-2 semantics from a dedicated client, not a
  short-lived sidecar.
* **TLS.** No TLS today. Deployments that need mTLS / 8883 should
  run `stunnel` or `mosquitto-auth-plug` as a sidecar, or swap
  `socket.create_connection` for `ssl.create_default_context`
  locally.

## Priority

Advertises `priority = 30`. No competing in-tree `bridge.mqtt`
provider today; the field reserves room for an AWS-IoT-Core
variant (which would want a lower priority number so the
dispatcher picks it first when both are loaded).
