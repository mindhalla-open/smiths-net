# ha-bridge

Reference `bridge.ha` sidecar — Home Assistant via the REST API.
Stdlib-only Python; no `homeassistant_api` / `aiohttp` deps.

## Setup

```bash
# 1. Generate a long-lived access token in Home Assistant:
#    Profile → Security → Long-Lived Access Tokens → Create Token

export HA_BASE_URL=http://homeassistant.local:8123
export HA_TOKEN=eyJ...your-token-here
```

Drop this directory under `plugins.dir` and start smiths-net.
`list_ai_providers` shows `ha-bridge`.

## Methods

| method         | params                           | notes                                           |
|----------------|----------------------------------|-------------------------------------------------|
| `emit_event`   | `{event_type, data}`             | POST `/api/events/<event_type>`                 |
| `get_state`    | `{entity_id}`                    | GET `/api/states/<entity_id>`                   |
| `call_service` | `{domain, service, data}`        | POST `/api/services/<domain>/<service>`         |

## Patterns

### 1. Agent → HA (notify on call)

Agent subscribes to MCP's `notifications/call/created`, then invokes
the bridge to fire a `sip_call` event HA automations can match:

```json
{"method": "ai_invoke", "params": {
  "plugin": "ha-bridge",
  "method": "emit_event",
  "params": {
    "event_type": "sip_call",
    "data": {"call_id": "abc@smiths.local", "from": "sip:doorbell@..." }
  }
}}
```

### 2. HA → Agent (doorbell triggers call)

HA automation hits the engine's webhook adapter (slice 4.4) when
the doorbell button pushes:

```yaml
# configuration.yaml / automation.yaml
automation:
  - alias: "Doorbell → SIP"
    trigger:
      - platform: state
        entity_id: binary_sensor.front_doorbell
        to: "on"
    action:
      - service: rest_command.smiths_call
        data:
          target: "sip:alice@10.0.0.2:5060"

rest_command:
  smiths_call:
    url: "http://smiths-net.internal:7879/hook/make_call"
    method: POST
    headers:
      Authorization: "Bearer {{ states('input_text.smiths_token') }}"
    payload: '{"target": "{{ target }}"}'
    content_type: "application/json"
```

## Priority

Advertises `priority = 30`. There's no routing decision to make
on the `bridge.ha` capability (one HA instance per deployment), so
priority mostly matters when operators run more than one bridge
variant (HomeKit, SmartThings) and pick one as primary.

## Limits

* No WebSocket / event-stream subscription — that would need a
  persistent connection + push shape the plugin protocol doesn't
  currently expose. Today the flow is **agent-driven polling**:
  the agent watches MCP notifications, then decides to call HA.
* HA's REST `/api/events/<event_type>` endpoint was deprecated in
  2023.4; on recent HA versions it's still available but emits a
  warning. Use `call_service` for actuating devices; events are
  the escape hatch for cases where you want the HA event bus.
