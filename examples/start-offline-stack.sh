#!/usr/bin/env bash
# Start llama.cpp LLM server (background) then ASR bot (foreground).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if ! pgrep -f 'llama-server.*8081' >/dev/null 2>&1; then
  echo "[offline] starting llama.cpp on :8081…"
  nohup bash examples/start-llamacpp.sh > /tmp/llamacpp.log 2>&1 &
  for i in $(seq 1 60); do
    curl -sf http://127.0.0.1:8081/health >/dev/null 2>&1 && break
    grep -q 'server is listening' /tmp/llamacpp.log 2>/dev/null && break
    sleep 2
  done
  echo "[offline] llama.cpp ready (log: /tmp/llamacpp.log)"
else
  echo "[offline] llama.cpp already running"
fi

exec bash examples/restart-asr-bot.sh
