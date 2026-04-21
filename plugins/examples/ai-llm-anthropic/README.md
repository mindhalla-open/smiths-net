# ai-llm-anthropic

Reference `ai.llm.chat` sidecar that talks to Anthropic's Messages
API. Supports streaming partials via `ai.llm.partial` JSON-RPC
notifications.

## Setup

```bash
export ANTHROPIC_API_KEY=sk-ant-...
# Optional:
export ANTHROPIC_MODEL=claude-3-5-haiku-latest   # default
export ANTHROPIC_API_BASE=https://api.anthropic.com
export ANTHROPIC_VERSION=2023-06-01
export ANTHROPIC_MAX_TOKENS=1024
export ANTHROPIC_TIMEOUT_SECS=60
```

Drop this directory under `plugins.dir` and start `smiths-net`.
`list_ai_providers` shows `ai-llm-anthropic` once load succeeds.

## Priority

Advertises `priority = 16` — one step behind `ai-llm-openai` (15)
so the dispatcher picks OpenAI first and fails over to Anthropic on
error; still ahead of Ollama (20) and the mock (50). Edit the
descriptor to flip preferences.

## Streaming

Pass `"controls": {"stream": true}` to stream. Each Anthropic
`content_block_delta` becomes one `ai.llm.partial` notification;
the final RPC response carries the assembled message + token usage.
`message_delta` frames feed the output token count in real time.

## Message-shape translation

Anthropic's API wants `system` outside the `messages` array.
The sidecar collapses every `{"role": "system", ...}` turn into one
top-level `system` string (joined by blank lines) and strips them
from the `messages` payload. `tool` roles are dropped — wire them
up here if you need the structured-call path.

## Token accounting

`smiths_ai_tokens_total{provider="ai-llm-anthropic", dir="input"}`
and `dir="output"` tick on every response that carries usage —
streaming and blocking alike.
