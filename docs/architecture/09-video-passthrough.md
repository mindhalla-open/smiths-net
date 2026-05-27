# Video calls — passthrough only

Slice 5.1 / P11. smiths-net speaks video *passthrough*: it
negotiates `m=video` in SDP with the same rules as the existing
`m=audio` path, relays RTP packets unchanged between legs, and
**never decodes or transcodes a frame**. The stance matters both
architecturally (the engine stays small) and operationally
(H.264 / VP8 / VP9 licensing + CPU cost stay out-of-scope).

## What shipped

The SDP layer. `smiths_sdp::Negotiator` learned a second codec
list (`supported_video`) and a new `answer_with_video` method.
The trait seam `smiths_core::SdpNegotiator` gained a `negotiate`
method that takes an optional video port and returns a
`NegotiationOutcome::Accepted { remote_media, video_media,
answer_body, srtp }`. Existing callers that use the older
`negotiate_audio` method continue to work unchanged and see
`video_media = None`.

Default video codec set:

| PT  | Codec | Clock rate | RFC            |
|-----|-------|-----------:|----------------|
| 96  | H264  |  90 000 Hz | RFC 6184       |
| 97  | VP8   |  90 000 Hz | RFC 7741       |
| 98  | VP9   |  90 000 Hz | draft-ietf-payload-vp9 |

Payload types are placeholders — on the answer the engine mirrors
the offerer's PT for passthrough-friendly B2BUA behaviour.

## What's honest-deferred

The UAS dual-bridge wiring. Today the UAS (`smiths_sip::UasServer`)
allocates one media endpoint per dialog and spawns one
`MediaFabric::bridge` per call. Adding a second endpoint +
second bridge for video means extending `DialogRecord`,
`PendingLeg`, `bridges_by_dialog`, and the BYE cleanup path —
a multi-hour refactor that deserves its own slice.

**Consequence today**: a peer that offers `m=audio + m=video`
gets a valid SDP answer with `m=video 0 ...` (RFC 3264 §6 port
0 = declined). Audio negotiates + bridges normally; no video
relay happens yet. Peers that only offer audio see zero change.

When the follow-on wires the dual-bridge path, the flip is an
additive config change — no wire or API breakage for existing
callers.

## SDP shape

Sample offer:

```
v=0
o=alice 1 1 IN IP4 192.0.2.101
s=-
c=IN IP4 192.0.2.101
m=audio 49170 RTP/AVP 0
a=rtpmap:0 PCMU/8000
m=video 49172 RTP/AVP 96 97
a=rtpmap:96 H264/90000
a=rtpmap:97 VP8/90000
```

Answer (with video port-zero decline, today's behaviour):

```
v=0
o=smiths ...
s=smiths-net
c=IN IP4 203.0.113.7
m=audio 16384 RTP/AVP 0
a=rtpmap:0 PCMU/8000
m=video 0 RTP/AVP 96 97
```

Answer (with full passthrough, once the UAS integration lands):

```
m=audio 16384 RTP/AVP 0
a=rtpmap:0 PCMU/8000
m=video 16386 RTP/AVP 96
a=rtpmap:96 H264/90000
```

The port `16 386` is a second, independently-allocated port pair
from the media fabric; it runs through a second `Bridge::spawn`
that does SSRC rewrite + RTCP stats exactly like the audio path.
No codec-specific handling: the RTP payload bytes pass through
untouched.

## Why passthrough, not transcoding

- **Licensing**: H.264 / HEVC licensing is a non-trivial legal
  surface for an embedded engine shipped as a binary. Not
  shipping a decoder sidesteps it entirely.
- **CPU**: software video codecs at even 720p30 cost a large
  fraction of a core per call. A B2BUA that stays in
  passthrough can handle hundreds of concurrent calls on
  hardware that would choke on a dozen transcoded ones.
- **Quality**: decode + re-encode is lossy, adds ~50 ms of
  buffering latency, and risks incompatible SPS/PPS across
  peers. Passthrough is both cheaper and higher quality for
  the B2BUA use case.

Callers that need transcoding (for example to bridge an H.264
peer to a VP9 peer) slot it in behind the `MediaFabric` trait as
a sidecar / FFmpeg-backed bridge; the `smiths-transcode` crate is
a dedicated slice (5.3 / P12) specifically for audio transcoding
and doesn't attempt video.

## Testing a new peer

Until the UAS dual-bridge lands, run any SIP softphone with
video enabled against the engine and confirm:

1. The 200 OK SDP answer echoes the offered m-line order
   (audio first, video second).
2. The answer's `m=video` line carries port `0` (declined).
3. Audio negotiates + bridges as usual — you hear the call.
4. No `500` / `488`; a peer that merely offers video + audio
   must not fail the call.

Post-follow-on, test matrix adds:

5. Peer offers H.264 → answer picks H.264; video RTP flows
   through the second bridge; `rtp_packets_forwarded{direction}`
   ticks on both bridges.
6. Peer offers AV1 only → answer declines video (port 0); audio
   still bridges; call not 488'd.

## References

- RFC 3264 §6 — port 0 declines a media stream.
- RFC 6184 — RTP payload format for H.264.
- RFC 7741 — RTP payload format for VP8.
- draft-ietf-payload-vp9 — RTP payload format for VP9.
