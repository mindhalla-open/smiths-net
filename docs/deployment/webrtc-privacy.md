# WebRTC privacy modes (slice 5.11-privacy)

The engine's WebRTC adapter offers three privacy postures that
layer additively. Each picks a defense against a specific threat;
none is a silver bullet. This doc lays out the threat model, how
to pick a mode, and what each mode does *not* protect against.

## Modes

```toml
[webrtc.privacy]
mode           = "open"         # "open" | "relay_only" | "strict"
redaction_key  = ""             # required + non-empty in "strict"
```

- **`open`** (default) — no hardening. Offers go through
  untouched; candidates of every type land in answers and logs
  verbatim. Fine for dev, on-premises LAN deployments, and
  anywhere the operator already owns the network path.
- **`relay_only`** — filter-phase enforcement. Offers carrying
  `host` / `srflx` candidates are rejected with
  `offer-rejected: privacy policy forbids host/srflx
  candidates`. The engine still emits answers from its own
  fabric ports; the wire effect is that every peer is forced
  through a TURN relay, so clients must set
  `iceTransportPolicy: "relay"` in their `RTCConfiguration`.
- **`strict`** — `relay_only` + keyed-hash IP redaction
  (`blake3`-style keyed SHA-256) on every peer IP the engine
  emits into a log, CDR row, or tracing span. Raw IPs never
  cross the observability boundary; operators see
  `ip=<redacted:3a6f…>:55123` and the `{port}` is preserved
  for triage.

## Threat model

| Threat                                        | `open` | `relay_only` | `strict` |
|-----------------------------------------------|:------:|:------------:|:--------:|
| On-path attacker reads SDP body               |   ✗    |      ✗       |    ✗     |
| Any NAT-local attacker reads media            |   ✗    |      ✗       |    ✗     |
| Client leaks its LAN IP via `host` candidate  |   ✓    |      ✗       |    ✗     |
| Operator logs leak client IP                  |   ✓    |      ✓       |    ✗     |
| CDR / audit retention leaks client IP         |   ✓    |      ✓       |    ✗     |
| Metrics (Prometheus) leak client IP           |   —    |      —       |    —     |
| Malicious client spoofs host→relay transition |   ✓    |      ✗       |    ✗     |

`✗` = blocked or made much harder. `✓` = not addressed.
`—` = not a concern (metrics use bounded-cardinality labels that
don't include IPs under any mode).

### What the modes do NOT protect against

- **SDP body confidentiality.** The signaling WebSocket is
  plaintext today. Front the engine with a TLS-terminating
  reverse proxy for `wss://`; `strict` mode logs a warning at
  boot when the configured certificate paths aren't set,
  matching the `tls_cert` / `tls_key` warning the 5.10-runtime
  slice ships.
- **Media confidentiality.** SRTP encrypts the payload, but a
  5-tuple observer still sees packet sizes + timing. Traffic
  analysis resistance is out of scope — that's what a VPN or
  onion route is for.
- **Malicious TURN / external TURN.** When
  `[webrtc.turn] external_url` points at an operator-controlled
  relay, that relay sees all media plaintext unless combined
  with end-to-end encryption (future E2EE slice). Trust your
  TURN or run the embedded one.
- **Client-side logging.** Browsers still log the full
  candidate list in their internal `webrtc-internals` page.
  The engine can't reach across that boundary.
- **Endpoint compromise.** A compromised client laptop renders
  every mode moot. This is fundamental — we can't protect
  against the owner of one half of a call.

## Picking a mode

```
┌──────────────────────────────────────────────────────────────┐
│ Is every client on a network the operator already owns       │
│ (private LAN, dedicated VPN)?                                │
│    ├── yes   → mode = "open"                                 │
│    └── no, clients reach the engine over the public internet │
│         ├── Operators have TURN servers configured?          │
│         │    ├── yes   → mode = "relay_only"                 │
│         │    └── no    → mode = "open" (accepting the IP     │
│         │               leak; add TURN before going public)  │
│         └── Retention / audit compliance forbids storing     │
│            client IPs in logs?                               │
│              ├── yes → mode = "strict" (with a rotating      │
│              │        redaction_key)                         │
│              └── no  → mode = "relay_only" is enough         │
└──────────────────────────────────────────────────────────────┘
```

Rule of thumb: run **`relay_only`** for any internet-exposed
deployment once TURN is in place (slice 5.11-turn ships an
embedded server). Upgrade to **`strict`** when a compliance or
regulatory requirement names "MUST NOT log client IP."

## `strict` mode: `redaction_key` rotation

The redaction key is the secret that anchors the hash. Two
properties matter:

1. **Correlation within a key lifetime.** Two log lines in the
   same rotation window hash the same IP to the same token, so
   an operator can still correlate events on one client — they
   just can't recover the IP.
2. **Anonymity across rotations.** After rotation, yesterday's
   redacted token no longer matches today's; historical logs
   don't reveal that "this same client came back today."

Rotation recipe:

```sh
# 1. Generate a fresh key (32 bytes of entropy).
NEW_KEY=$(openssl rand -hex 32)

# 2. Edit config.toml → webrtc.privacy.redaction_key = "$NEW_KEY"
sed -i '' -E "s/^redaction_key  *= .*/redaction_key  = \"$NEW_KEY\"/" config.toml

# 3. Trigger hot reload — the 5.8-b adapter picks it up on the
#    next offer without restarting.
smiths-net reload --pid $(pgrep smiths-net) --config config.toml
```

Rotate on a schedule that matches your log retention:

- 7-day log retention → rotate weekly.
- 30-day retention → rotate monthly.
- Indefinite retention → don't.

**Pitfall**: don't rotate the key *mid-incident*. While
investigating a live issue, correlation across log lines is
what you need; a rotation during triage breaks the thread.

## Observability

- `smiths_webrtc_candidates_rejected_total{reason}` — offers
  the privacy filter dropped; `reason` ∈ `host` / `srflx`. A
  rising `host` slope against `relay_only` = clients that
  haven't set `iceTransportPolicy`.
- `smiths_webrtc_privacy_redactions_total` — counter of peer
  IPs the engine redacted in `strict`. Bumps once per render
  (one log line touching a peer IP = one increment).

## Default-safe posture

The engine ships with `mode = "open"` so a first-time operator
who hasn't thought about privacy isn't surprised by 488s on
their dev traffic. For production deployments, flip to
`relay_only` or `strict` intentionally — the config parse will
accept either mode against any other setup without a restart.
