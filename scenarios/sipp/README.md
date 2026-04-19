# SIPp load scenarios

Third-party load generator runs; not part of `cargo test`.

## Install

```sh
brew install sipp     # macOS
apt install sipp      # Debian/Ubuntu
```

## Prepare the engine

SIPp authenticates against a pre-provisioned account. Start the engine
with the `sipp` user in the credential store (the REGISTER integration
test already covers this pattern — mirror it in your `main.rs` when
benchmarking):

```rust
store.insert(Credentials {
    username: "sipp".into(),
    realm:    "smiths.test".into(),
    password: "s3cret".into(),
});
```

Then boot the engine on `127.0.0.1:5060/udp`.

## Target: 100 concurrent REGISTERs, median < 1 s

```sh
sipp -sf scenarios/sipp/register.xml \
     -s smiths.test \
     -r 100 -rp 1s \
     -l 100 \
     -m 100 \
     -t u1 \
     127.0.0.1:5060
```

Flags:

- `-s smiths.test` — Request-URI user-part (the `realm` the engine expects).
- `-r 100 -rp 1s` — arrival rate: 100 calls in 1 second.
- `-l 100` — allow up to 100 concurrent in-flight calls.
- `-m 100` — stop after 100 completed calls.
- `-t u1` — single UDP socket (mimics a real client stack).

Watch the **Response Time** histogram in SIPp's output — the gate is
median of the `1 REGISTER → 2 REGISTER` (the auth round-trip) under 1 s
with zero timeouts.

## Saving traces

```sh
sipp ... -trace_stat -stat_delimiter ,  # CSV stats
sipp ... -trace_err                     # per-call errors
sipp ... -trace_logs -log_file sipp.log # verbose log
```
