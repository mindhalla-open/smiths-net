# TURN server (slice 5.11-turn)

The engine ships an embedded RFC 8656 TURN server covering the
browser-compat subset: Allocate / Refresh / CreatePermission /
ChannelBind / Send & Data indications / ChannelData. Behind it
sits RFC 8489 long-term credentials (MD5(user:realm:pass) →
HMAC-SHA-1 `MESSAGE-INTEGRITY`).

## When to run the embedded server vs `coturn`

```
┌────────────────────────────────────────────────────────────────┐
│ Are your clients all on the same LAN as the engine?            │
│    ├── yes  → TURN disabled; host candidates are enough.       │
│    └── no   → TURN required                                    │
│         ├── Single-site deployment; engine lives on a public  │
│         │   IP with a fixed NAT mapping                        │
│         │    └── **Embedded TURN** — one config section, no    │
│         │       extra process, credentials rotate via          │
│         │       `[reload]`.                                    │
│         ├── Geo-distributed deployment (EU + US edges sharing │
│         │   a TURN fleet)                                      │
│         │    └── **coturn** — operate one TURN fleet, point   │
│         │       every engine's `external_url` at it.           │
│         └── Enterprise with strict NAT / corporate firewall    │
│              rules + ops team for coturn                       │
│              └── **coturn** — richer DOS mitigation, full RFC │
│                 6062 (TCP) support, mature operator playbook. │
└────────────────────────────────────────────────────────────────┘
```

## Embedded config

```toml
[webrtc.turn]
enabled               = true
bind                  = "0.0.0.0:3478"
realm                 = "turn.example.com"
relay_ip              = "203.0.113.1"           # public IP to hand clients
allocation_lifetime_s = 600                      # cap (RFC 8656 §3.2 default)
credentials           = [
  { username = "alice", password = "hunter2" },
  { username = "bob",   password = "deadbeef" },
]
# When set, the embedded server is skipped and clients are
# handed this URL on the signaling channel:
external_url          = ""
```

**The `realm` is part of the long-term key.** Changing the realm
invalidates every client's stored credential — rotate both in
lockstep (see below).

## NAT-traversal decision tree

```
Does the engine reach the open internet directly?
  ├── yes + clients are on public IPs
  │    └── No TURN needed. Host candidates suffice.
  ├── yes + clients behind NAT
  │    └── Embedded TURN on the engine's public interface.
  │       Set `relay_ip` to the public IP; clients use the
  │       server as a relay.
  └── no — engine is behind a corporate NAT itself
       └── Run a TURN server on a public-IP bastion. The engine
          hands clients the bastion's `external_url` + the
          bastion does the relay. (Future slice: engine-as-TURN-client
          for its own media path.)
```

## Credential rotation

Long-term credentials are hashed into the server's internal key
map at load time; the plaintext doesn't live past startup.
Rotating a password means:

```sh
# 1. Edit the credential list.
vim /etc/smiths/config.toml

[webrtc.turn]
credentials = [
  { username = "alice", password = "NEW_hunter3" },
  { username = "bob",   password = "deadbeef" },
]

# 2. Hot-reload (slice 5.8-c). The CLI read-through picks up
#    the new credential map on the next Allocate:
smiths-net reload --pid $(pgrep smiths-net) --config /etc/smiths/config.toml

# 3. Clients with the old password see `401 Unauthorized` on
#    their next Allocate; old allocations stay live for
#    `allocation_lifetime_s` — they're bound to the prior
#    credential that authorized them. Cutting those short means
#    a second reload with the credential removed entirely.
```

**Realm rotation** is the stricter cousin: because the realm
is hashed into the long-term key, changing realm + same
password yields a different key. Coordinate with whoever's
producing the `RTCIceServer` configuration on the client side
— they have to update both in the same deploy.

## Observability

- `smiths_turn_allocations_total{outcome="success"}` —
  Allocates that produced a relay.
- `smiths_turn_allocations_total{outcome="challenged"}` — 401
  challenge emitted (the normal first half of the auth dance).
  Expect roughly equal counts between `challenged` and
  `success` under steady-state load; a divergence means
  clients are giving up mid-dance.
- `smiths_turn_allocations_total{outcome="auth_failed"}` —
  real auth failures (wrong password or tampered
  `MESSAGE-INTEGRITY`). Should be near zero.
- `smiths_turn_allocations_total{outcome="forbidden"}` — 403
  on unsupported transport (client asked for TCP against a
  UDP-only server today).
- `smiths_turn_active_allocations` — live allocations.

## Limits

- **UDP only.** RFC 6062 (TCP transport) is future scope.
- **IPv6 relay.** Bind accepts IPv6 addresses but
  `XOR-RELAYED-ADDRESS` encoding has only been exercised for
  IPv4 in the integration test. IPv6 support is "probably
  works" not "proven."
- **No DOS mitigation beyond malformed-message drop.** A
  deployment expecting hostile load should front the embedded
  server with a firewall / CGNAT / coturn.
- **No `EVEN-PORT` / `DONT-FRAGMENT`.** Server parses them +
  ignores. Clients that require them need coturn.

## Cert rotation recipe

For `wss://` fronting: see `docs/deployment/webrtc.md` — the
TURN server is plaintext UDP; TLS rotation applies to the
WebSocket signaling path, not TURN.
