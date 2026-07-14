#!/usr/bin/env bash
# Restart the ASR bot (Megafon trunk mode) — foreground, logs to the terminal.
#
#   bash examples/restart-asr-bot.sh
#
# Stops the running instance (if any) and starts a new one.
# Output is also duplicated to /tmp/asr-bot.log (or ASR_BOT_LOG=...).
# Ctrl+C — stop the bot.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

LOG="${ASR_BOT_LOG:-/tmp/asr-bot.log}"
# Match the script regardless of interpreter (python3 vs .venv/bin/python),
# otherwise stop_bot misses venv-launched instances and they pile up.
PATTERN='examples/python-client/asr_bot.py --mode trunk'
ENGINE_PATTERN='target/release/smiths-net --config examples/multifon.toml'

ensure_llamacpp() {
  local host="${LLAMACPP_HOST:-http://127.0.0.1:8081}"
  local port="${host##*:}"
  port="${port%%/*}"

  if curl -sf "${host%/}/health" >/dev/null 2>&1; then
    echo "[restart] llama.cpp already running at $host"
    return 0
  fi

  if [[ "${OFFLINE:-0}" != "1" && -z "${LLAMACPP_HOST:-}" ]]; then
    return 0
  fi

  if [[ ! -x "${HOME}/.local/share/smiths-net/llama-bin/llama-server" ]]; then
    echo "[restart] WARN: llama.cpp not installed — LLM calls will fail" >&2
    echo "[restart]       run: bash examples/setup-offline.sh" >&2
    return 0
  fi

  echo "[restart] starting llama.cpp on :${port}…"
  nohup bash examples/start-llamacpp.sh > /tmp/llamacpp.log 2>&1 &
  for _ in $(seq 1 45); do
    if curl -sf "${host%/}/health" >/dev/null 2>&1; then
      echo "[restart] llama.cpp ready (log: /tmp/llamacpp.log)"
      return 0
    fi
    if grep -qE 'error|failed|cannot' /tmp/llamacpp.log 2>/dev/null; then
      echo "[restart] WARN: llama.cpp failed to start — see /tmp/llamacpp.log" >&2
      return 1
    fi
    sleep 2
  done
  echo "[restart] WARN: llama.cpp not ready after 90s — see /tmp/llamacpp.log" >&2
  return 1
}

sync_public_ip() {
  # OPT-IN ONLY (ENABLE_IP_SYNC=1). Auto-detecting the public IP via ifconfig.me
  # is WRONG on this deployment: the router has a port-forwarded public IP
  # (176.115.151.161) that Megafon reaches, while ifconfig.me reports the
  # carrier's CGNAT egress (79.127.255.77) where inbound never arrives. Letting
  # the detector overwrite the configured address broke inbound calling. So the
  # pinned SIP_PUBLIC_ADDRESS / advertise_ip win unless you explicitly opt in on
  # a network where ifconfig.me really equals the inbound WAN.
  if [[ "${ENABLE_IP_SYNC:-0}" != "1" ]]; then
    return 0
  fi
  local ip=""
  for u in "https://api.ipify.org" "https://ifconfig.me" "https://icanhazip.com"; do
    ip="$(curl -s --max-time 8 "$u" 2>/dev/null | tr -d '[:space:]')"
    [[ "$ip" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] && break || ip=""
  done
  if [[ -z "$ip" ]]; then
    echo "[restart] WARN: could not detect public IP — keeping configured address" >&2
    return 0
  fi

  local env_file="examples/multifon.env"
  if [[ -f "$env_file" ]] && ! grep -q "^SIP_PUBLIC_ADDRESS=$ip$" "$env_file"; then
    sed -i -E "s/^SIP_PUBLIC_ADDRESS=.*/SIP_PUBLIC_ADDRESS=$ip/" "$env_file"
    echo "[restart] updated SIP_PUBLIC_ADDRESS → $ip"
  fi
  local toml="examples/multifon.toml"
  if [[ -f "$toml" ]] && ! grep -q "^advertise_ip = \"$ip\"$" "$toml"; then
    sed -i -E "s/^advertise_ip = \".*\"/advertise_ip = \"$ip\"/" "$toml"
    echo "[restart] updated advertise_ip → $ip"
  fi
}

stop_bot() {
  local stopped=0
  if pgrep -f "$PATTERN" >/dev/null 2>&1; then
    stopped=1
  fi
  if pgrep -f "$ENGINE_PATTERN" >/dev/null 2>&1; then
    stopped=1
  fi
  if [[ "$stopped" -eq 0 ]]; then
    echo "[restart] not running"
    return 0
  fi

  echo "[restart] stopping…"
  pkill -f "$PATTERN" 2>/dev/null || true
  pkill -f "$ENGINE_PATTERN" 2>/dev/null || true

  for _ in $(seq 1 20); do
    pgrep -f "$PATTERN" >/dev/null 2>&1 || break
    sleep 0.5
  done

  if pgrep -f "$PATTERN" >/dev/null 2>&1 || pgrep -f "$ENGINE_PATTERN" >/dev/null 2>&1; then
    echo "[restart] force kill"
    pkill -9 -f "$PATTERN" 2>/dev/null || true
    pkill -9 -f "$ENGINE_PATTERN" 2>/dev/null || true
    sleep 1
  fi

  # Let OS release RTP sockets (engine drain_timeout_secs ≈ 10).
  echo "[restart] waiting for RTP port release…"
  sleep 3

  echo "[restart] stopped"
}

start_bot() {
  if [[ ! -f target/release/smiths-net && ! -f target/debug/smiths-net ]]; then
    echo "[restart] smiths-net not found — build first:" >&2
    echo "  cd $ROOT && CARGO_TARGET_DIR=target cargo build --release -p smiths-cli" >&2
    exit 1
  fi

  # Refresh the public IP in the config files *before* sourcing them, so the
  # bot and engine both pick up the current address (dynamic-IP self-heal).
  sync_public_ip

  for env_file in examples/multifon.env examples/asr-bot.env examples/offline.env; do
    if [[ -f "$env_file" ]]; then
      set -a
      # shellcheck disable=SC1090
      source "$env_file"
      set +a
    fi
  done

  ensure_llamacpp || true

  # Put the project venv first on PATH so the engine's Python sidecar
  # plugins (which start via `#!/usr/bin/env python3`) resolve the venv
  # interpreter with torch / faster-whisper / silero installed — without hardcoding
  # an absolute interpreter path in the committed plugin shebangs.
  local venv="$ROOT/.venv"
  local py="python3"
  if [[ -x "$venv/bin/python" ]]; then
    export VIRTUAL_ENV="$venv"
    export PATH="$venv/bin:$PATH"
    py="$venv/bin/python"
    echo "[restart] using venv: $venv"
  fi

  echo "[restart] starting (foreground, log also → $LOG)"
  echo "[restart] Ctrl+C to stop"
  echo "---"

  env PYTHONUNBUFFERED=1 "$py" examples/python-client/asr_bot.py \
    --mode trunk \
    --config examples/multifon.toml \
    --bot-config examples/bot.toml \
    2>&1 | tee "$LOG"
}

stop_bot
start_bot
