# Python client sample

A tiny SIP UAC + RTP toolkit in **pure Python 3.9+**, using only the
standard library. Mirrors what `crates/smiths-testkit` does in Rust, so
you can poke the engine from any machine on your LAN without touching
the Rust toolchain.

## Two ways to talk to the engine

The engine now exposes **two control-plane adapters in addition to SIP**:

- **MCP stdio** — one process pair: an LLM host (Claude Code, Cursor,
  or `mcp_demo.py`) spawns the engine with `--mcp stdio` and exchanges
  JSON-RPC 2.0 frames over stdin/stdout.
- **A2A HTTP** — long-running engine, clients post JSON-RPC to
  `/a2a`, discover capabilities via `/.well-known/agent.json`.

Both adapters serve the **same tool set**: `list_calls`,
`get_call_status`, `health`. Media — when it flows — always rides SIP +
RTP. MCP/A2A are the control plane, not the media plane.

Run the control-plane demos alongside the SIP demos to see both
halves.

## Requirements

- Python **≥ 3.9**. Nothing to install — only `socket`, `wave`, `struct`,
  `math`, `threading`, `random` from the standard library.
- A running `smiths-net` engine reachable at a known SIP `host:port`
  (default is `0.0.0.0:5060` from `examples/config.toml`).

```bash
# Terminal 1 — run the engine
cargo build --release
./target/release/smiths-net --config examples/config.toml
```

## What's in here

| File               | Purpose                                                      |
| ------------------ | ------------------------------------------------------------ |
| `smiths_client.py` | `SipUAC` class, μ-law codec, RTP packet builder, WAV I/O.    |
| `demo_call.py`     | Two UACs in one process — A plays a sine wave, B records it. |
| `speaker.py`       | Standalone UA that streams a WAV (or a generated sine) in.   |
| `listener.py`      | Standalone UA that records received RTP to a WAV.            |
| `mcp_demo.py`      | Spawns the engine in `--mcp stdio` mode, walks MCP handshake + tool calls. |
| `a2a_demo.py`      | Talks to the A2A HTTP endpoint (same tool set, JSON-RPC over HTTP). |
| `voice_agent.py`   | Full voice-agent demo: MCP notifications + STT/LLM/TTS on bridged RTP. |
| `voice_caller.py`  | Simulated inbound caller that dials the voice agent. |

## Where files live

The repo has a project-local `tmp/` folder (listed in `.gitignore`) for
throwaway audio. Scripts default to `tmp/…` **relative to the current
working directory**, so run them from the repo root:

```bash
cd /path/to/smiths-net
python3 examples/python-client/demo_call.py   # writes tmp/smiths-py-received.wav
```

If you prefer absolute paths pass `--out /anywhere/you/like.wav`.

## Sample voice files

A few ready-made WAVs produced by macOS `say` live in `tmp/` after
running:

```bash
say --file-format=WAVE --data-format=LEI16@8000 \
    -o tmp/smiths-hello.wav \
    "Hello. This is a test call from the Python client to the smiths engine."
```

Any mono 16-bit 8 kHz WAV works — those are the only files
`read_wav_mono_pcm16_8k` accepts (forced by the engine's PCMU @ 8 kHz
codec). Convert anything else with:

```bash
ffmpeg -i any.mp3 -ar 8000 -ac 1 -sample_fmt s16 tmp/my-input.wav
```

## Quick start — one-process demo

The engine must be running on `127.0.0.1:5060`.

```bash
# 1 kHz generated sine tone (no WAV needed):
python3 examples/python-client/demo_call.py
# → writes tmp/smiths-py-received.wav

# Stream a real voice WAV:
python3 examples/python-client/demo_call.py --wav tmp/smiths-hello.wav \
  --out tmp/smiths-hello-received.wav

afplay tmp/smiths-hello-received.wav     # macOS
aplay  tmp/smiths-hello-received.wav     # Linux
```

You'll hear the input audio round-tripped through the engine's
rendezvous bridge, with the slight G.711 / 8 kHz "phone call" timbre
introduced by μ-law encoding.

## Control-plane demos (MCP + A2A)

### MCP stdio

```bash
cargo build --release
python3 examples/python-client/mcp_demo.py
```

The demo spawns `target/release/smiths-net --mcp stdio` as a subprocess
and talks JSON-RPC 2.0 over its stdin/stdout. Output shows the
`initialize` handshake, `tools/list`, and a few `tools/call` invocations.

This is the same wire an LLM host (Claude Code) uses. To wire it into
Claude Code, add to its MCP config:

```jsonc
{
  "mcpServers": {
    "smiths-net": {
      "command": "/absolute/path/to/target/release/smiths-net",
      "args": ["--config", "/absolute/path/to/examples/config.toml",
               "--mcp", "stdio"]
    }
  }
}
```

### Voice agent (MCP notifications + STT → LLM → TTS)

The biggest demo — a Python "voice agent" that spawns the engine,
subscribes to MCP push notifications, acts as the SIP callee through
`SipUAC`, and runs a full STT → LLM → TTS pipeline against the
bridged RTP.

```bash
cargo build --release

# Terminal 1 — agent (it spawns smiths-net internally)
python3 examples/python-client/voice_agent.py

# Terminal 2 — simulated caller
python3 examples/python-client/voice_caller.py \
    --wav tmp/smiths-hello.wav \
    --out tmp/voice-agent-reply.wav

afplay tmp/voice-agent-reply.wav   # "Алло, Алиса слушает вас"
```

Architecture honest-note:

- **Real today**: MCP push notifications
  (`notifications/call/created` / `terminated`), SIP / SDP / RTP
  bridging, μ-law codec, engine-allocated media sockets.
- **Real on the Python side**: agent-side TTS via macOS `say`
  (produces a PCM16 mono 8 kHz WAV, encoded to PCMU and streamed).
- **Mocked**: STT (returns a placeholder from audio duration) and LLM
  (always replies with the fixed greeting). These are the hooks where
  the real **`ai.*` plugins** (P22 in post-MVP) will plug in — the
  agent's `stub_stt` / `stub_llm` functions stay intact, but their
  bodies will change to `await mcp.call_tool("ai_invoke", ...)` once
  the plugin system ships.

### A2A HTTP

```bash
# Enable A2A in the engine config:
cat > tmp/a2a.toml <<'EOF'
[observability]
health_bind = "127.0.0.1:8080"
[sip]
bind = ["127.0.0.1:5060"]
[a2a]
enabled = true
bind    = "127.0.0.1:7879"
EOF
./target/release/smiths-net --config tmp/a2a.toml &

# In another terminal:
python3 examples/python-client/a2a_demo.py
```

The demo fetches the agent card, lists tools, and invokes a few. Any
A2A-compliant agent (Google A2A SDK, a custom HTTP bot, a curl loop)
can drive the same endpoint.

## Two-terminal demo (LAN or loopback)

Run the listener and speaker in separate terminals — they meet at the
engine through the shared rendezvous key.

```bash
# Terminal 1 — engine

# Terminal 2 — listener
python3 examples/python-client/listener.py \
  --engine 127.0.0.1:5060 \
  --room   hello \
  --out    tmp/rx.wav \
  --seconds 10

# Terminal 3 — speaker
python3 examples/python-client/speaker.py \
  --engine 127.0.0.1:5060 \
  --room   hello \
  --wav    tmp/smiths-hello.wav    # optional; omit for a generated tone
```

Both processes use `sip:hello@127.0.0.1` as the Request-URI; the engine
pairs them and forwards RTP A ↔ B. Afterwards open `tmp/rx.wav`.

## Live mic/speaker and group calls

This Python client streams from WAV files. For **live two-way audio
from your computer's mic and speakers**, use the native
[`smiths-softphone`](../../crates/smiths-softphone) client instead:

```bash
cargo run -p smiths-softphone -- call --engine 127.0.0.1:5060 --room hello
```

For an **N-party conference** (more than two participants mixed into one
call), start the engine with a conference-room prefix and dial a room
under it:

```bash
SMITHS__SIP__CONFERENCE_PREFIX=conf cargo run --release -- --config examples/config.toml
# then each participant:
python3 examples/python-client/speaker.py --engine 127.0.0.1:5060 --room conf-standup ...
```

Rooms not matching the prefix keep the classic 2-peer bridge. See the
top-level [examples README](../README.md).

## What this sample does not do (yet)

- Authentication (digest / REGISTER). The engine doesn't require it in
  the current phase; when REGISTER lands the client will grow
  `authenticate(user, password)`.
- TCP / TLS transport — UDP only.
- Video / multiple `m=` lines.
- Jitter buffer on the receive side. On loopback and healthy LAN you
  don't need one; across the public internet you would.

## Troubleshooting

- **No audio in the WAV / zero-length file.** Check the engine is bound
  on the address you gave the client (`smiths-net ready …
sip_binds=[…]`). Firewalls on the RTP ephemeral ports will also
  silently drop packets.
- **`488 Not Acceptable Here`.** You sent an SDP offer whose only codec
  isn't PCMU / PCMA / Opus. This sample only offers PCMU, so you
  shouldn't see this unless you patched the offer.
- **Python < 3.9.** Upgrade; `:=` and PEP 585 type hints are used.
