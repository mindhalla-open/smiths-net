# Contributing to smiths-net

Thanks for looking at the code. Below is the minimum a contributor needs
to get productive.

## Prerequisites

- Rust stable, pinned via `rust-toolchain.toml` (`rustup` installs it on
  the first `cargo` invocation). The minimum supported version is the
  workspace `rust-version` in `Cargo.toml` (currently 1.95); CI builds
  on current stable.
- Unix shell. macOS / Linux supported; Windows works but is less tested.
- Linux only: `cargo test --workspace` and `cargo build --workspace`
  compile `smiths-softphone`, whose audio backend (`cpal`) needs the
  ALSA headers — `sudo apt-get install libasound2-dev` (CI installs the
  same package). macOS uses CoreAudio and needs nothing extra. The
  `smiths-net` engine binary itself needs only a C compiler (bundled
  SQLite, TLS crypto backends) and runs as a static binary.
- Optional: `sipp` for the manual scenarios under `scenarios/sipp/`
  (see `docs/qa/manual-testing.md`).

## Development loop

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release
```

Run the engine locally:

```bash
cargo run --release -- --config examples/config.toml
# in another shell:
curl http://127.0.0.1:8080/health
# => {"status":"ok", ...}
```

`smiths-net --config <file> validate` checks a config without starting
the engine (exit 0 = ok, 1 = parse error, 2 = semantic error); unknown
keys are rejected, so run it on any config you ship in `k8s/` or docs.

Structured JSON logs are on by default. For readable dev output:

```bash
SMITHS__OBSERVABILITY__LOG_FORMAT=pretty \
  cargo run -- --config examples/config.toml
```

`RUST_LOG` is honored as an override for the log filter.

## Conventions

- **Edition 2024**, MSRV 1.95.
- Workspace-wide `unsafe_code = "deny"`. The single sanctioned use is
  the sidecar sandbox in `crates/smiths-sidecar/src/supervisor.rs`
  (`CommandExt::pre_exec`), carrying a scoped `#[allow(unsafe_code)]`
  with a justification comment. New `unsafe` follows the same pattern
  and is called out in the PR.
- Errors: `thiserror` in libraries, `anyhow` only in binary crates.
- Config: add new sections to `smiths-core::config::Config`; never parse
  config in sibling crates directly. Document new keys in
  `examples/config.toml`.
- Logging: `tracing` with `#[instrument]` on non-trivial async flows.
  Correlate by `call_id` and `transaction_id`.
- Async: single multi-thread `tokio` runtime; long-lived tasks take a
  `CancellationToken` clone for graceful shutdown.
- Notifications go on `smiths-core::EventBus`; control flow goes through
  the trait seams in `smiths-core` (`MediaFabric`, `SdpNegotiator`,
  `AiRegistry`, `CallOriginator`), which `smiths-cli` wires at startup.
  A crate may depend on a lower layer it genuinely composes, but never
  on `smiths-cli`, and `smiths-sip` must stay free of `smiths-sdp` and
  `smiths-media`. See `docs/architecture/01-crate-layout.md`.
- Comments explain the current design, not its history: no release
  numbers, slice or phase tags in code comments.

## Plugins and the cookbook

Plugins come in two tiers — WASM (in-process, `crates/smiths-wasm`) and
sidecar (out-of-process, `crates/smiths-sidecar`) — plus embedded Rhai
scripts. Minimal, copy-ready examples live in `plugins/cookbook/`
(`wasm/rust/`, `sidecar/python/`, `sidecar/nodejs/`, `script/rhai/`);
fuller examples, including the AI adapters, live in `plugins/examples/`.
The `cookbook` workflow builds the WASM examples and runs the sidecar
tests on every change under `plugins/cookbook/`. WASM examples need
`rustup target add wasm32-unknown-unknown`; their `target/` directories
are git-ignored, so commit only the `.wasm` a README says is shipped
prebuilt.

## Fuzzing

`fuzz/` is a separate Cargo workspace (it needs nightly) with three
`cargo-fuzz` targets: `sip_parser` (raw datagrams through the UAS
parsers), `via_branch` (structured Via headers through the branch
extractors and `rsip`), and `sdes_crypto` (SDP `a=crypto:` parsing).

```bash
cargo install cargo-fuzz && rustup toolchain install nightly
cargo +nightly fuzz run sip_parser -- -max_total_time=60   # from the repo root
```

The `fuzz-nightly` workflow runs each target for about five hours every
night (the six-hour GitHub-hosted job cap), carries the corpus across
runs and uploads crash inputs. Triage steps are in `fuzz/README.md`.

## Docker

`Dockerfile` cross-compiles a static musl `smiths-net` for linux/amd64
and linux/arm64 onto a distroless-static base. CI builds the amd64 image
on every push; the release workflow publishes both architectures and
attaches the binaries to the GitHub release.

```bash
docker buildx build --platform linux/amd64 -t smiths-net .
```

## PR checklist

- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] `CHANGELOG.md` updated under the `[Unreleased]` section

## Where things live

| Area                            | Location                                   |
|---------------------------------|--------------------------------------------|
| Project goals + scope           | `README.md`                                |
| Release notes                   | `CHANGELOG.md`                             |
| Product spec                    | `docs/openswitch.md`                       |
| Design (crates, SIP core, plugins, MCP, media, FSM) | `docs/architecture/` |
| Roadmap and phase plans         | `docs/plans/`                              |
| Manual test procedures          | `docs/qa/`                                 |
| Running it in production        | `docs/operator-runbook.md`, `docs/deployment/`, `docs/observability/` |
| Writing MCP / A2A tools         | `docs/tool-authoring.md`                   |
| Deployment manifests            | `k8s/`, `systemd/`, `Dockerfile`           |

## Commit style

Short imperative subject ≤ 72 chars. Body explains the *why* when it is
not obvious.

## License

By contributing you agree that your contribution is licensed under the
Apache License 2.0 — same as the project (`LICENSE`).
