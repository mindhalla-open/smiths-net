# HTTP/3 MCP + SIP-over-QUIC

Slice 4.3 / P17. This document describes how HTTP/3 (MCP) and
SIP-over-QUIC (signaling) fit into the engine, which parts ship in
0.45.0, and which parts are honestly deferred to a follow-on slice.

## What shipped in 0.45.0

1. **MCP HTTP/2.** The axum dependency was upgraded to enable the
   `http2` feature. When the MCP HTTP adapter is fronted by a TLS
   terminator that negotiates ALPN (nginx, Envoy, Caddy, Istio), the
   connection upgrades to `h2` transparently. HTTP/1.1 clients
   continue to work unchanged.

2. **`[mcp.http3]` config section.** New `enabled` + `bind` fields
   on `McpConfig::http3`. Accepted at config-load time regardless of
   feature flags; a warning is logged at startup when the runtime
   listener isn't wired.

3. **`sip.transports = ["...", "quic"]`** is now a valid
   enumeration value. With `--features sip-quic`, the CLI warns at
   startup that the listener isn't wired yet; without it, the value
   is ignored with a targeted warning.

4. **`--version` protocol advertisement.** `smiths-net --version`
   prints every supported transport + plugin tier + storage
   backend. Scaffolded protocols are flagged `(scaffold)` so
   operators can tell at a glance what's wired vs what's planned.

5. **`docs/architecture/07-http3.md`** (this file).

## What's deferred

The runtime listeners for both `mcp-http3` and `sip-quic` are
deferred. Why:

- **ALPN + certificate management.** `h3` over QUIC requires a
  TLS 1.3 server configuration. The engine already has an rcgen-
  based self-signed path for DTLS-SRTP (slice 1.3). Reusing that
  for h3 is possible but conflates signaling + media certs —
  operators typically want these separate. A follow-on slice adds
  a dedicated `[mcp.http3.tls]` cert path + SNI story.

- **Crate-host integration.** `quinn` + `h3-quinn` + `h3` wire to
  axum through `tower::Service`, not `axum::serve`. Writing the
  integration correctly (graceful shutdown, per-stream
  cancellation, 0-RTT replay safety) is ~200-400 LOC that
  deserves its own slice.

- **SIP-over-QUIC draft churn.** `draft-ietf-sipcore-sip-quic` is
  still evolving (handshake + stream model). Landing an
  interoperable implementation today ties us to a spec version
  that may shift. Scaffold now; wire runtime once the draft
  stabilizes or we have a concrete peer to test against.

## Why upgrade MCP to h2+h3

Three concrete wins, in order of impact:

1. **Head-of-line blocking.** An MCP agent that opens a streaming
   tool (slice 3.2's `ai.llm.partial` notifications, the SSE
   lifecycle stream) and a synchronous `tools/call` in parallel
   serializes them under h1. Under h2 they multiplex; under h3
   they avoid TCP-level HOL blocking on lossy mobile / wifi.

2. **Push notifications latency.** The MCP server pushes
   `notifications/call/created` + `notifications/plugin/<method>`
   frames. On an agent that already has h1 utilization for
   synchronous calls, those notifications queue behind the
   in-flight request. h2 splits them into their own stream.

3. **Multi-tenant hosting.** When operators put several smiths-
   net instances behind one HTTP endpoint, the load-balancer's h2
   / h3 connection pooling halves the TLS-handshake cost vs
   re-opening h1 per client.

For single-agent dev setups (one LLM host, one smiths-net
subprocess, stdio MCP) the upgrade is invisible. That's fine —
it's there for the tier that needs it without bloating the tier
that doesn't.

## SIP-over-QUIC specifically

The motivation is weaker than for MCP h3 but still real:

- **Mobile networks.** A 4G / 5G handset on a degraded connection
  can see 5-20% packet loss; a TCP SIP session's head-of-line
  blocking masks itself as intermittent INVITE timeouts. QUIC's
  per-stream loss recovery keeps REGISTER + OPTIONS flowing while
  one INVITE's bytes are retransmitting.

- **NAT traversal.** QUIC's connection-ID story is friendlier to
  NAT rebinding than TCP. A handset rolling between wifi and
  cellular keeps its SIP registration live.

- **0-RTT.** For polling OPTIONS / keepalives, 0-RTT resumption
  saves ~1 RTT per hit. Not enough to matter on LAN; meaningful
  on transatlantic links.

None of these is a must-have for 0.45.0 operators. That's why
`sip-quic` is a feature flag rather than a default.

## Rollout plan

| Release  | Scope                                                             |
|----------|-------------------------------------------------------------------|
| 0.45.0   | h2 shipped, h3 + QUIC scaffolds land (config + `--version`)       |
| 0.46.0*  | Landing the quinn+h3 runtime; `mcp-http3` flips to a real serve   |
| 0.47.0*  | SIP-over-QUIC runtime; `sip-quic` behaves as a real transport     |
| 0.48.0*  | WebTransport (P19) on top of h3                                   |

`*` = planned; exact ordering depends on slice 4.4 (P20 A2A) and
slice 4.5 landing first.

## Verifying what you have

Everything the binary knows it can do is in `--version`:

```text
$ smiths-net --version
smiths-net 0.45.0
sip transports: udp, tcp, tls, proxy: socks5 + http-connect
mcp adapters: stdio, http/1.1, http/2
storage: cdr+kv (sqlite), vector (memory), recording (fs), auth (sqlite+http)
ai: dispatcher, ollama/openai/anthropic refs, whisper.cpp ref, piper ref
plugin tiers: sidecar, wasm, script (rhai)
```

Add `--features mcp-http3` to the build and the `mcp adapters:`
line grows `, h3 (scaffold)`. Add `--features sip-quic` and the
SIP line grows `, quic (scaffold)`. The `(scaffold)` tag comes
off in the release where the runtime lands.
