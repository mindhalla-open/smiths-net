# smiths-net

**An AI-first SIP engine in Rust.** A single static binary that speaks
RFC 3261 signaling and RTP media — and lets an LLM *answer, route, and
bridge real phone calls* through an embedded [MCP][mcp] control plane.
Runs fully offline.

[![CI](https://github.com/mindhalla-open/smiths-net/actions/workflows/ci.yml/badge.svg)](https://github.com/mindhalla-open/smiths-net/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
![Rust 1.95+](https://img.shields.io/badge/rust-1.95%2B-orange.svg)
![Status: pre-1.0](https://img.shields.io/badge/status-pre--1.0-yellow.svg)

<!--
  ▶ HERO DEMO GOES HERE. Record it following docs/launch/demo-storyboard.md,
  save as docs/assets/demo.gif, then replace the line below with:
  <p align="center"><img src="docs/assets/demo.gif" alt="A local AI answering a live SIP call on smiths-net" width="760"></p>
-->
<p align="center"><em>▶ Demo clip coming soon — a fully local AI answering a live phone call (Whisper + llama.cpp + Silero, no cloud). See <a href="docs/launch/demo-storyboard.md">how it's made</a>.</em></p>

> **Status:** pre-1.0, under active development. The core ships and is
> exercised end-to-end — signaling over UDP/TCP/TLS, an RTP/SRTP/DTLS
> media bridge, WASM + sidecar plugins, the MCP control plane, and an HA
> Raft cluster (see the [CHANGELOG](CHANGELOG.md)). APIs still move and
> some media-plane polish is outstanding. Early, but real — try the demo.

## Why smiths-net

- **Agent-native.** An embedded [MCP][mcp] server exposes the engine to
  LLM agents as typed tools and resources — place a call, bridge legs,
  transcribe, synthesize — no glue code.
- **Local-first AI.** Speech-to-text, LLM, and text-to-speech are just
  capability plugins. Run Whisper + llama.cpp + Silero on one GPU and
  answer a phone line with **zero bytes to the cloud** — or point the
  same `ai.*` capabilities at OpenAI / Anthropic / Gemini.
- **One small static binary.** Target < 20 MB, < 64 MB RSS at idle;
  ships as a `scratch`-based multi-arch Docker image. No system deps.
- **Plugins in any language, one ABI.** Two tiers:
  - **WASM (wasmtime)** — sandboxed hot-path hooks; any WASM-targeting
    language (Rust, TinyGo, C, Zig).
  - **Sidecar (subprocess + protobuf IPC)** — control-plane and AI
    plugins; any language at all (Python, Node, Go, Java).
- **Rust core, `unsafe`-free by policy.** `unsafe_code = "deny"`
  workspace-wide; the media/crypto hot path is pure Rust (SRTP, DTLS).

## The flagship demo: an AI that answers the phone — offline

```
   PSTN / SIP trunk
        │  SIP + RTP (G.711, 8 kHz)
        ▼
   ┌──────────────┐     MCP (JSON-RPC)      ┌──────────────────────────┐
   │  smiths-net  │ ──────────────────────▶ │  AI sidecar plugins (GPU) │
   │   engine     │  transcribe / chat /    │  Whisper · llama.cpp ·    │
   │ SIP+RTP+MCP  │ ◀────────────────────── │  Silero  — 100% local     │
   └──────────────┘        synthesize        └──────────────────────────┘
```

It answers an inbound trunk call, transcribes with Whisper, asks a local
Gemma via llama.cpp, speaks the reply with Silero, handles barge-in and
speculative turn-taking, and hangs up on its own — on a single 12 GB GPU,
no cloud. Turnkey setup and walkthrough:
**[examples/README-asr-bot.md](examples/README-asr-bot.md)**.

## Quickstart

```bash
# Build the engine + a live-audio SIP client (needs Rust 1.95+)
cargo build --release
#   → target/release/smiths-net        (the engine)
#   → target/release/smiths-softphone  (a mic/speaker SIP client)
```

**Talk in 60 seconds — bridge two callers (no AI, no config):**

```bash
# Terminal A — start a local engine and join room "demo"
./target/release/smiths-softphone call --room demo --host

# Terminal B (another machine on your LAN) — join the same room.
# You're now bridged to A; talk to each other.
./target/release/smiths-softphone call --engine <A-LAN-IP>:5060 --room demo
```

*(Just checking audio? `smiths-softphone loopback` pipes your mic to your
speakers with no network.)*

**Run the engine on its own:**

```bash
./target/release/smiths-net init            # interactive wizard → config.toml
./target/release/smiths-net --config config.toml
```

**Or as a container:**

```bash
docker build -t smiths-net .    # ~20 MB static musl image on scratch
```

**Add the offline voice assistant** → follow
[examples/README-asr-bot.md](examples/README-asr-bot.md)
(`bash examples/setup-offline.sh` pulls the models, llama.cpp, and Gemma).

## Batteries included

The engine core is deliberately small; everything below ships as an
example plugin under [`plugins/examples/`](plugins/examples/):

| Capability | Example plugins |
|------------|-----------------|
| `ai.asr` — speech-to-text | `faster-whisper`, `whisper` |
| `ai.llm.chat` | `llama.cpp`, `Ollama`, `OpenAI`, `Anthropic` |
| `ai.tts` — text-to-speech | `Silero`, `Piper` |
| routing / dialplan | `dialplan-yaml`, `route-rhai`, `ivr-kit` |
| storage | `store-qdrant` (vector), `store-s3-recording` |
| integrations | `mqtt-bridge`, `ha-bridge`, `rust-logger` |

## Architecture & docs

- Design & specs: [`docs/architecture/`](docs/architecture/) — the
  [AI plugin protocol](docs/architecture/05-ai-plugin-protocol.md) is the
  place to start for AI work.
- Roadmap & scope: [`docs/plans/`](docs/plans/).
- Full change history: [`CHANGELOG.md`](CHANGELOG.md).

## Contributing

Issues and PRs welcome. Contributions are accepted under the
[Developer Certificate of Origin](https://developercertificate.org/):
sign off each commit with `git commit -s`. No CLA. See
[CONTRIBUTING.md](CONTRIBUTING.md).

## License

Licensed under the [Apache License, Version 2.0](LICENSE) — permissive,
with an explicit patent grant.

[mcp]: https://modelcontextprotocol.io
