# ai-llm-ollama

Reference `ai.llm.chat` sidecar that delegates to a local
[Ollama](https://ollama.com/) daemon.

## Why it exists

Slice 3.1 adds the `AiDispatcher` and the `translate(text, to)` MCP
tool. Either of those is inert without a real LLM provider to dispatch
to. This plugin makes the "install the engine, pull a model, call
`translate` from your agent" path a three-command setup.

## Quick start

```bash
# 1. Install Ollama (macOS / Linux).
curl -fsSL https://ollama.com/install.sh | sh

# 2. Pull a small, fast model.
ollama pull llama3.2:3b

# 3. Make sure the daemon is up.
ollama serve      # leave this running, or use the systemd / launchd unit.

# 4. Drop this directory under your engine's plugins.dir, then start
#    smiths-net. `list_ai_providers` should show ai-llm-ollama.
```

## Configuration

Environment variables on the sidecar process:

| Variable              | Default                   | Notes                          |
| --------------------- | ------------------------- | ------------------------------ |
| `OLLAMA_HOST`         | `http://127.0.0.1:11434`  | Daemon base URL                |
| `OLLAMA_MODEL`        | `llama3.2:3b`             | Any tag you've pulled          |
| `OLLAMA_TIMEOUT_SECS` | `60`                      | HTTP read timeout              |

## Priority

The descriptor advertises `priority = 20` so the dispatcher picks
Ollama over the canned `ai-llm-mock` (which leaves the field at its
default `50`). Install both side-by-side and the engine will route
real traffic to Ollama while the mock remains as a fail-over.

## Limits / gotchas

* The sidecar is stdlib-only — no `requests`, no `httpx`. Large
  completions serialize through a single `urlopen`. If you need
  streaming, swap in a real HTTP client and upgrade the descriptor's
  `streaming.supported` flag to `true`.
* If the daemon is unreachable at invocation time, the sidecar returns
  a JSON-RPC error and the dispatcher fails over to the next candidate
  (see `smiths_ai_failovers_total`).
