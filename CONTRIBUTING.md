# Contributing to smiths-net

Thanks for looking at the code. Below is the minimum a contributor needs
to get productive.

## Prerequisites

- Rust stable (pinned via `rust-toolchain.toml`; `rustup` will install on
  first `cargo` invocation).
- Unix shell. macOS / Linux supported; Windows works but is less tested.
- Phase 1+ integration tests need `pjsua` or `sipp` on `$PATH`.

## Development loop

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release
```

Run the binary locally:

```bash
cargo run --release -- --config examples/config.toml
# in another shell:
curl http://127.0.0.1:8080/health
# => {"status":"ok"}
```

Structured JSON logs are on by default. For readable dev output:

```bash
SMITHS__OBSERVABILITY__LOG_FORMAT=pretty \
  cargo run -- --config examples/config.toml
```

`RUST_LOG` honored as an override for the log filter.

## Conventions

- **Edition 2024**, MSRV 1.85.
- Workspace-wide `unsafe_code = "forbid"`. If you genuinely need
  `unsafe`, propose a scoped crate-level opt-out in the PR.
- Errors: `thiserror` in libraries, `anyhow` only in binary crates.
- Config: add new sections to `smiths-core::config::Config`; never parse
  config in sibling crates directly.
- Logging: `tracing` with `#[instrument]` on non-trivial async flows.
  Correlate by `call_id` and `transaction_id` once those exist.
- Async: single multi-thread `tokio` runtime; long-lived tasks take a
  `CancellationToken` clone for graceful shutdown.
- Cross-module communication goes through `smiths-core::EventBus`. No
  sibling crate should call another sibling crate directly.

## PR checklist

- [ ] `cargo fmt --check` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] `CHANGELOG.md` updated under the `[Unreleased]` section

## Where things live

| Area                  | Doc                                                  |
|-----------------------|------------------------------------------------------|
| Project goals + scope | `README.md`                                          |
| Release notes         | `CHANGELOG.md`                                       |

## Commit style

Short imperative subject ≤ 72 chars. Body explains the *why* when it is
not obvious. Reference phase number in the subject when relevant, e.g.
`phase-1: transaction FSM scaffolding`.

## License

By contributing you agree that your contribution is licensed under the
Apache License 2.0 — same as the project (`LICENSE`).
