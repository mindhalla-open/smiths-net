# Binary-framing sidecar (Python)

A sidecar that speaks smiths-net's length-prefixed binary framing
instead of the default newline-delimited JSON-RPC. Stdlib only, like
every other sample here.

## When you want this

Almost never. JSON-RPC is the default because a plugin is then one
file with no dependencies, no schema compiler and nothing to
regenerate when the engine changes. Reach for the binary framing only
when you have measured that per-frame JSON parsing is actually costing
you — a plugin on the per-packet RTP path is the case it exists for.
Control-plane plugins and the whole `ai.*` family invoke once per
agent turn, where the encoding is lost in the noise.

## Turning it on

One key in `plugin.toml`:

```toml
wire_format = "binary"   # default: "json"
```

The engine rejects any other value at load time and names the ones it
accepts. Nothing else changes: the same `describe_capabilities` and
`invoke` methods, the same notification channel, the same per-RPC
timeout, frame cap and restart policy.

## The framing

Each frame, in both directions:

```
4 bytes   big-endian uint32 length of the body
N bytes   an Envelope
```

The body is a fixed-offset `Envelope`. Every field inside it is
**little-endian**; only the outer length prefix is big-endian.

```
offset  size  field
0       4     magic = b"SMEV"
4       2     version = 1
6       1     kind: 1 = request, 2 = response, 3 = notification
7       1     reserved
8       8     id (request and response; 0 otherwise)
16      4     error_code (responses; 0 when there is no error)
20      4     method_len
24      4     params_len
28      4     result_len
32      4     err_msg_len
36      ...   method ++ params ++ result ++ err_msg, all UTF-8
```

Fields a given kind does not use are zero-length, so the kind byte
tells a reader what to expect without scanning. `params` and `result`
carry JSON text, exactly as they do over the JSON transport.

A response sets either `result` or `err_msg`, never both. A
notification has no id and no reply.

The engine only ever *sends* requests and only ever *accepts*
responses and notifications; a child that sends a request is treated
as speaking a protocol the engine does not offer, and is dropped.

## Running it

```bash
./target/release/smiths-net --config config.toml
```

with `[plugins] dir` pointing at a tree containing this directory.
Then call the `echo` capability through MCP.

`binary_frames.py` is about 60 lines of actual logic, which is the
point: the layout is meant to be implementable from this README in
any language, without a code generator.
