# Changelog

All notable changes to **smiths-net** are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Full-binary end-to-end test (`crates/smiths-cli/tests/e2e.rs`):
  spawns the real `smiths-net` binary with a temp TOML on ephemeral
  ports, polls `/health`, drives `OPTIONS` + an unknown method over UDP,
  sends `SIGTERM`, and asserts a clean exit. Pure Rust, no external
  tooling (unix only for now — Windows signal path is a follow-up).
- `tempfile` added to `[workspace.dependencies]`.
- UAS now answers `INVITE` with `100 Trying` + `200 OK` (with a `Contact`
  header), creates an early in-memory dialog, confirms it on `ACK`, and
  tears it down on `BYE` with `200 OK`. `BYE` against an unknown dialog
  returns `481`. Dialog state is keyed by `(Call-ID, local-tag,
  remote-tag)` per RFC 3261.
- `SipEvent::DialogCreated` and `SipEvent::DialogTerminated` published on
  the event bus.
- Three new integration tests (`crates/smiths-sip/tests/invite.rs`):
  `invite_establishes_dialog_ack_then_bye`,
  `bye_without_dialog_returns_481`,
  `invite_retransmit_replays_same_200`.

### Changed

- `UasServer::new` now returns `Result<Self, Error>` — it reads the
  transport's local address to precompute a `Contact` header.
- `INVITE` responses now carry an SDP answer body; `build_response` /
  `respond` take an explicit body slice and compute `Content-Length`.
- Release binary size: 2.6 MB → **2.7 MB** (SDP types + tests).

### Added — Media bridge (audio end-to-end)

- `smiths-media::bridge` with `Bridge` and `Leg` — a byte-transparent
  two-leg UDP forwarder. One `recv_from` / `send_to` task per direction
  driven by a shared `CancellationToken`. Unit-tested on loopback.
- **Rendezvous bridging in the UAS.** Two `INVITE`s whose Request-URI
  user-part matches (e.g. both to `sip:room-1@engine`) are paired: the
  engine extracts each offer's media endpoint from SDP, spins up a
  `Bridge` between the engine's allocated sockets, and the two UAs
  exchange RTP through us. A `BYE` from either side tears the bridge
  down.
- `smiths-sip` now depends on `smiths-media` (acknowledged sibling-dep
  debt; future work will route bridge lifecycle through the event bus).
- `smiths-testkit` grew a real test toolkit:
  - `TestUac` — INVITE with a PCMU-only SDP offer, reads `100`/`200`,
    parses the engine's SDP answer, sends ACK and BYE.
  - `rtp::RtpPacket` — minimal RTP v2 encode/decode (no extensions /
    CSRCs / padding).
  - `codec` — bit-exact G.711 μ-law encoder/decoder.
  - `signal::sine_wave` — tone generator returning `Vec<i16>`.
  - `wav::write_mono_pcm16` — minimal RIFF/WAVE writer (PCM-16 mono)
    so a human can open the received audio.
- New integration test `two_uas_call_preserves_audio_byte_for_byte`:
  two `TestUac`s INVITE `sip:call-1@engine`, engine bridges, UA-A sends
  50 frames of 1 kHz sine encoded as PCMU @ 8 kHz, UA-B receives and
  asserts a middle-tail slice is bit-identical with the sent μ-law
  stream. The received audio is also written to
  `/tmp/smiths-call-received.wav` for manual listening.
- Internal extensions supporting the bridge:
  - `RequestSummary` gained `ruri_user`; `summarize_request` parses the
    user-part out of `sip:…@…` / `sips:…@…` / `<sip:…@…>` Request-URIs.
  - `sdp_remote_rtp` extracts the peer RTP endpoint from an SDP offer
    (media-level `c=` with session-level fallback).

### Added — SDP

- New `smiths-sdp` crate with a minimal RFC 8866 subset:
  - Types: `SessionDescription`, `Origin`, `ConnectionInfo`,
    `MediaDescription`, `MediaKind`, `RtpMap`, `Direction`.
  - Parser: accepts `v=`, `o=`, `s=`, `c=`, `t=`, `m=`, `a=rtpmap`,
    and the four direction attributes. Tolerates bare `\n` endings.
  - `Display` impls serialize back to wire SDP with CRLF endings.
  - Offer/answer `Negotiator` with PCMU / PCMA / Opus passthrough.
    Picks the first offered codec whose `(name, clock)` matches the
    engine's supported list; static payload types without `rtpmap`
    (legacy PCMU=0, PCMA=8) are recognized. Returns
    `NegotiationResult::Mismatch` → MVP guardrail for transcoding.
  - Eight unit tests (parse, round-trip, bare-LF, codec pick, legacy PT,
    direction reversal, mismatch, missing-version rejection).
- UAS wired to the negotiator:
  - `INVITE` with `Content-Type: application/sdp` is parsed and
    negotiated; if Mismatch → `488 Not Acceptable Here`; on malformed
    SDP → `400 Bad Request`.
  - On accept, allocates a fresh UDP socket (OS-chosen ephemeral port)
    and publishes the port in the SDP answer's `m=`/`c=`. The socket is
    held on the dialog record so step 3 can forward RTP through it.
- New `Content-Type` / body extraction in `summarize_request` and
  header/body splitting used by both parser and response builder.
- Integration tests (`crates/smiths-sip/tests/sdp.rs`):
  `invite_with_sdp_offer_gets_sdp_answer`,
  `invite_with_only_unknown_codecs_returns_488`.

## [0.1.0] - [2026-04-18]

Phase 1 slice — SIP signaling over UDP with an `OPTIONS`-answering UAS
and the MVP guardrails (`Transport` trait, `CredentialStore` trait) in
place. Full RFC 3261 transaction FSMs, TCP/TLS transports, REGISTER, and
digest challenge/response land in subsequent passes.

### Added

- `smiths-sip` crate:
  - `Transport` trait with message-level (not byte-stream) semantics —
    MVP guardrail for later TCP, TLS, QUIC, SOCKS-tunneled, WebTransport
    backings.
  - `UdpTransport` implementation with a spawned reader task feeding an
    mpsc channel, cancellation-aware shutdown.
  - `UasServer` that parses with `rsip`, answers `OPTIONS` with `200 OK`
    and rejects other methods with `405 Method Not Allowed`, preserving
    all mandatory headers (`Via`, `From`, `To` with added `tag`,
    `Call-ID`, `CSeq`) per RFC 3261 §8.2.6.
  - UDP retransmission dedupe via a bounded `DashMap` keyed by `Via`
    branch.
  - `auth::CredentialStore` trait + `InMemoryCredentialStore` — MVP
    guardrail for pluggable subscriber databases (SQLite, Postgres,
    LDAP, sidecar) without core changes.
  - `Error` type via `thiserror`.
- `smiths-core`:
  - `SipConfig` (`bind`, `transports`, `drain_timeout_secs`) with defaults
    (`0.0.0.0:5060`, UDP only).
  - `SipTransport` enum covering UDP / TCP / TLS; only UDP is wired in
    this phase.
  - `SipEvent::{RequestReceived, ResponseSent, ParseError}` published
    on the event bus.
- `smiths-cli`:
  - Per-bind UDP SIP spawn at startup.
  - Graceful shutdown drains SIP tasks before the health endpoint and
    publishes `SystemEvent::ShutdownComplete`.
- `examples/config.toml`: `[sip]` section with defaults.
- Integration tests (`crates/smiths-sip/tests/options.rs`):
  - `options_returns_200_ok`
  - `unknown_method_returns_405`
  - `retransmission_replays_cached_response`
- Six new unit tests (summary parsing, response building, tag uniqueness,
  credential store CRUD).
- `dashmap`, `rsip`, `bytes` added to `[workspace.dependencies]`.

### Changed

- Workspace version bumped `0.0.0` → `0.1.0`.
- Release binary size: 2.4 MB → **2.6 MB** (rsip + dashmap overhead;
  still comfortably under the 20 MB target).
- `smiths-cli` log line at startup now includes `sip_binds` and
  `sip_transports`.

## [0.0.0] - [2026-04-17]

Phase 0 — Foundation. Workspace scaffolding, core runtime primitives, and a
boot-and-shutdown binary. No SIP / media / plugins yet.

### Added

- Cargo workspace (`resolver = "3"`, edition 2024, MSRV 1.85) with 11 crates:
  `smiths-core`, `smiths-proto`, `smiths-sip`, `smiths-sdp`, `smiths-media`,
  `smiths-plugin`, `smiths-wasm`, `smiths-sidecar`, `smiths-mcp`,
  `smiths-cli`, `smiths-testkit` (all but `smiths-core` and `smiths-cli`
  are placeholders).
- Workspace lints: `unsafe_code = "forbid"`, clippy pedantic warn with
  pragmatic allows.
- Centralized dependency versions in `[workspace.dependencies]`.
- Release profile tuned for size (thin LTO, 1 codegen unit, stripped
  symbols).
- `rust-toolchain.toml` pinning stable channel with rustfmt + clippy.
- GitHub Actions CI: fmt + clippy + test + release build.
- `examples/config.toml` with commented defaults.
- `smiths-core`:
  - `config::Config` layered loader (defaults → TOML file → `SMITHS__*` env,
    `deny_unknown_fields`).
  - `bus::EventBus` over `tokio::sync::broadcast`.
  - `event::{Event, SystemEvent}` with `#[non_exhaustive]`.
  - `shutdown::Shutdown` over `tokio_util::CancellationToken`, handling
    SIGINT/SIGTERM on unix and Ctrl-C elsewhere.
  - `error::Error` via `thiserror`.
  - Seven green unit tests (bus round-trip, config defaults/env/TOML, shutdown
    cancel).
- `smiths-cli` binary `smiths-net`:
  - `clap` CLI (`--config`, `--log`), with `RUST_LOG` honored as override.
  - `tracing-subscriber` JSON or pretty output selected by config.
  - Axum `GET /health` endpoint with graceful shutdown tied to the cancel
    token.
- Release binary size: **2.4 MB** (target was < 20 MB).

### Docs

- `README.md` project pitch.
- `CONTRIBUTING.md` — prerequisites, dev loop, conventions, PR checklist.
- `LICENSE` — Apache-2.0.
- `docs/openswitch.md` — authoritative spec.
- `docs/architecture/` — overview, crate layout, plugin system, MCP + ops,
  post-MVP scope.
- `docs/plans/` — roadmap, MVP phase docs (0 through 6), post-MVP phases
  (P7–P23), and a live implementation TODO.

### Fixed

- `.gitignore`: added `.DS_Store` to the ignore list.

[Unreleased]: https://github.com/mindhalla/smiths-net/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/mindhalla/smiths-net/releases/tag/v0.1.0
[0.0.0]: https://github.com/mindhalla/smiths-net/releases/tag/v0.0.0
