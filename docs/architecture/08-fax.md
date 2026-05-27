# T.38 FAX (slice 5.4 / P13)

smiths-net ships a T.38 relay — **not** a fax terminal. The engine
sits between two fax-capable endpoints (ATAs, soft-PBXs, gateway
cards) that negotiate T.38 directly; our job is to carry their
UDPTL datagrams byte-identity and help them renegotiate from audio
into T.38 mid-call.

## Why T.38 at all

G.711 passthrough of fax audio through any packet network is
essentially broken. V.17 (14.4 kbps) and V.34 (33.6 kbps) modem
signaling depends on precise phase relationships between samples;
a dropped packet or 20 ms of jitter collapses the constellation
and restarts training. T.38 sidesteps the problem by **demodulating
at the source**: the fax gateway decodes the modem signaling into
structured IFP (Internet Facsimile Protocol) frames, carries those
over UDPTL, and remodulates at the destination. Loss tolerance is
built into UDPTL via redundancy: each datagram can carry the
previous N primary payloads so a single loss is recoverable without
retransmission.

## What the engine does, concretely

1. **SDP detection.** An INVITE or re-INVITE arrives with
   `m=image <port> udptl t38`. The SDP parser surfaces
   `MediaKind::Image`; `smiths_fax::find_fax_media` picks the
   block out.
2. **Answer composition.** The engine builds an answer via
   `smiths_fax::answer_fax_offer`. If the offer had both an audio
   m-line and a T.38 m-line (mid-call switch), the audio m-line
   comes back declined (port 0, `a=inactive`) per RFC 3264 §6;
   the T.38 block gets a locally-bound UDPTL port.
3. **Session construction.** The engine binds a UDP socket per leg
   and constructs a `UdptlSession`. The session spawns two
   forwarder tasks — one per direction — that `recv_from` the
   ingress socket, log the UDPTL sequence number at `debug` (when
   `trace_sequence = true`), and `send_to` the egress peer.
4. **Lifecycle.** The session is a `dyn MediaSession`. `stop()`
   cancels the forwarders. On BYE the engine drops the
   `Arc<UdptlSession>`; the sockets close; the UDP flows stop.

## What the engine does NOT do

- **Parse IFP.** The primary field of a UDPTL datagram is an
  ASN.1 PER-encoded `IFPPacket`. The relay doesn't care — it
  forwards bytes. A future feature (per-page accounting, DTMF
  relay on the fax path, bill-by-page CDR) would need an IFP
  parser; that's new code in `smiths-fax::ifp`, not a change to
  the session.
- **Terminate T.38.** If a deployment has one modem-talking
  endpoint (an analog fax machine on a POTS line) and one T.38
  endpoint, the converter between the two is a **gateway**, not
  a relay. Gateways run a full fax FSM plus a modem, usually
  spandsp — the engine is the wrong home for that, both because
  of the license footprint and because gateways are already
  available off-the-shelf (Asterisk's `res_fax_spandsp`,
  hardware ATAs).
- **Trigger the audio→T.38 switch.** CED-tone detection (the
  2100 Hz answer tone a fax machine emits) is a DSP task that
  lives in a plugin or at the terminal. When that signal fires,
  the UAC re-INVITEs through the engine; we handle the signaling
  side with `smiths_fax::fax_renegotiate` but we don't listen
  for tones ourselves.

## Relay vs transcoder: why one but not the other

A fax **relay** forwards UDPTL datagrams between two peers that
both speak T.38 — the tonic pattern. A fax **transcoder** would
sit between a T.38 peer and a G.711 peer, demodulating on the
T.38 side and modulating on the G.711 side (or vice versa). The
transcoder path needs full DSP and is exactly what gateways exist
for. The relay path is the useful-and-cheap subset; that's what
slice 5.4 ships.

Compare with audio transcoding (slice 5.3): the engine *does* ship
G.711 ↔ Opus because the conversion is tractable (stateless G.711,
well-understood Opus bindings) and the alternative (passthrough-only)
breaks mixed codec endpoints. Fax transcoding is a much bigger DSP
lift for much rarer deployments; the cost/benefit doesn't line up.

## UDPTL wire format (summary)

```
+-----------+---------------------+---------------------------+
| Seq (u16) | Primary IFP [len-p] | Error Recovery [count-p]  |
+-----------+---------------------+---------------------------+
```

- Sequence: 16-bit big-endian, wraps. Semantic equivalent of RTP's
  sequence number.
- Primary IFP: length-prefixed opaque bytes. Length prefix is
  T.38's "OpenType length" — 1 byte for 0..127, 2 bytes (high bit
  set on the first byte) for 128..16383.
- Error recovery (redundancy mode): 1-byte count N, followed by N
  length-prefixed copies of the previous N primaries. Most-recent
  prior first.

`smiths_fax::udptl::UdptlPacket` parses + encodes this layout. FEC
mode (T.38 Annex A.2) is not yet parsed — a relay doesn't need to
peek inside the FEC payload to forward it.

## Honest deferral: bridge integration

Bridging a `UdptlSession` into the live signaling path (atomic
swap of a `dyn MediaSession` on a re-INVITE, metadata ticking for
the call FSM, BYE propagation across the new session type) is the
same per-call-FSM refactor that slice 5.1's video dual-bridge and
slice 5.3's transcoding are queued behind. Primitives + SDP
surface + session type + renegotiation helper land in this slice;
wiring lands with the FSM refactor.

## See also

- `crates/smiths-fax/src/udptl.rs` — UDPTL parser/emitter.
- `crates/smiths-fax/src/session.rs` — `UdptlSession`.
- `crates/smiths-fax/src/sdp.rs` — `m=image udptl t38`
  detection, answer composition, `T38Params` attribute model.
- `crates/smiths-fax/src/renegotiate.rs` — audio → T.38
  re-INVITE SDP builder.
- `crates/smiths-fax/tests/reference_flow.rs` — relay
  byte-identity test shaped like a synthetic spandsp fax page.
