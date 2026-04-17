# Changelog

All notable changes to **smiths-net** are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
