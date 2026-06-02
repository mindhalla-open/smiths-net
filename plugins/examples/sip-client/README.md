# sip-client — WASM call-control plugin

The **brain** half of the SIP-client work. It decides *when* to place a
call and asks the engine to do it through the `smiths::originate` host
function. It carries **no media**: a WASM sandbox has no sockets and no
audio devices, so the actual voice is handled by the native
[`smiths-softphone`](../../../crates/smiths-softphone) client. Think of
this plugin as the dialer/orchestrator and the softphone as the handset.

## Capability

Advertises `routing.dial`. Invoke it with the target SIP URI as
`params`:

| Method | Params                          | Effect                                  |
|--------|---------------------------------|-----------------------------------------|
| `dial` | `"sip:bob@host:5060"` (string)  | Calls `smiths::originate(target)`; returns `{"result":"dialing"}`. |

The call is dispatched fire-and-forget — `originate` returns
immediately while the engine's UAC runs the INVITE round-trip. Watch
engine logs / call events for the outcome and the allocated Call-ID.

## Permissions

```toml
permissions = ["send_sip"]   # unlocks smiths::originate / smiths::hangup
```

Without `send_sip`, the engine traps the guest's first `originate` call
with `PermissionDenied { permission: "send_sip" }`.

## Build

```bash
rustup target add wasm32-unknown-unknown   # once
cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/sip_client.wasm ./sip_client.wasm
```

`plugin.toml`'s `entry = "./sip_client.wasm"` is resolved relative to
this directory, so the copy step puts the artifact where the loader
looks. A prebuilt `sip_client.wasm` is checked in for convenience.

## Try it

Point the engine's plugin dir at the examples folder and confirm it
loads:

```bash
SMITHS__PLUGINS__DIR=plugins/examples \
  cargo run -p smiths-cli --bin smiths-net -- --config examples/config.toml
# look for: "plugin loaded" plugin=sip-client
```

Then drive `routing.dial` via the MCP `make_call`-style tooling, or from
another plugin. The dialed leg can be bridged to a `smiths-softphone`
caller on the same room for end-to-end audio.
