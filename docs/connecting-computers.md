# Connecting two or more computers

How to talk between computers with the bundled **`smiths-softphone`**
client. The model is simple:

- **One computer runs the engine** (`smiths-net`). It's the "server" /
  switchboard — it answers calls and bridges or mixes their audio.
- **Every participant runs the softphone** and dials the **same room**
  on that engine. Two people on a room are bridged 1:1; three or more
  on a *conference room* are mixed together.

The host can also be a participant — it just dials its own engine.

> Use **headphones** on every machine. There's no echo cancellation
> yet, so open speakers will feed back.

---

## 0. Build (every machine, once)

```bash
git clone https://github.com/friday-mindhalla/smiths-net.git
cd smiths-net
cargo build --release
```

The two binaries you'll use:
- `target/release/smiths-net` — the engine (host only).
- `target/release/smiths-softphone` — the client (everyone).

Before involving anyone else, confirm your mic + speakers work:

```bash
cargo run --release -p smiths-softphone -- loopback
# speak — you should hear yourself. Ctrl-C to stop.
```

---

## One-command host (`--host`)

Don't want to start the engine separately? Pass `--host` to the
softphone — it launches the engine for you, waits until it's listening,
prints the address others should dial, then joins it. The engine is
shut down when you quit (Ctrl-C).

```bash
# you become the host AND a participant, in one command
cargo run --release -p smiths-softphone -- call --host --room demo
```

It prints something like:

```
Local engine is up on 0.0.0.0:5060.
Others on your network can join with:
  smiths-softphone call --engine 192.168.1.42:5060 --room demo
```

Everyone else just runs that line. `--host` enables conference rooms
(prefix `conf`) automatically, so `--room conf-anything` is a group
call. (`--host` needs the `smiths-net` binary next to `smiths-softphone`
or on your `PATH` — `cargo build --release` produces both.)

The rest of this guide is the manual, two-process version (run the
engine yourself), which gives you full control over `config.toml`.

## Scenario A — two people on the same network (LAN)

### 1. Host: start the engine

```bash
cargo run --release -- --config examples/config.toml
```

It listens on `0.0.0.0:5060` (all interfaces). Find the host's LAN IP:

| OS | Command |
|----|---------|
| macOS | `ipconfig getifaddr en0` |
| Linux | `hostname -I` (first address) |
| Windows | `ipconfig` → IPv4 Address |

Say it's **`192.168.1.50`**.

### 2. Both people: dial the same room

On each computer:

```bash
cargo run --release -p smiths-softphone -- \
  call --engine 192.168.1.50:5060 --room demo
```

When both are in, each prints `Connected. Media flows to …` and you can
talk. Press **Ctrl-C** to hang up.

That's it for the common case.

---

## Scenario B — three or more people (conference)

A 1:1 room bridges two callers. For a group call you need a **conference
room**, which mixes everyone (leave-one-out: each person hears all the
others).

### 1. Host: start the engine with a conference prefix

```bash
SMITHS__SIP__CONFERENCE_PREFIX=conf \
  cargo run --release -- --config examples/config.toml
```

Or set it permanently in `config.toml`:

```toml
[sip]
conference_prefix = "conf"
```

Any room whose name starts with `conf` is now a conference; everything
else still bridges 1:1.

### 2. Everyone: dial the same conference room

```bash
cargo run --release -p smiths-softphone -- \
  call --engine 192.168.1.50:5060 --room conf-standup
```

Every participant who dials `conf-standup` joins the same mix. Add or
drop people any time.

---

## Scenario C — computers on different networks (internet)

This needs more than a LAN. Pick whichever fits:

### Option 1 (easiest, most reliable): VPN

Put all machines on a single private network with **Tailscale** or
**WireGuard**, then use the **Scenario A/B** steps with the VPN IPs
(e.g. the host's `100.x.y.z` Tailscale address). Nothing else changes.

### Option 2: reachable engine + STUN

If the engine has a **public IP** (or you port-forward UDP 5060 + the
RTP range to it — see Ports below), callers behind a home NAT add a
STUN server so their public address is discovered and advertised:

```bash
cargo run --release -p smiths-softphone -- \
  call --engine <engine-public-ip>:5060 --room demo \
  --stun stun.l.google.com:19302
```

This works through typical home (cone) NATs. **Symmetric** NATs (some
corporate / carrier-grade setups) still won't traverse — fall back to
the VPN option. Full ICE/TURN for those is not yet implemented on the
SIP leg.

---

## Change your voice in real time

The softphone can modulate your **outgoing** audio so the other side
hears a different voice. Start with an effect:

```bash
smiths-softphone call --engine HOST:5060 --room demo --voice deep
```

…and switch it **live during the call** by typing a name and pressing
Enter:

```
deep      # lower pitch
high      # higher pitch
chipmunk  # much higher
robot     # metallic ring-mod
none      # back to your normal voice
```

Effects run per 20 ms frame and preserve call timing, so you can flip
between them mid-sentence. (Pitch shifting adds a little warble — it's a
fun real-time changer, not studio quality.)

## Ports & firewall

The engine uses:

- **UDP 5060** — SIP signaling.
- **RTP media** — by default *ephemeral* (random high) UDP ports, which
  are awkward to firewall. Pin them to a fixed window in `config.toml`:

  ```toml
  [media.rtp_ports]
  min = 16384
  max = 16484
  ```

  Then open **`udp/5060`** and **`udp/16384-16484`** on the host. RTP
  uses even ports and RTCP `port+1`, so each call needs two ports
  (~50 simultaneous legs per 100-port window).

On a trusted home LAN you usually only need to let the `smiths-net`
binary through the OS firewall. The caller's own RTP port can be pinned
with `--rtp-port <n>` if that side is firewalled too.

---

## Troubleshooting

| Symptom | Likely cause / fix |
|---------|--------------------|
| Softphone prints `Calling …` but never `Connected` | Engine not reachable. Check the IP/port, that the engine is running, and the host firewall allows UDP 5060. |
| Connected, but silence | RTP isn't getting back. On a LAN, check the firewall allows the RTP ports. Across NAT, add `--stun` or use a VPN. Confirm both sides aren't muted and are on headphones. |
| `STUN discovery failed … Can't assign requested address` | Your `--engine` is a loopback/unroutable address, so the RTP socket can't reach the internet. Use the real engine IP. |
| Echo / howling | Use headphones — there's no echo cancellation. |
| Third caller doesn't hear the others | You're on a plain room (1:1 bridge). Use a `conf…` room with `conference_prefix` set (Scenario B). |
| Choppy / robotic audio | Expected-ish at the edges: it's 8 kHz G.711 with a simple resampler. Wired headset + low-latency network helps. |

---

## Quick reference

```bash
# Host (switchboard)
smiths-net --config config.toml                         # 1:1 rooms
SMITHS__SIP__CONFERENCE_PREFIX=conf smiths-net --config config.toml   # + conferences

# Participant
smiths-softphone loopback                               # test mic/speaker
smiths-softphone call --engine HOST:5060 --room demo            # 1:1
smiths-softphone call --engine HOST:5060 --room conf-team       # conference
smiths-softphone call --engine HOST:5060 --room demo --stun stun.l.google.com:19302   # behind NAT
```

See also: [`examples/README.md`](../examples/README.md),
[`examples/config.toml`](../examples/config.toml).
