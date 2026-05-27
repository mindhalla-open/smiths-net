# WebRTC-native signaling (slice 5.10-followup)

Browsers that only speak WebRTC can talk to the engine over a
plain WebSocket without implementing any SIP. The JSON frame
shape is the same `WtSignal` format the WebTransport demo uses
(slice 5.7); only the transport differs.

## Runtime status (v0.67.0)

Slice 5.10-followup ships the **CLI adapter** wiring an axum
WebSocket route at `/smiths/webrtc` + a concrete
`WebRtcSessionHandler` that routes offers through the shared
`SdpNegotiator`. The signaling layer is complete and the browser
demo round-trips real frames against the engine.

The DTLS-SRTP terminator + tag-based rendezvous bridge (slices
5.10-dtls / 5.10-bridge) land in this release — browsers that
offer `UDP/TLS/RTP/SAVP` now receive a real answer carrying the
engine's fingerprint, complete the handshake, and pair with a
SIP leg (or another WebRTC leg) sharing the same `tag`. See the
"DTLS-SRTP" + "Tag-based rendezvous" sections below for the
ops-facing details.

One piece still explicitly surfaces a clean error rather than a
silent failure:

- **ICE / TURN.** The DTLS handshake trusts the peer address
  in the offer's `c=` / `m=` block — fine for same-subnet
  deployments + the browser demo, but real NATs need ICE.
  Slice 5.10-ice / 5.11-turn fill this gap; until then browsers
  behind symmetric NAT either need a host-network setup or
  the operator fronts the engine with an external TURN (see
  `docs/deployment/turn.md` when that lands).

What works end-to-end today:

- `session-init` / `session-ack` lifecycle (optional `tag`).
- `offer` frames carrying **SIP-style `RTP/AVP` SDP** — used
  by SIP-over-WebSocket-style callers that ride this adapter
  without DTLS.
- `offer` frames carrying **WebRTC-style `UDP/TLS/RTP/SAVP[F]`
  SDP** — accepted when the engine has a DTLS cert configured
  (auto-minted at boot); the answer carries `a=fingerprint` +
  `a=setup` flipped to the reverse role.
- `ice-candidate` / `ice-end` acked silently (handler default
  is a no-op; ICE pairing lands in a follow-on).
- `echo` mirrored back.
- `bye` closes the session.

## DTLS-SRTP

The engine mints a fresh ECDSA P-256 self-signed cert at boot
(via `smiths_core::SelfSignedCert::generate`). The cert is held
for the process lifetime and its SHA-256 fingerprint is what
every answer carries. One cert per engine instance — regenerate
the engine on a restart to rotate.

### Role negotiation

Per RFC 5763 §5 the answer's `a=setup:` is the complement of the
offer's:

| Offer's `a=setup:` | Answer's `a=setup:` | Engine role          |
|--------------------|---------------------|----------------------|
| `actpass` (common) | `active`            | DTLS client (sends `ClientHello`) |
| `passive`          | `active`            | DTLS client          |
| `active`           | `passive`           | DTLS server          |
| missing            | `active`            | DTLS client (treated as `actpass`) |

### Handshake-timeout tuning

The handshake inherits `webrtc-dtls` 0.12's retransmit defaults
(1 s initial, exponential backoff, fail after ~30 s). Under
live browser traffic against a nearby STUN/TURN proxy you should
see completions in 50–250 ms; anything over a few seconds points
at packet loss or a NAT hairpin issue (STUN isn't wired yet —
every datagram has to take the direct path named in the offer's
`c=` line).

- **Watch**: `smiths_webrtc_dtls_handshakes_total{outcome="success"}`
  vs the sum of every other outcome bucket. A rising
  `fingerprint_mismatch` slope means peers are advertising certs
  that don't match their handshake cert — often a sign of a
  MITM box between the browser and the engine, or a stale
  offer being replayed.
- **Fingerprint-algorithm rejection**: only `sha-256` is
  accepted today. Ancient clients sending `sha-1` land on
  `{outcome="unsupported_algorithm"}`; the fix is on the
  client — the engine won't accept a downgrade.
- **Probe latency**: `smiths_webrtc_dtls_handshakes_total`'s
  rate is the proxy signal today. A dedicated histogram
  (`smiths_webrtc_dtls_handshake_duration_seconds`) is a
  follow-on.

No operator-facing timeout knob is exposed yet. If real-world
browsers start failing on the default, we'll light up a
`[webrtc.dtls] handshake_timeout_s` field in a dedicated slice;
today's default is the same ~30 s webrtc-dtls ships with, which
matches how browsers themselves behave.

## Tag-based rendezvous

A WebRTC leg and a SIP leg pair up via a shared `tag` string.
The first `session-init` with tag `X` parks its endpoint + DTLS
context in the engine's `PendingLegs` map; the second leg (SIP
INVITE or another WebRTC session) with tag `X` pulls the
partner's endpoint and installs the bridge. Both sides then
exchange RTP over the engine's media fabric.

### Deadline

Unpaired legs are evicted after **30 seconds** by default. The
engine logs `webrtc rendezvous leg evicted; no partner arrived
before deadline` and sends a `bye` to the orphaned session so
the browser UX doesn't hang.

### Metrics

- `smiths_webrtc_sessions_paired_total{partner="sip"}` — one
  increment per successful pairing where the other leg came
  from a SIP INVITE.
- `smiths_webrtc_sessions_paired_total{partner="webrtc"}` —
  two WebRTC legs pairing (Alice calls Bob, both via the same
  engine).
- `smiths_webrtc_sessions_paired_total{partner="none"}` —
  reserved for deadline evictions; bumped so dashboards can
  alert on a rising slope of orphaned legs (usually a sign of
  a broken client flow — one side finishing signaling, the
  other never showing up).

## Config

```toml
[webrtc]
enabled  = true                 # default false
ws_bind  = "127.0.0.1:7881"     # plaintext HTTP/WebSocket
tls_cert = ""                   # reserved; see "TLS" below
tls_key  = ""                   # reserved; see "TLS" below

[webrtc.privacy]
mode          = "open"          # slice 5.11 (scaffold today)
redaction_key = ""
```

The adapter is off by default. When `enabled = true` the CLI
opens a TCP listener on `ws_bind`, accepts WebSocket upgrades at
`GET /smiths/webrtc`, and streams binary frames through the
signaling state machine. Any other route returns 404.

## TLS — front the engine

`webrtc.tls_cert` / `webrtc.tls_key` are accepted by the config
parser but the adapter binds plaintext today; setting them logs
a warning at boot. For production `wss://`, front the engine
with a TLS-terminating reverse proxy that forwards to the
plaintext port:

```nginx
server {
    listen 443 ssl http2;
    server_name rtc.example.com;
    ssl_certificate     /etc/letsencrypt/live/rtc.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/rtc.example.com/privkey.pem;

    location /smiths/webrtc {
        proxy_pass http://127.0.0.1:7881;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_read_timeout 3600s;
    }
}
```

Same recipe works for Caddy (one `reverse_proxy` directive) and
Envoy. This mirrors the guidance for the MCP HTTP adapter in
`docs/deployment/` — one pattern across adapters.

## ICE (slice 5.10-ice)

The engine's ICE surface is **ICE-Lite**: it trusts the peer's
candidate selection, doesn't perform controlling / controlled
role agent logic, and advertises a single host candidate
derived from the media-fabric endpoint the SDP answer publishes.

```toml
[webrtc.ice]
enabled       = true                  # default false
host_binds    = []                    # extra binds to advertise
stun_servers  = []                    # future: srflx gathering
```

When `enabled = true`, every DTLS-SRTP answer carries:

- `a=ice-ufrag:` — fresh 8-char ICE-char token per answer
- `a=ice-pwd:` — fresh 24-char ICE-char token per answer
- `a=ice-options:trickle` — advertised for browser compat
- `a=candidate:...` — one `host` candidate for
  `(local_ip, allocated_port)`
- `a=end-of-candidates` — in-band list is complete

Trickle candidates posted by the client on the signaling
channel are parsed + logged; the engine's DTLS handshake
trusts whatever source address actually reaches its socket,
so adding more peer candidates is diagnostic rather than
load-bearing in the ICE-Lite posture.

**Limits today:**

- No role-swap: the engine is always the "passive" ICE side
  (ICE-Lite). Fine for a server-mediated deployment;
  peer-to-peer ICE needs a full agent (future slice).
- No srflx gathering: `stun_servers` is a scaffold field.
  The existing STUN implementation in `smiths-ice::stun`
  client-side only does connectivity checks, not
  `XOR-MAPPED-ADDRESS` gathering from an external server.
- No retransmit loop: a single `binding_ping` runs; RFC 8445
  T1-doubling retransmits are future scope.

## TURN — embedded or external (slice 5.11-turn)

```toml
[webrtc.turn]
enabled               = true           # default false
bind                  = "0.0.0.0:3478" # standard TURN port
realm                 = "turn.example.com"
relay_ip              = "203.0.113.1"  # public IP to hand back
allocation_lifetime_s = 600
credentials           = [{ username = "alice", password = "hunter2" }]
external_url          = ""             # when set: skip embedded, hand URL to clients
```

See `docs/deployment/turn.md` for the full picture: when to
run the embedded server vs coturn, the NAT-traversal decision
tree, and the LongTermCredential cert-rotation recipe.

## CORS

The WebSocket adapter doesn't emit CORS headers because a
WebSocket handshake isn't subject to the Fetch CORS spec in
practice — browsers perform the upgrade as a separate protocol.
If you front the engine with a reverse proxy and serve the
browser page from a different origin, you're fine.

## WebRTC vs SIP-over-WebSocket

Pick this adapter when:

- Clients are browsers using `RTCPeerConnection` directly.
- Operators want signaling traffic over a single port (443
  after TLS termination) without SIP-WS-specific middleboxes.
- You prefer a JSON wire format to SIP parsing in the client.

Pick SIP-over-WebSocket (the existing `ws://` SIP transport)
when:

- Clients are existing SIP softphones that happen to speak
  WebSocket (jsSIP, SIP.js).
- You want SIP's dialog semantics to surface directly at the
  client — REFER, replaces, subscribe/notify — without bridging.

The two adapters coexist fine; both can be enabled on the same
engine instance with different ports.

## Demo

`examples/browser-webrtc/` ships a static page that exercises
every frame type. See its README for the run recipe.

## Troubleshooting

| Symptom                                                           | Explanation                                                                                                        |
|-------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------|
| Browser receives `offer-rejected: DTLS-SRTP transport offered but engine has no cert configured` | The CLI didn't mint a cert at boot — check the startup log for `SelfSignedCert::generate` errors. |
| Browser receives `offer-rejected: DTLS-SRTP offer missing a=fingerprint` | Offer is malformed per RFC 5763 §5.3; the browser's `RTCPeerConnection` didn't include its cert fingerprint. Refresh the page and try again — if it persists, something is intercepting SDP. |
| `smiths_webrtc_dtls_handshakes_total{outcome="fingerprint_mismatch"}` ticks up | Peer's cert didn't hash to the SDP-advertised fingerprint. MITM or a stale offer being replayed — investigate the path between browser and engine. |
| `smiths_webrtc_dtls_handshakes_total{outcome="other"}` ticks up | Handshake timeouts, cipher mismatches, transport errors. Check the engine log — the offer's `o=` origin line is logged on every failure for correlation. |
| Browser receives `offer-rejected: no common codec`                | The `RTP/AVP` offer advertised no codec in the engine's supported set (default: PCMU). Add PCMU to the offer.      |
| `[webrtc] tls_cert/tls_key are set but this adapter binds plaintext` in logs | Expected warning — front the engine with nginx/Caddy for `wss://`.                                                 |
| 404 on `/smiths/webrtc`                                           | `[webrtc] enabled = true` missing from config, or the engine wasn't restarted after editing.                       |
| Connection refused on `ws_bind`                                   | Another process owns the port, or the firewall blocks it.                                                          |
