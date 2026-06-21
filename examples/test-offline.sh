#!/usr/bin/env bash
# Smoke-test the offline stack: faster-whisper, Silero, llama.cpp.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
set -a; source examples/offline.env; set +a
PY="$ROOT/.venv/bin/python"

echo "=== STT: faster-whisper ==="
"$PY" -c "from faster_whisper import WhisperModel; WhisperModel('${FW_MODEL:-large-v3}', device='${FW_DEVICE:-cuda}', compute_type='${FW_COMPUTE_TYPE:-int8_float16}'); print('faster-whisper OK')"

echo "=== TTS: Silero ==="
REQ='{"jsonrpc":"2.0","id":1,"method":"synthesize","params":{"text":"Привет","output":{"codec":"pcm_s16le","sample_rate":8000}}}'
echo "$REQ" | "$PY" plugins/examples/ai-tts-silero/main.py | "$PY" -c "import json,sys; r=json.load(sys.stdin); print('Silero OK:', r['result']['duration_ms'], 'ms')"

echo "=== LLM: llama.cpp Gemma ==="
curl -sf "$LLAMACPP_HOST/v1/chat/completions" \
  -H 'Content-Type: application/json' \
  -d '{"model":"gemma","messages":[{"role":"user","content":"Скажи одно слово: да"}],"max_tokens":16}' \
  | "$PY" -c "import json,sys; d=json.load(sys.stdin); print('LLM OK:', d['choices'][0]['message']['content'][:80])"

echo "=== All offline components OK ==="
