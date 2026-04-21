# ai-asr-whisper

Reference `ai.asr` sidecar that shells out to
[whisper.cpp](https://github.com/ggerganov/whisper.cpp) — local
CPU/GPU speech-to-text from GGML model files.

## Setup

```bash
# 1. Build whisper.cpp:
git clone https://github.com/ggerganov/whisper.cpp && cd whisper.cpp
make -j
# The binary name differs across releases — newer builds ship
# `whisper-cli`; older ones `main`. Both work here.

# 2. Pull a model. `base.en` (~140 MB) is a good default for
#    English; `small` / `medium` for multilingual accuracy.
bash ./models/download-ggml-model.sh base.en
# Resulting file: models/ggml-base.en.bin

# 3. Point the sidecar at the binary + model.
export WHISPER_BIN=/path/to/whisper.cpp/build/bin/whisper-cli
export WHISPER_MODEL=/path/to/whisper.cpp/models/ggml-base.en.bin
```

Drop this directory under `plugins.dir` and start `smiths-net`.
`list_ai_providers` shows `ai-asr-whisper`.

## Configuration

| Variable           | Default         | Notes                                |
| ------------------ | --------------- | ------------------------------------ |
| `WHISPER_BIN`      | `whisper-cli`   | Binary name or full path             |
| `WHISPER_MODEL`    | _(unset)_       | GGML model path (required at call)   |
| `WHISPER_THREADS`  | `4`             | Passed as `-t`                       |
| `WHISPER_LANG`     | `auto`          | Default language; `-l` is set when ≠ auto |

## Priority

Advertises `priority = 20` so the dispatcher picks Whisper over the
canned `ai-asr-mock` (50). The 15-19 range is reserved for cloud
ASRs if/when they land.

## I/O shape

Accepts base64-encoded PCM16 LE; `sample_rate` is honored (8 kHz is
crude-upsampled to 16 kHz, which is all whisper.cpp accepts). Each
call writes a temp WAV for whisper.cpp's `-f` flag, reads its JSON
output, and returns `{text, language, confidence, segments}`.

## Limits / gotchas

* Whisper is launched per-call (`subprocess.run`). Each call pays
  model-load latency (~200-500 ms for `base`, seconds for larger
  models). A production plugin should keep whisper.cpp running
  with streaming input; that path is a follow-on.
* 8 kHz → 16 kHz resample is nearest-neighbor sample doubling.
  Telephony-grade quality for speech; anything else should ship a
  proper resampler.
* The `WHISPER_MODEL` env var must be set at invocation time. The
  descriptor still loads when the model file is missing so
  `list_ai_providers` stays honest; `transcribe` returns a
  JSON-RPC error and the dispatcher fails over to the next
  `ai.asr` candidate.
