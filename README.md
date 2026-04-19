# smiths-net

A lightweight, AI-first SIP engine with dynamic plugins in any language.

**Status**: design phase.

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
- **Capability-based plugins** — local AI (Whisper, Piper, Ollama) and
  cloud AI (OpenAI, Anthropic, Gemini) are both just plugins declaring
  `ai.*` capabilities. Local-first by default.
- **Pluggable storage** — SQL, KV, document, vector, time-series all
  behind the same trait set.

## License

Licensed under the [Apache License, Version 2.0](LICENSE) — permissive,
with an explicit patent grant.

Contributions are accepted under the [Developer Certificate of
Origin](https://developercertificate.org/): sign off each commit with
`git commit -s`. No CLA.
