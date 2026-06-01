# Examples

Ways to talk to (and through) a running `smiths-net` engine.

Start the engine first:

```bash
cargo run --release -- --config examples/config.toml
```

## Native softphone — talk through your computer

[`crates/smiths-softphone`](../crates/smiths-softphone) is a live-audio
SIP client: it captures your microphone, G.711-encodes it into RTP, and
plays the peer's audio back through your speakers. The "server" is
whoever runs the engine; everyone else just runs the softphone.

```bash
# validate your mic → speaker chain (no network)
cargo run -p smiths-softphone -- loopback

# two people, one room, bridged by the engine — you hear each other
cargo run -p smiths-softphone -- call --engine 127.0.0.1:5060 --room demo
# cross-machine: swap 127.0.0.1 for the host's LAN IP
```

Use headphones — there's no echo cancellation yet.

## Group calls — conference rooms

Set a conference prefix on the engine, then everyone who dials a room
under that prefix is mixed into one N-party call (leave-one-out mixer)
instead of a 2-peer bridge:

```bash
# enable conference rooms named conf-*
SMITHS__SIP__CONFERENCE_PREFIX=conf \
  cargo run --release -- --config examples/config.toml

# in three terminals (or on three machines):
cargo run -p smiths-softphone -- call --engine 127.0.0.1:5060 --room conf-standup
```

See `conference_prefix` in [`config.toml`](config.toml).

## Call-control brain — the WASM `sip-client` plugin

[`plugins/examples/sip-client`](../plugins/examples/sip-client) is a
sandboxed WASM plugin that places calls on the engine's behalf
(`smiths::originate`) and can react to inbound calls automatically. Wire
it to call events with `[plugins] call_event_hooks = ["sip-client"]` —
when a dialog goes live the engine invokes the plugin's
`on_dialog_created`. See the
[Call Control cookbook recipe](https://github.com/friday-mindhalla/smiths-net/tree/main/site/src/content/cookbook/wasm/rust/call-control).

## Firewall & ports

The engine listens on **UDP 5060** for SIP. By default it allocates RTP
on *ephemeral* ports, which is awkward to firewall. To pin media to a
known window, set a range in [`config.toml`](config.toml):

```toml
[media.rtp_ports]
min = 16384
max = 16484
```

Then open `udp/5060` and `udp/16384-16484` on the host. RTP uses even
ports, RTCP `port + 1`, so each call consumes two ports (~50 calls per
100-port window). The softphone's own RTP port can be pinned with
`--rtp-port <n>` if the caller side is also firewalled.

### Across NAT / the internet

If the engine is reachable (public IP, or port-forwarded) but a caller
is behind a home NAT, point the softphone at a STUN server so it
discovers and advertises its public address — the engine's return RTP
then traverses the NAT:

```bash
cargo run -p smiths-softphone -- call \
  --engine <engine-public-ip>:5060 --room demo \
  --stun stun.l.google.com:19302
```

This works for cone NATs (the STUN probe and RTP share one socket, so
they get the same public mapping). Symmetric NATs still need a relay
(TURN) or a VPN — put both machines on Tailscale / WireGuard and use the
LAN recipe.

## Other clients

| Path | What it is |
|------|------------|
| [`python-client/`](python-client) | Pure-stdlib Python SIP UAC + RTP toolkit, plus MCP / A2A control-plane demos. No Rust toolchain needed. |
| [`browser-webrtc/`](browser-webrtc) | Browser WebRTC ↔ SIP interop demo. |
| [`browser-webtransport/`](browser-webtransport) | Browser WebTransport signaling demo. |
| [`config.toml`](config.toml) | Annotated dev config — the canonical reference for every knob. |
