# SIP parser fuzz harness

Built with [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz) +
`libfuzzer-sys`. Excluded from the main workspace because it requires a
nightly toolchain.

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
cargo +nightly fuzz run sip_parser
```

## Triage

A crash lands under `fuzz/artifacts/sip_parser/<id>`. Reproduce it from
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

Seed inputs live under `fuzz/corpus/sip_parser/`. Drop any real SIP
messages you want fuzzer mutations to start from there.
