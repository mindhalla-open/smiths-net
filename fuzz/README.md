# Fuzz harness

Built with [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz) +
`libfuzzer-sys`. Excluded from the main workspace because it requires a
nightly toolchain.

## Targets

| Target        | What it drives                                                    |
|---------------|-------------------------------------------------------------------|
| `sip_parser`  | Raw bytes through `rsip`'s message guard and the UAS request-summary / Via-branch parsers |
| `via_branch`  | Structured Via headers (compact names, folding, odd `branch` introductions, non-UTF-8) through the two UAS branch extractors and `rsip`'s typed `Via` |
| `sdes_crypto` | SDP `a=crypto:` parsing, standalone and inside a full session description |

The `fuzz-nightly` workflow runs each target for about five hours a
night, keeps the corpus in the Actions cache between runs, and fails
the job on any crash / timeout / OOM input.

## Install once

```sh
cargo install cargo-fuzz
rustup toolchain install nightly
```

## Run

```sh
# From the repo root. One-minute smoke:
cargo +nightly fuzz run sip_parser -- -max_total_time=60

# Continuous:
cargo +nightly fuzz run via_branch
```

## Triage

A crash lands under `fuzz/artifacts/<target>/<id>`. Reproduce it from
a normal build:

```sh
cargo +nightly fuzz run sip_parser fuzz/artifacts/sip_parser/<id>
```

Minimize a crash input:

```sh
cargo +nightly fuzz tmin sip_parser fuzz/artifacts/sip_parser/<id>
```

Lift the minimized bytes into a regular unit test once reproduced.

## Corpus

Seed inputs live under `fuzz/corpus/<target>/` (git-ignored). Drop any
real SIP messages you want fuzzer mutations to start from there.
