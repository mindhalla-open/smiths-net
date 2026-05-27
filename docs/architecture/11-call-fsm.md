# Call FSM: multi-session dialogs + atomic session swap

Slice 5.6 introduces the data model that lets a single dialog
carry multiple concurrent media sessions and lets the UAS
atomically swap one of them mid-call without a BYE round-trip.
This is the shared follow-on that slices 5.3 (transcoding), 5.4
(T.38 FAX), and 5.5 (conferencing) all explicitly deferred to —
each of those shipped a primitive (codec pipeline, UDPTL relay,
conference mixer) but stopped at the UAS re-INVITE path because
they all needed the same refactor.

## Why the old shape didn't fit

Pre-5.6 `DialogRecord` carried:

- `media: Option<EndpointId>` — one RTP endpoint per call.
- `remote_media: Option<SocketAddr>` — one remote RTP address.
- Nothing about codecs — the negotiator picked one and that was
  all the call-level state had room for.

This shape covers audio-only point-to-point with one codec both
sides speak. It can't express:

| Capability             | What it needs                           |
|------------------------|------------------------------------------|
| Transcoding (5.3)      | Two legs with **different** codecs       |
| Video passthrough (5.1)| Two **parallel** sessions (audio+video)  |
| T.38 FAX (5.4)         | **Swap** one session for another (audio → image) |
| Conferencing (5.5)     | Swap the 2-peer bridge for a conference participant session |

## Data model (slice 5.6)

### `LegId`

Newtype `LegId(pub u64)` identifying one *leg* of a call. `LegId(0)`
is the offerer's side, `LegId(1)` the answerer's. The engine
itself never has a `LegId` — it relays, transcodes, or mixes. The
type is distinct from `EndpointId` (media socket handle) so a
leg-vs-endpoint mix-up is a compile error.

### `MediaKindTag`

Parallel to `smiths_sdp::MediaKind` but lives in `smiths-core`:
`Audio | Video | Image | Application | Other(String)`. Keeping
it in core lets the FSM track media kinds without pulling in the
SDP crate.

### `SessionKey = (LegId, MediaKindTag)`

The key under which one media session is filed. A 2-peer audio
call has two keys; an audio+video call has four; an
audio-to-T.38 renegotiation briefly has both audio and image
keys before the audio keys drop.

### `NegotiatedCodec`

`Pcmu | Pcma | G722 | Opus | H264 | Vp8 | Vp9 | T38 | Other(String)`.
Parsed from an SDP `a=rtpmap:<pt> <name>/…` codec token (case-
insensitive); serialized as the canonical lowercase form. Two
legs' `NegotiatedCodec` entries compare equal iff the call can
run passthrough — that's the signal the 5.6b transcoding router
reads to decide whether to build a `CallTranscoder`.

### `DialogRecord::per_leg_codec`

New serializable field, keyed by `LegId`, valued as
`NegotiatedCodec`. Populated by the UAS at 200 OK INVITE time
from `NegotiationOutcome::Accepted::{audio_codec, video_codec}`.
`#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]`
so pre-5.6 HA snapshots deserialize cleanly — an older
replica's dialogs come back with an empty map, which is the
passthrough default.

### `DialogSessions`

New runtime type in `smiths_core::dialog_sessions`. Keyed by
`(DialogKey, SessionKey)`, valued as `Arc<dyn MediaSession>`.
Non-serializable (handles don't snapshot) and kept out of
`DialogRecord`. Cheap to clone (`Arc<DashMap>` inside); shared
between the UAS's re-INVITE handler, the BYE handler, and the
MCP conference tools without anyone needing a lock held across
an `.await`.

Key operations:

- `install(dialog, key, session) -> Option<old>` — add a fresh
  session. Returns the previous entry (if any) so callers that
  expected a first-install can check.
- **`swap(dialog, key, new) -> Option<old>`** — atomically
  replaces the session. The **displaced** handle is returned
  rather than dropped so the caller can `stop()` it on its own
  schedule (typically one tick after the peer's 200 OK so
  in-flight RTP drains). This is the load-bearing primitive for
  the deferred wirings.
- `remove(dialog, key) -> Option<old>` — single-key removal.
- `remove_dialog(dialog) -> Vec<old>` — drain every session of
  a dialog on BYE.
- `get(dialog, key)` — snapshot-clone one session handle.

## Swap semantics

`swap` returns the old handle rather than dropping it. Why:

1. **No forwarding gap.** The caller installs the new session
   first, verifies it's running, then stops the old one. Drop-
   then-install would have a window where neither side
   forwards bytes.
2. **Stop schedule is caller-owned.** The UAS knows when the
   peer's 200 OK ACK arrives; it can wait for any in-flight
   RTP on the old session to drain before stopping. The table
   doesn't need to know.
3. **Panic-safety.** If the caller panics mid-swap, the
   displaced handle is dropped naturally — the `Arc` reference
   count hits zero and `MediaSession::stop` runs via drop
   implementations. No leaked forwarder tasks.

## Decision tree: what does a re-INVITE mean?

Pre-5.6, the UAS treated every re-INVITE as a no-op. Post-5.6,
the router consults:

```text
re-INVITE arrives
  ├── answer's per_leg_codec differs from live record's
  │     → codec change. Either swap to a new bridge with the
  │       new codec (passthrough) or build a `CallTranscoder`
  │       (transcoding). Slice 5.6b.
  ├── answer adds/removes an `m=<kind>` block
  │     → media-kind change (e.g. audio → image for T.38).
  │       `swap_session((leg, audio), new_udptl_session)` +
  │       install matching image-keyed session. Slice 5.6b.
  └── answer is identical to the active SDP
        → refresh-only (keepalive). No session swap.
```

## What 5.6 ships vs what 5.6b ships

| Concern                                     | 5.6 (this slice) | 5.6b (follow-on) |
|---------------------------------------------|-------------------|-------------------|
| `LegId`, `MediaKindTag`, `SessionKey` types | ✅               |                   |
| `NegotiatedCodec` enum                      | ✅               |                   |
| `DialogRecord.per_leg_codec` field          | ✅               |                   |
| `DialogSessions` table + `swap` API         | ✅               |                   |
| `NegotiationOutcome::Accepted` codec fields | ✅               |                   |
| UAS populates `per_leg_codec` on INVITE     | ✅               |                   |
| Transcoding wiring (codec mismatch → `CallTranscoder`) |       | ✅                 |
| T.38 wiring (audio→image re-INVITE swap)    |                   | ✅                 |
| Conference wiring (`join_conference` swap)  |                   | ✅                 |
| UAS uses `DialogSessions` instead of `bridges_by_dialog` |  | ✅                 |

The 5.6 split is pre-committed in the slice doc: the data-model
refactor + codec detection is one working session; wiring the
three deferred primitives through is a second. The honest-scope
rule in the plan (split if a big task passes ~600 LOC or a small
passes ~200 LOC) called this out before implementation started,
so the split isn't scope-creep — it's the expected shape.

## Backwards compatibility

- `DialogRecord` gains a new field with `#[serde(default)]`,
  so existing HA snapshots deserialize unchanged (old
  snapshots come back with an empty codec map, which the
  passthrough router treats identically to "no codec info".)
- `NegotiationOutcome::Accepted` gained two new fields
  (`audio_codec`, `video_codec`). Existing callers that
  destructured with `..` need no change; one call site in the
  UAS had to destructure them explicitly (covered in this
  slice).
- `MediaKindTag` is distinct from (and does not replace)
  `smiths_sdp::MediaKind`. Both enums exist — the SDP one
  belongs to the parse tree, the core one belongs to the call
  FSM. Conversion between them is an impl detail of the 5.6b
  wirings.

## See also

- `crates/smiths-core/src/call.rs` — `LegId`, `MediaKindTag`,
  `SessionKey`, `NegotiatedCodec`, `DialogRecord.per_leg_codec`.
- `crates/smiths-core/src/dialog_sessions.rs` —
  `DialogSessions` table + atomic `swap`.
- `crates/smiths-core/src/sdp.rs` —
  `NegotiationOutcome::Accepted.audio_codec` /
  `video_codec` fields.
- `crates/smiths-sdp/src/negotiate.rs:first_codec_of_kind` —
  extracts the negotiated codec from an answer SDP.
- `crates/smiths-sip/src/uas.rs` — handle_invite path
  populates `per_leg_codec` on the DialogRecord.
- `docs/architecture/06-sip-core.md` — SIP core; this doc
  extends it.
