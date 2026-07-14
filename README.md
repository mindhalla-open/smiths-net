# smiths-net

A lightweight, AI-first SIP engine with dynamic plugins in any language.

**Status**: pre-1.0, under active development. The core is implemented and
exercised end-to-end — RFC 3261 signaling (UDP/TCP/TLS), an RTP/SRTP/DTLS media
bridge, WASM + sidecar plugins, the MCP control plane, and an HA Raft cluster all
ship today (see [CHANGELOG.md](CHANGELOG.md)). Some media-plane polish — notably
an adaptive jitter buffer for playout endpoints — is still outstanding.

## What it is

A small Rust core that does RFC 3261 SIP signaling and RTP media, nothing
more. Everything else — routing, auth, AI features, storage — lives in
**plugins** loaded at runtime.

## Why it's different

- **Single static binary.** Target < 20 MB, < 64 MB RSS at idle.
- **Two-tier plugins, one ABI**:
  - **WASM (wasmtime)** — sandboxed hot-path hooks; any WASM-targeting
    language (Rust, TinyGo, C, Zig).
  - **Sidecar (subprocess + protobuf IPC)** — control-plane and AI
    plugins; any language at all (Python, Node, Go, Java).
- **AI-first control plane** — embedded [MCP][mcp] server: LLM agents
  drive the engine through typed tools and resources.
- **Capability-based plugins** — local AI (Whisper, Piper, Ollama,
  llama.cpp) and cloud AI (OpenAI, Anthropic, Gemini) are both just
  plugins declaring `ai.*` capabilities. Local-first by default.
- **Pluggable storage** — SQL, KV, document, vector, time-series all
  behind the same trait set.

## License

Licensed under the [Apache License, Version 2.0](LICENSE) — permissive,
with an explicit patent grant.

Contributions are accepted under the [Developer Certificate of
Origin](https://developercertificate.org/): sign off each commit with
`git commit -s`. No CLA.
