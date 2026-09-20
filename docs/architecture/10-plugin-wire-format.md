# Plugin wire format

What actually crosses the engine↔plugin boundary, per tier. There is
no per-plugin wire-format switch: each tier has one encoding, and a
manifest cannot change it.

| Tier | Encoding | Defined in |
|------|----------|------------|
| Sidecar | Newline-delimited JSON-RPC 2.0 over the child's stdin / stdout | `crates/smiths-sidecar/src/rpc.rs` |
| WASM | Typed envelopes passed as bytes through guest linear memory | `crates/smiths-proto/src/lib.rs` |
| Script (Rhai) | Rhai values converted in-process; no serialization | `crates/smiths-script/src/lib.rs` |

## Sidecar: JSON-RPC over stdio

One JSON object per line. Requests carry an `id`; the plugin replies
with the same `id`. Lines without an `id` are notifications, which the
engine republishes on its event bus. Anything on stderr is logged
against the plugin's name.

```json
{"jsonrpc":"2.0","id":1,"method":"describe_capabilities","params":{}}
{"jsonrpc":"2.0","id":1,"result":[{"capability":"ai.tts","plugin":"ai-tts-piper","model_id":"...","abi":"1.0"}]}
```

JSON was chosen over protobuf so a plugin author needs no `protoc`,
no schema compiler and no dependencies. Every example sidecar under
`plugins/examples/` is a single stdlib-only Python file. Frames are
capped (`LoaderOpts::sidecar_max_frame_bytes`, 16 MiB by default) and
each RPC has a timeout (`sidecar_rpc_timeout`, 30 s by default); a
child that overruns either is restarted under the restart policy.

Swapping this encoding later is an implementation change inside
`smiths-sidecar`, not a change to the plugin protocol: the method
names, parameter shapes and capability descriptors stay the same.

## WASM: typed envelopes

`smiths-proto` owns the logical types (`Envelope`, `Request`,
`Response`, `Notification`, `RtpFrame`) shared by the host and guest.
The host writes the encoded bytes into guest memory and passes a
pointer and length; the guest writes its reply back the same way.
Every read is bounds-checked against the guest's memory size.

`FixedFrameWireFormat` (`crates/smiths-proto/src/fixed_frame.rs`) is a
hand-rolled fixed-offset binary layout for these types: a magic, a
version, fixed-width scalars, then a variable-length region. It has no
tags and no varints, so a reader can jump straight to a field, and a
payload slice can be read without copying.

Despite the name it once carried, this is **not** FlatBuffers. The
`flatbuffers` crate's table API is `unsafe`, and the workspace sets
`unsafe_code = "deny"`; the schema needed here is a handful of scalars
plus one byte slice, so a ~250-line in-tree layout was cheaper than a
scoped `unsafe` allow plus 100 KiB of crate code. If a future format
needs schema evolution or unions, that trade is worth revisiting.

### `RtpFrame` layout

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

### `Envelope` layout

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

Fields a given kind does not populate are zero-length, so the kind
byte branches a decoder in constant time.

## See also

- `crates/smiths-sidecar/src/rpc.rs` — the JSON-RPC framing.
- `crates/smiths-proto/src/lib.rs` — the logical types.
- `crates/smiths-proto/src/fixed_frame.rs` — the fixed-offset layout.
- `crates/smiths-plugin/src/manifest.rs` — what a `plugin.toml` may
  contain.
