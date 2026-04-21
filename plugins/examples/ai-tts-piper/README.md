# ai-tts-piper

Reference `ai.tts` sidecar that shells out to
[Piper](https://github.com/rhasspy/piper) — Rhasspy's fast neural TTS
that runs entirely on CPU from a local ONNX voice file.

## Why it exists

Slice 3.1 ships the `AiDispatcher`, the `translate` tool, and the
`ai-llm-ollama` sidecar. For a full "translate + speak it back into
the call" demo, the engine needs a real TTS too. Piper is small,
fast, and license-friendly for local deployment.

## Quick start

```bash
# 1. Install Piper. Easiest via pip:
pip install piper-tts

# 2. Grab a voice (example: en_US-lessac-medium):
mkdir -p ~/piper-voices && cd ~/piper-voices
wget https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/lessac/medium/en_US-lessac-medium.onnx
wget https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/lessac/medium/en_US-lessac-medium.onnx.json

# 3. Tell the sidecar where to find them.
export PIPER_VOICE=~/piper-voices/en_US-lessac-medium.onnx
export PIPER_VOICE_ID=lessac
export PIPER_LANG=en-US

# 4. Drop this directory under your engine's plugins.dir and start
#    smiths-net. `list_ai_providers` should show ai-tts-piper.
```

## Configuration

| Variable         | Default     | Notes                            |
| ---------------- | ----------- | -------------------------------- |
| `PIPER_BIN`      | `piper`     | Path to the Piper executable     |
| `PIPER_VOICE`    | _(unset)_   | Full path to the `.onnx` voice   |
| `PIPER_VOICE_ID` | `default`   | Voice id advertised in descriptor|
| `PIPER_LANG`     | `en`        | BCP-47 tag for the voice         |

## Priority

The descriptor advertises `priority = 20` so the dispatcher picks
Piper over the canned `ai-tts-mock` (which stays at the default `50`).
Install both side-by-side — when Piper errors out the dispatcher
fails over to the mock and bumps `smiths_ai_failovers_total`.

## Limits / gotchas

* Piper is launched per-call (`subprocess.run`), not persistently.
  First-call latency dominates (~300 ms on M-series Mac for short
  inputs); follow-on calls without warmup hit the same cold start.
  A production plugin should keep Piper running and stream stdin /
  stdout, at which point you can flip the descriptor's
  `streaming.supported` to `true`.
* Resampling from Piper's native 22.05 kHz down to 8 kHz (for PCMU
  calls) is crude decimation. Good enough for development; swap in a
  real resampler before shipping.
