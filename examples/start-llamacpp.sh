#!/usr/bin/env bash
set -euo pipefail
BIN_DIR="/home/nikolas/.local/share/smiths-net/llama-bin"
MODEL="${LLAMACPP_MODEL_PATH:-/home/nikolas/.local/share/smiths-net/models/gemma-2-9b-it-Q4_K_M.gguf}"
PORT="${LLAMACPP_PORT:-8081}"
CTX="${LLAMACPP_CTX:-2048}"
# GPU offload: with the Vulkan build all layers fit on the RTX 4070 Ti (12 GB).
# -ngl 99 = offload every layer; falls back to CPU automatically if no GPU.
NGL="${LLAMACPP_NGL:-99}"

cd "$BIN_DIR"
# The Vulkan backend (libggml-vulkan.so) is loaded from BIN_DIR; make sure the
# co-located shared libs resolve.
export LD_LIBRARY_PATH="$BIN_DIR:${LD_LIBRARY_PATH:-}"

exec ./llama-server \
  -m "$MODEL" \
  --host 127.0.0.1 --port "$PORT" \
  -c "$CTX" \
  -ngl "$NGL" \
  -fa on \
  --cont-batching \
  --no-mmap \
  --keep -1
