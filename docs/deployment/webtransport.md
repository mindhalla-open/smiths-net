# WebTransport signaling (slice 5.7 / P19)

Browsers that speak WebTransport can open a signaling session
against the engine over HTTP/3 + QUIC **without implementing any
SIP**. Each WebTransport session carries JSON-framed `WtSignal`
messages — offer/answer/ICE-candidate/bye — over a bidirectional
stream. Media lands on the engine's existing DTLS-SRTP path, same
as a SIP-over-WebSocket browser call today, but with one fewer
layer of translation.

## Scaffold status (v0.55.0)

Slice 5.7 ships the **signaling protocol** + **listener trait** +
**config surface** + **browser demo**. The **QUIC runtime is
deferred** to a follow-on slice (matches the `sip-quic` and
`mcp-http3` scaffolds). Flipping `[webtransport] enabled = true`
with the `webtransport` Cargo feature compiled in:

- Logs a loud `webtransport: scaffold listener refusing bind;
  runtime lands with a later slice` at boot.
- Returns `WtListenError::ScaffoldOnly` from
  `WebTransportListener::bind`.
- Otherwise lets the engine start — the config is validated, the
  feature is advertised in `--version`, operators who build
  integration against the protocol shape today can do so.

Flipping `enabled = true` with a binary built **without**
`--features webtransport` is a config error that fails startup —
same pattern as `[mcp.http3]` + `--features mcp-http3`.

## Wire protocol

Each WebTransport session is one **bidirectional stream**
exchanging UTF-8 JSON frames — one frame per WebTransport
*message*. The frame's `type` field is the discriminator; full
schema lives in `crates/smiths-sip/src/webtransport.rs` as the
`WtSignal` enum. See `examples/browser-webtransport/README.md` for
a table of example frames.

### Session lifecycle

```text
client                                                      engine
  │                                                             │
  │ ── WebTransport CONNECT /smiths/signal ─────────────────▶   │
  │                                                             │
  │ ── bidi stream open ────────────────────────────────────▶   │
  │ ── { "type": "session-init", "tag": "demo" } ──────────▶    │
  │                                                             │
  │  ◀ { "type": "session-ack", "session_id": 17,           ─── │
  │     "protocol_version": 1 }                                 │
  │                                                             │
  │ ── { "type": "offer", "session_id": 17,                ──▶  │
  │     "sdp": "v=0\r\n..." }                                   │
  │                                                             │
  │  ◀ { "type": "answer", "session_id": 17,               ─── │
  │     "sdp": "v=0\r\n..." }                                   │
  │                                                             │
  │ ── trickle ICE candidates both ways (ice-candidate frames) ─│
  │ ── ice-end when done ─────────────────────────────────────  │
  │                                                             │
  │         — DTLS-SRTP media flows out-of-band on                  │
  │           engine's existing RTP paths —                         │
  │                                                             │
  │ ── { "type": "bye", "session_id": 17 } ────────────────▶   │
  │  ◀ stream close ───────────────────────────────────────────│
```

### Error handling

Engine errors are terminal: after emitting `{"type":"error",...}`
the engine closes the stream. Client errors (decode failure,
unknown frame type) prompt an engine `error` frame with code
`"malformed-frame"` and then stream close.

## Deployment

### TLS cert

Browsers won't open a WebTransport session without a valid TLS
certificate. Options in order of operational ease:

1. **Publicly-trusted cert (Let's Encrypt, ZeroSSL, etc.)** —
   the normal path. `cert_path` + `key_path` point at PEM-encoded
   files the engine hot-reloads (v0.57.0's config hot-reload
   picks this up; until then, SIGHUP plus manual cert rotation).
2. **Self-signed + `serverCertificateHashes`** (Chromium only) —
   useful for self-hosted demos where a real CA isn't practical.
   The browser page supplies the cert's SHA-256 hash to
   `new WebTransport(url, { serverCertificateHashes: [...] })`.
   Firefox doesn't support this; ignore it if you need Firefox
   support.
3. **Operator-trusted CA** — internal PKI; same cert plumbing as
   option 1.

### Port

Default bind is `127.0.0.1:7880`. Move to `0.0.0.0:443` behind a
real load balancer for production; browsers are happiest with
WebTransport on 443. Distinct from SIP ports so existing SIP
listeners stay independent.

### CORS

WebTransport is bound by the same origin model as `fetch()`.
Pages served from `https://app.example.com` opening a transport
to `https://webtransport.example.com/smiths/signal` need an
`Access-Control-Allow-Origin` response header from the engine on
the initial HTTP/3 CONNECT. The runtime slice wires this; the
scaffold rejects before that matters.

## What this doesn't replace

- **SIP over UDP/TCP/TLS.** Still the canonical way to connect
  softphones and PBXs. WebTransport is for the **browser** leg.
- **SIP-over-WebSocket (RFC 7118).** If your target is browsers
  running a JS SIP stack (sip.js, jssip), stick with WebSocket —
  WebTransport is the path for browsers that want out of the SIP
  stack entirely.
- **WebRTC-native signaling (slice 5.10).** That slice lands the
  native WebRTC path over WebSocket JSON — which is a simpler
  baseline than WebTransport. 5.7 is the *transport substrate*
  for 5.10's message shape to optionally ride over when QUIC's
  head-of-line-blocking-free path matters.

## See also

- `crates/smiths-sip/src/webtransport.rs` — `WtSignal`,
  `WebTransportListener`, `NullWebTransportListener`.
- `examples/browser-webtransport/` — static HTML + JS demo.
- `docs/architecture/07-http3.md` — the MCP HTTP/3 scaffold this
  slice's pattern mirrors.
- `docs/architecture/07-webrtc-interop.md` — how the media plane
  already handles the DTLS-SRTP side that WebTransport plugs
  into.
