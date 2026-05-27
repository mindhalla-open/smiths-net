# Phase 0 — Foundation

**Goal**: the skeleton everything else lives in. An empty binary that boots,
loads config, serves a health check, and shuts down cleanly. No SIP logic
yet.

## Deliverables

1. Cargo workspace with the 11 crates from `architecture/01-crate-layout.md`
   (most are empty `lib.rs` + a doc comment).
2. `smiths-cli` binary that:
   - parses CLI flags via `clap`,
   - loads TOML config via `figment`,
   - initializes `tracing-subscriber`,
   - starts a tokio multi-thread runtime,
   - exposes `GET /health` (always returns `ok` for now),
   - handles SIGINT/SIGTERM with graceful shutdown.
3. Event bus prototype in `smiths-core::bus` (typed channels, pub/sub).
4. CI workflow (`.github/workflows/ci.yml`):
   - `cargo fmt --check`
   - `cargo clippy --all-targets -- -D warnings`
   - `cargo test --workspace`
   - `cargo build --release`
5. `docs/CONTRIBUTING.md` with the development loop.

## Step-by-step tasks

1. Convert current single-crate project to a workspace.
   - Move existing `src/main.rs` into `crates/smiths-cli/src/main.rs`.
   - Create workspace `Cargo.toml` with `members = ["crates/*"]`.
   - Create empty `lib.rs` for each crate.
2. Add pinned dependencies to `[workspace.dependencies]`.
3. Implement `smiths-core`:
   - `config::Config` struct + loader (defaults → file → env).
   - `bus::EventBus` using `tokio::sync::broadcast` for topic channels.
   - `shutdown::Shutdown` token that broadcasts on signal.
4. Implement `smiths-cli`:
   - `clap` CLI with `--config`, `--log-level`.
   - Initialize tracing.
   - Start runtime; run health HTTP server on a config-bound port.
   - Wait on shutdown.
5. Add unit tests for config loading and shutdown signal wiring.
6. Add GitHub Actions CI.
7. Tag `v0.0.0`.

## Acceptance criteria

- [ ] `cargo build --release` produces a binary under 20 MB.
- [ ] `smiths-cli --config examples/config.toml` starts and logs "ready".
- [ ] `curl :8080/health` returns `{"status":"ok"}`.
- [ ] `kill -TERM <pid>` exits within 2 s with "graceful shutdown complete".
- [ ] CI green on a fresh checkout.
- [ ] `cargo test --workspace` passes (no tests yet is fine — the crates
  must at least compile).

## Out of scope for this phase

- Any SIP, RTP, WASM, MCP, or plugin code. Only scaffolding.

## Risks & notes

- Keep the event bus generic but do not over-engineer — `broadcast` + a
  `TypeMap` of senders is enough for v1.
- Resist adding `anyhow`/`thiserror` patterns until a concrete error use
  case appears; one `SmithsError` enum in `smiths-core` is fine to start.
