# Phase 1 — SIP Core

**Goal**: a working SIP UAS + UAC without media. Can register itself, answer
`OPTIONS`, handle digest auth, manage transactions and dialogs per RFC 3261.

## Deliverables

1. `smiths-sip` crate with:
   - UDP transport (required), TCP transport (required).
   - SIP parser/serializer (choose `rsip` v0.4 to start).
   - Transaction layer FSM with RFC 3261 timers A–K.
   - Dialog layer FSM (Early → Confirmed → Terminated).
   - Digest auth (MD5 + SHA-256), challenge/response on 401/407.
2. `smiths-core` integration:
   - `SipEvent` variants published on the bus.
   - `ControlEvent::SendSip` consumed by transport.
3. `smiths-cli` wiring:
   - `sip.bind` addresses opened on startup.
   - Graceful drain on shutdown: stop accepting, finish in-flight
     transactions up to `sip.drain_timeout` (default 10 s).
4. Integration tests driving the engine with `pjsua` or `sipp`.

## Step-by-step tasks

1. **Transport layer** (`smiths-sip::transport`)
   - UDP socket reader task per bound address; framing by datagram.
   - TCP listener + connection tasks; framing by Content-Length.
   - Outbound send API: `Transport::send(target, bytes)`.
   - TLS stubbed behind `tls` feature; real impl in phase 6.
2. **Parser** (`smiths-sip::msg`)
   - Wrap `rsip` types; add typed headers we care about.
   - Re-emit malformed messages as a `ParseError` event.
3. **Transactions** (`smiths-sip::txn`)
   - Four FSMs: client-invite, client-non-invite, server-invite,
     server-non-invite.
   - Timer wheel from `smiths-core` drives RFC 3261 timers A–K.
   - Matching by branch parameter + method (RFC 3261 §17.2.3).
4. **Dialogs** (`smiths-sip::dialog`)
   - Dialog ID = (Call-ID, local-tag, remote-tag).
   - State: Early → Confirmed → Terminated.
   - Target refresh via re-INVITE / UPDATE (UPDATE optional in v1).
5. **Auth** (`smiths-sip::auth`)
   - Parse `WWW-Authenticate` / `Proxy-Authenticate`.
   - Compute `Authorization` with MD5 and SHA-256, `qop=auth`.
   - Nonce cache keyed by realm.
6. **Event bus integration**
   - `SipEvent::RequestReceived`, `ResponseReceived`, `TransactionTerminated`,
     `DialogCreated`, `DialogTerminated`.
   - `ControlEvent::SendRequest`, `SendResponse` consumed by transport.
7. **Testkit**
   - `smiths-testkit::fake_uac` — tiny UAC that can REGISTER, OPTIONS,
     INVITE (no SDP yet), and assert on responses.
   - `smiths-testkit::fake_uas` — answers canned responses.
8. **Integration tests** in `crates/smiths-testkit/tests/`:
   - `options_roundtrip.rs` — engine answers `OPTIONS` with 200.
   - `register_with_auth.rs` — REGISTER → 401 → REGISTER with auth → 200.
   - `invite_401_cancel.rs` — engine INVITE, server challenges, engine
     retries, then CANCEL.
9. **Fuzz harness** for the parser (`cargo-fuzz`), checked into
   `crates/smiths-sip/fuzz/`.

## Acceptance criteria

- [ ] `pjsua` can send `OPTIONS` and receive `200 OK`.
- [ ] `pjsua` can REGISTER against the engine (engine acting as registrar
  with a hard-coded account) through a 401 challenge.
- [ ] `sipp` scenario: 100 concurrent REGISTERs complete under 1 s median
  on a dev laptop.
- [ ] All RFC 3261 mandatory timers (A, B, E, F, G, H, I, J, K) fire
  within spec tolerance under simulated loss.
- [ ] Malformed packet fuzz corpus (10k cases) produces zero panics or
  hangs over a 5-minute run.
- [ ] `cargo bloat` shows `smiths-sip` contributes < 4 MB to release size.

## Out of scope

- SDP, RTP, media. That is phase 2.
- TLS transport beyond a compile-checked stub.
- Proxy behavior (Record-Route merging, loose routing); v1 is UA-scoped.

## Risks & notes

- Consider ditching `rsip` for a hand-rolled `nom` parser if it blocks us
  on edge cases; defer decision to the end of the phase.
- Timer wheel granularity: 10 ms is fine for RFC timers; do not over-tune.
- Do not leak parser types across the bus. Bus payloads use
  `smiths-proto::SipMessage` so plugins see the same types later.
