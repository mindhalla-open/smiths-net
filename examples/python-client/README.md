# Python client sample

A tiny SIP UAC + RTP toolkit in **pure Python 3.9+**, using only the
standard library. Mirrors what `crates/smiths-testkit` does in Rust, so
you can poke the engine from any machine on your LAN without touching
the Rust toolchain.

## Why no MCP yet?

The engine's Model Context Protocol server will be soon.
Until it lands there's nothing to talk to
on the control plane. Even after Phase 5, **MCP drives call setup /
teardown — media always rides SIP + RTP**. So the SIP client code in
this folder stays relevant; a future `mcp_client.py` will add the
control-plane sugar (`make_call("sip:room@engine")`, `list_calls()`).

For the sample you'll interact with the engine the same way a real
softphone does: SIP over UDP + PCMU RTP.

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
