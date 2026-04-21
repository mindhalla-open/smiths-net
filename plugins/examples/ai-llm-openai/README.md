# ai-llm-openai

Reference `ai.llm.chat` sidecar that talks to OpenAI's Chat
Completions API. Supports streaming partials via `ai.llm.partial`
JSON-RPC notifications.

## Setup

```bash
export OPENAI_API_KEY=sk-...
# Optional:
export OPENAI_MODEL=gpt-4o-mini        # default
export OPENAI_API_BASE=https://api.openai.com
export OPENAI_TIMEOUT_SECS=60
```

Drop this directory under `plugins.dir` and start `smiths-net`.
`list_ai_providers` shows `ai-llm-openai` once load succeeds.

## Priority

Advertises `priority = 15` — the dispatcher prefers OpenAI over
Ollama (20) and the mock (50). Flip the order by editing the
descriptor if your deployment budgets latency over model quality.

## Streaming

Pass `"controls": {"stream": true}` to any `llm_chat` / `translate`
tool call that routes here. The sidecar issues one
`ai.llm.partial` notification per token delta, then a final-marker
partial with `"is_final": true`. The blocking `chat` RPC still
returns with the full assembled message + token usage. Clients that
don't care about streaming can ignore the notifications.

## Limits

* Stdlib-only — no `openai` or `httpx` dependency. Large
  completions block a single `urlopen`; streaming parses SSE
  line-by-line with `urlopen`'s iterator.
* No request retries. The engine's `AiDispatcher` fails over to the
  next `ai.llm.chat` provider on any error or timeout.
* `OPENAI_API_KEY` is redacted when dumped via the `ai.openai_api_key`
  config path, but the env var itself is visible in /proc on Linux.
  Prefer running the engine under a dedicated user if that matters.

## Token accounting

The sidecar surfaces `usage.{input,output}_tokens` on every
response (streaming + blocking). The `AiDispatcher` credits these to
`smiths_ai_tokens_total{provider="ai-llm-openai", dir=...}` — divide
by wall-clock for tokens/sec, or multiply by the vendor rate card
for `$/day` cost.
