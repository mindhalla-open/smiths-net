# Changelog

All notable changes to **smiths-net** are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/mindhalla/smiths-net/compare/v0.0.0...HEAD
[0.0.0]: https://github.com/mindhalla/smiths-net/releases/tag/v0.0.0
