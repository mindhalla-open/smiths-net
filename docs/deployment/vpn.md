# WireGuard co-deployment

Slice 3.5 ships SOCKS5 + HTTP-CONNECT wrappers for the engine's
outbound TCP path. That's enough for "my enterprise egress is a
SOCKS proxy" scenarios. For *mesh* deployments — engines in separate
networks that need to reach each other over an encrypted overlay —
the right tool is WireGuard.

This doc covers two shapes:

1. **Sidecar WireGuard** (recommended today) — the engine runs
   unmodified; the host (or its pod) carries a WireGuard interface
   that it binds against or routes through.
2. **Embedded WireGuard** (optional, `wireguard` Cargo feature) —
   the engine initializes a `boringtun` userspace device and runs
   SIP through it without a host-level interface. Useful on
   stripped-down hosts where operators can't install a kernel
   module or root-level tooling. Config surface lands in 0.42.0;
   runtime plumbing is a dedicated follow-on.

## Why WireGuard, not IPsec or OpenVPN

Three reasons:

- **Minimal attack surface.** Modern crypto, ~4k lines of kernel
  code, one transport protocol. Every other VPN stack we've looked
  at comes with dialects the SIP engine doesn't need.
- **Deterministic NAT behaviour.** WireGuard's roaming model
  (clients update their own endpoints) keeps SIP's `Contact:`
  rewriting story simpler than IPsec's.
- **Operator familiarity.** `wg-quick` ships on every recent
  Linux; kernel module on 5.6+, userspace (`wireguard-go`) for
  BSDs / macOS / Windows. Ubiquitous.

## Option 1 — Sidecar WireGuard (host-level)

### Host (Debian / Ubuntu)

```bash
apt-get install -y wireguard
ip link add wg0 type wireguard
wg set wg0 \
  private-key <(cat /etc/smiths-net/wg.key) \
  listen-port 51820 \
  peer <PEER_PUB_KEY> allowed-ips 10.42.0.0/24 endpoint <PEER_IP>:51820
ip addr add 10.42.0.2/24 dev wg0
ip link set wg0 up
```

### Engine config

Bind SIP to the WireGuard address so peers on the overlay can reach
it; keep `0.0.0.0` if you want external reachability too.

```toml
[sip]
bind = ["10.42.0.2:5060"]
```

Optional: combine with the slice-3.5 outbound proxy for egress
through a shared SOCKS5 relay on the overlay.

```toml
[sip.proxy]
mode    = "socks5"
address = "10.42.0.1:9050"    # SOCKS5 proxy on the overlay's gateway
```

### Kubernetes (WireGuard sidecar pattern)

Stand the WG interface up with an init container, share the
network namespace with the engine. Example manifest lives at
`k8s/wireguard-sidecar.yaml` (follow-on).

Gotchas:

- `/health` and `/metrics` need a separate bind (loopback or the
  pod's cluster IP) — operators monitoring the engine typically
  don't route over the WireGuard overlay.
- Drain timeout (`[sip] drain_timeout_secs`) should exceed the
  WireGuard rekey window (default 120 s) so mid-rekey dialogs
  don't land on a stale endpoint.

## Option 2 — Embedded `boringtun` (feature `wireguard`, 0.42.0+)

```toml
# Cargo
smiths-cli = { version = "0.42", features = ["wireguard"] }
```

```toml
# engine config
[sip.vpn]
mode            = "wireguard"
private_key     = "...="
peer_public_key = "...="
peer_endpoint   = "203.0.113.7:51820"
allowed_ips     = ["10.42.0.0/24"]
interface_ip    = "10.42.0.5/24"
```

When the engine starts it creates an in-process `boringtun` device,
binds SIP against the embedded interface, and routes every outbound
SIP packet through the tunnel. No host root, no `wg-quick`.

**Status today.** Slice 3.5 adds the config scaffold and reserves
the `wireguard` feature flag; the runtime device is a dedicated
follow-on (it needs tun/tap support which is macOS / Linux only and
bleeds into the host's routing table). Operators on 0.42.0 who set
`mode = "wireguard"` get a clean warning + the engine falls back to
`mode = "none"` at startup.

## Picking between the two

| | Sidecar WG | Embedded (`boringtun`) |
|---|---|---|
| Needs host root / kernel module | yes | no |
| Multiple engine pods on one host share one WG interface | yes | no (each has its own) |
| Cross-platform (macOS, BSD) | via `wireguard-go` | via `boringtun` (native) |
| Operator-managed key rotation | `wg set` | restart engine |
| Traffic shaping / policy routing | host-level (`iptables`, `tc`) | engine-level only |

For production deployments, default to the sidecar pattern. The
embedded feature exists for appliances / dev / demos where a single
self-contained binary wins.

## Verifying the tunnel

A proxy / VPN misconfiguration usually masks itself as a SIP `408
Request Timeout`. Quick ladder:

1. `wg show` → confirms the peer's handshake completed.
2. `nc -vz 10.42.0.1 5060` → confirms TCP reachability.
3. `smiths-cli ... --config ... --mcp stdio` and call `health` →
   confirms the engine itself is alive.
4. `metrics` at `/metrics` → `sip_responses_total` increments under
   test traffic.

If the first two succeed but 3/4 don't tick, it's an engine config
issue, not a VPN one.
