# llama.cpp LLM (offline Gemma / Llama)

Local LLM via [llama.cpp](https://github.com/ggerganov/llama.cpp) `llama-server`
OpenAI-compatible API. Fully offline once the GGUF model is loaded.

## Install llama.cpp

```bash
# Ubuntu — build from source or use prebuilt:
git clone https://github.com/ggerganov/llama.cpp
cd llama.cpp && cmake -B build && cmake --build build -j
```

## Download Gemma 3 (recommended for Russian)

```bash
# Gemma 3 4B Instruct — good balance of speed and quality on CPU/GPU:
# https://huggingface.co/models?search=gemma-3-4b-it-gguf
wget -O gemma-3-4b-it-Q4_K_M.gguf \
  "https://huggingface.co/.../gemma-3-4b-it-Q4_K_M.gguf"
```

Other options: `gemma-2-2b-it-Q4_K_M`, `Llama-3.2-3B-Instruct-Q4_K_M`.

## Start server

```bash
./build/bin/llama-server \
  -m /path/to/gemma-3-4b-it-Q4_K_M.gguf \
  --host 127.0.0.1 --port 8080 \
  -c 4096 \
  -ngl 99          # GPU layers; use 0 for CPU-only
```

Verify:

```bash
curl http://127.0.0.1:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"gemma","messages":[{"role":"user","content":"Привет"}]}'
```

## Bot configuration

```bash
OFFLINE=1
LLM_PLUGIN=ai-llm-llamacpp
LLAMACPP_HOST=http://127.0.0.1:8080
LLAMACPP_MODEL=gemma
LLM_MAX_TOKENS=128
```

See `examples/offline.env.example` for the full offline stack (Vosk + Silero + llama.cpp).

## vs Ollama

|                | llama.cpp server     | Ollama              |
|----------------|----------------------|---------------------|
| Control        | Full (flags, quant)  | Managed daemon      |
| Gemma support  | Direct GGUF          | `ollama pull gemma3:4b` |
| Plugin         | `ai-llm-llamacpp`    | `ai-llm-ollama`     |
| Network        | None (offline)       | None (offline)      |

Use Ollama if you prefer `ollama pull` workflow; use llama.cpp for direct GGUF control.
