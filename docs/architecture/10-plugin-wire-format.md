# Plugin wire format

Slice 5.2 / P18. smiths-net ships two wire formats for the
engine↔plugin channel. Plugin authors opt in per-plugin via a
single manifest field; the engine picks the right encoder at
load time.

| Format         | Manifest value     | When to use                                                              |
|----------------|--------------------|--------------------------------------------------------------------------|
| Protobuf       | `"proto"` (default)| Control-plane RPC, ai.* capabilities, anything off the per-packet hot path. |
| FlatBuffers-style | `"flatbuffers"` | `media.streaming_rtp` plugins — zero-copy on `RtpFrame`, ≥2× decode throughput. |

Both formats speak the same logical types (`Envelope`,
`Request`, `Response`, `Notification`, `RtpFrame`); they differ
only in on-wire bytes. A plugin that flips from `proto` to
`flatbuffers` changes exactly two things: the manifest field and
the encoder/decoder import on the plugin side.

## Picking

**Rule of thumb**: if you process RTP frames per packet, pick
`flatbuffers`. Everything else, stay on `proto`.

The benchmark in
`crates/smiths-proto/tests/wire_format_throughput.rs` (run it
with `cargo test -p smiths-proto --test wire_format_throughput
-- --ignored --nocapture`) measures **3.27× speedup** on a
160-byte PCMU-shaped `RtpFrame` in debug mode. In release the
gap widens further because prost's varint decode doesn't
inline as aggressively as the flat layout's fixed-offset loads.

For `ai.*` methods (tts, asr, llm, embed) the gain is in the
noise: those methods invoke once per agent turn, and the
Envelope's `params` / `result` fields are opaque JSON that
dominates the encode cost either way. Stay on `proto` unless you
have a measurement saying otherwise.

## Manifest

Add one line:

```toml
# plugin.toml
name        = "rtp-tap"
type        = "sidecar"
entry       = "./rtp_tap.py"
provides    = ["media.streaming_rtp"]
wire_format = "flatbuffers"      # default: "proto"
```

The engine validates the token at load time — unknown values
(`"msgpack"`, `"cbor"`, typos) fail the manifest parse rather
than silently fall back. That failure is loud: the plugin
doesn't load, the CLI's startup summary reports it, operators
notice immediately.

## `RtpFrame` layout (flatbuffers variant)

```
offset  size  field
------  ----  -----------------------------------------
0       4     magic              = b"SMRF"
4       2     version            = 1 (u16 LE)
6       2     reserved           = 0
8       4     ssrc               (u32 LE)
12      4     sequence           (u32 LE)
16      4     timestamp          (u32 LE)
20      4     payload_type       (u32 LE)
24      2     direction_len      (u16 LE)
26      2     call_id_len        (u16 LE)
28      4     payload_len        (u32 LE)
32      N     direction          (UTF-8)
32+N    M     call_id            (UTF-8)
32+N+M  P     payload            (raw bytes)
```

Plugins that read only `payload` (the common case) skip the
entire variable-length region by jumping straight to the offset
at `32 + direction_len + call_id_len`. No per-field tags, no
varints, no heap allocation. The `RtpFrameView` type exposes
this as typed accessors on the engine side:

```rust
use smiths_proto::FlatbuffersWireFormat;
use smiths_proto::flatbuffers_io::RtpFrameView;

let bytes = FlatbuffersWireFormat.encode_rtp_frame(&frame);
let view = RtpFrameView::new(&bytes)?;
let payload: &[u8] = view.payload();  // zero-copy
```

Guest-side readers in other languages are small — the spec above
compiles to ~20 lines of Python or ~10 lines of Go.

## `Envelope` layout (flatbuffers variant)

```
offset  size  field
------  ----  ---------------------------------------------------
0       4     magic = b"SMEV"
4       2     version = 1
6       1     kind (0=none, 1=request, 2=response, 3=notification)
7       1     reserved
8       8     id (u64 LE; request + response only, 0 otherwise)
16      4     error_code (i32 LE; response only)
20      4     method_len  (u32 LE)
24      4     params_len
28      4     result_len
32      4     err_msg_len
36      N+M+R+E  method ++ params ++ result ++ err_msg (UTF-8)
```

Fields not populated by a given kind are zero-length. Kind byte
branches decoders in O(1).

## Migration checklist for existing plugins

1. Measure. Run your plugin through its real workload for a few
   minutes with the default `proto` format. Record the
   per-invoke CPU cost.
2. If `media.streaming_rtp` is in `provides` and the per-call
   CPU cost is non-trivial, flip `wire_format = "flatbuffers"`.
   Update your plugin's decoder to read `SMRF` framed bytes.
3. If neither is true, stay on `proto`. The format is still
   maintained and benefits from prost's mature ecosystem
   (gRPC-interop, reflection, every other Rust/Go/Python codec).
4. Re-measure. If you don't see ≥2× improvement on encode
   throughput alone, the bottleneck isn't the wire format —
   look upstream.

## Why we didn't pull the official flatbuffers runtime

The `flatbuffers` crate's low-level table API is marked
`unsafe`, and the smiths-net workspace sets `unsafe_code =
"deny"` at the root. Two ways forward were available:

1. Add a scoped `#[allow(unsafe_code)]` with a justification,
   matching the pattern `smiths-sidecar::sandbox` uses for
   `pre_exec`.
2. Hand-roll a flat binary layout that matches the FlatBuffers
   philosophy (fixed offsets, no tags, zero-copy reads) without
   pulling the library.

Option 2 won because the schema we need is tiny — five scalar
fields plus one bytes slice for `RtpFrame`, seven scalar/string
fields for `Envelope`. The library's strength (schema
evolution, hundreds of field tables, unions) isn't exercised by
our schema; its cost (100+ KiB of crate code, `unsafe`) is. The
in-tree hand-rolled layout is ~250 LOC total and keeps the
unsafe-free posture.

If a future slice needs a richer schema (say, a streaming
camera-frame format with optional metadata tables), we can
revisit — a scoped `unsafe_code` allow with a justification
comment is a one-file change.

## See also

- `crates/smiths-proto/src/lib.rs` — `WireFormat` trait +
  `ProtoWireFormat`.
- `crates/smiths-proto/src/flatbuffers_io.rs` — hand-rolled
  flat layouts + `RtpFrameView`.
- `crates/smiths-proto/tests/wire_format_throughput.rs` — the
  ≥2× acceptance test.
- `crates/smiths-plugin/src/manifest.rs` — `WireFormat` enum +
  manifest field.
