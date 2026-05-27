# WebRTC interop

End-to-end walk-through of what it takes to put a browser on one
side and a SIP counterparty on the other, with smiths-net as the
B2BUA in between. Assumes familiarity with
`docs/architecture/06-sip-core.md` (transaction + dialog) and
`docs/architecture/04-post-mvp-scope.md` (the DTLS-SRTP rollout
plan).

---

## The four layers

WebRTC interop stacks four independent concerns. Each lands in its
own slice; nothing in the chain is optional for a real browser call.

| Layer  | Crate          | What it solves                                           | Landed in |
|--------|----------------|----------------------------------------------------------|-----------|
| SDP    | `smiths-sdp`   | Recognize DTLS-SRTP + ICE attrs; 488 on mismatch         | v0.25.0   |
| DTLS   | `smiths-dtls`  | Mutual cert handshake; extract SRTP keys                 | v0.26.0   |
| ICE    | `smiths-ice`   | Host candidates, STUN binding, connectivity checks       | v0.27.0   |
| RTP    | `smiths-media` | SRTP transform on each leg (shared with SDES)            | v0.15.0   |

## Call flow (browser ↔ engine ↔ SIP UA)

```
  browser                    smiths-net                    sip-ua
     │                           │                            │
     │─── INVITE (WebRTC) ──────►│                            │
     │    m=audio UDP/TLS/RTP/SAVP                            │
     │    a=fingerprint …                                     │
     │    a=setup:actpass                                     │
     │    a=ice-ufrag / pwd                                   │
     │    a=candidate host …                                  │
     │                           │──── INVITE (RTP/AVP) ──────►
     │                           │     transcoded SDP         │
     │                           │                            │
     │                           │◄──── 200 OK (SDP answer)───│
     │◄── 200 OK (SDP answer) ───│                            │
     │    a=fingerprint (engine's)                            │
     │    a=setup:passive                                     │
     │    a=candidate (engine's host)                         │
     │                           │                            │
     │── ICE binding checks ────►│                            │
     │◄─ ICE binding responses ──│                            │
     │                           │                            │
     │── DTLS handshake ────────►│                            │
     │◄── DTLS response ─────────│                            │
     │ (SRTP keys extracted from DTLS keying material)        │
     │                           │                            │
     │── SRTP audio ──►[ decrypt │ encrypt_ab ─ RTP audio ───►│
     │◄── SRTP audio ──[ encrypt │ decrypt_ab ◄── RTP audio ──│
```

### What changes per leg

The engine negotiates SRTP **separately per leg**:

- The **browser leg** runs DTLS-SRTP. Keys come out of
  `extract_srtp_keying_material` (RFC 5764 §4.2).
- The **SIP-UA leg** can be plain RTP/AVP or RTP/SAVP (SDES). The
  negotiator doesn't care which half does DTLS — it drives each leg
  independently off the offer/answer it owns.

The bridge in `smiths-media` already threads a per-direction
`SrtpTransform` per leg (v0.15.0) — DTLS-SRTP reuses the same hook,
just with keys from a different source. No media-plane changes.

## Where each failure surfaces

Crypto failures are typed on the event bus as
[`SipEvent::MediaSecurityError`] so dashboards can count them without
scraping log lines:

| Failure                            | Kind                      | When it fires                      |
|------------------------------------|---------------------------|------------------------------------|
| Peer cert ≠ SDP fingerprint        | `DtlsFingerprint`         | DTLS handshake complete, cert mismatch |
| DTLS handshake failed otherwise    | `DtlsHandshake`           | Cipher mismatch, malformed message, reset |
| SRTP auth tag bad on inbound RTP   | `SrtpAuthTag`             | Per-packet, on the bridge hot path |
| Negotiated profile unsupported     | `UnsupportedSuite`        | Not `AES_CM_128_HMAC_SHA1_80`       |

Operators should treat the first three as equally severe — they all
mean **someone can't prove they own the key that the SDP promised**.
`UnsupportedSuite` is a misconfiguration signal, not a compromise
signal.

## Trickle ICE

`a=end-of-candidates` (RFC 8840) is parsed by the SDP module and
surfaced as [`MediaDescription::end_of_candidates`]. Before the flag
flips, the engine keeps the candidate set open — a re-INVITE
piggybacking more candidates doesn't trigger a full renegotiation.

Today the engine emits candidates all at once (no trickle-send); the
receive side is where trickle matters, because Chromium gathers in
the background and re-INVITEs as srflx / relay pairs arrive.

## Testing posture

The full browser harness — headless Chromium launches a page that
calls `RTCPeerConnection.createOffer`, posts it to the engine, and
asserts audio RTP flows both ways — is scaffolded under
`smiths-testkit::webrtc_harness` but **gated behind the `browser`
feature** because Chromium isn't available in every CI environment.

When the harness runs, it exercises:

- Offer with `UDP/TLS/RTP/SAVP` + host + srflx candidates.
- Engine side runs ICE binding checks, DTLS handshake, SRTP keying.
- The engine emits a downstream INVITE that a fake SIP UA answers
  with plain RTP/AVP.
- The browser page decodes inbound SRTP and a `getUserMedia` loop
  encodes outbound; both directions are verified with an RTP payload
  counter.

Unit tests (`smiths-dtls`, `smiths-ice`) cover the individual
primitives without needing Chromium — the harness is purely the
end-to-end "does a real browser work?" prove-out.

## What's still not in scope after slice 1.5

- TURN relay candidates (P16 or a dedicated follow-on).
- IPv6-only paths — the MVP bind is dual-stack but candidate gathering
  prefers IPv4 when both are available.
- Browser-to-browser through the engine as a mixing B2BUA — that's
  conferencing (P14, slice 5.5).
- WebTransport as a media transport (P19, slice 5.6) — distinct
  codepath from DTLS-SRTP/ICE.
