# Phase 2 — Media Passthrough

**Goal**: a B2BUA-style media anchor. Two SIP UAs negotiate SDP through the
engine and exchange RTP through it. Audio works end-to-end with G.711.

## Deliverables

1. `smiths-sdp` crate with parse/generate + offer/answer negotiation for
   PCMU (0), PCMA (8), Opus (111) — passthrough-only, no transcoding.
2. `smiths-media` crate with:
   - RTP session per call leg (one per direction pair).
   - RTCP RR/SR emission on a timer.
   - Passthrough router: frames from leg A forwarded to leg B, SSRC
     rewritten, sequence/timestamp re-based if needed.
   - Optional fixed jitter buffer (default off).
3. Call FSM wiring in `smiths-core`:
   - On INVITE with SDP: negotiate answer, allocate RTP sockets from
     `media.rtp_port_range`, create media session, respond `200 OK`.
   - On BYE: tear down media, release ports.
4. Integration tests using two `pjsua` instances bridged by the engine.

## Step-by-step tasks

1. **SDP parse/generate** (`smiths-sdp`)
   - Adopt `webrtc-sdp` if it compiles cleanly; else hand-roll (SDP is
     small).
   - Supported `a=` lines: `rtpmap`, `fmtp`, `sendrecv`/`sendonly`/
     `recvonly`/`inactive`, `rtcp-mux` (accept, don't require).
2. **Offer/answer** (`smiths-sdp::negotiate`)
   - Intersect codec lists (ordered by offerer preference).
   - Reject if no common codec → 488 Not Acceptable Here.
   - Output: answer SDP + selected codec + selected local RTP port.
3. **RTP session** (`smiths-media::rtp`)
   - Use `webrtc-rtp` for packet parse/encode.
   - One `UdpSocket` per session (port pair: RTP + RTCP).
   - Async tasks: `reader`, `writer`, `rtcp_timer`.
4. **Media router** (`smiths-media::router`)
   - Registry keyed by `call_id`: `{leg_a, leg_b}`.
   - Forwarding loop: `leg_a.rx → leg_b.tx` and vice versa.
   - Rewrite SSRC to a per-leg value, rewrite seq/ts monotonically.
5. **Jitter buffer** (`smiths-media::jitter`)
   - Fixed-size ring, configurable depth (default 40 ms).
   - Off by default for passthrough; enable only for hooked legs later.
6. **Port allocator** (`smiths-media::ports`)
   - Even-numbered RTP, odd-numbered RTCP (RFC 3550 convention).
   - Allocator state in `smiths-core`, released on call termination.
7. **Call FSM updates** (`smiths-core::call`)
   - New states: `AnsweringWithSdp`, `MediaActive`.
   - Failure paths: 488 on SDP fail, 408 on RTP-timeout (no packets for
     30 s on active call — configurable).
8. **Metrics** (lay the groundwork; actual Prometheus endpoint is phase 6)
   - `rtp_packets_in`, `rtp_packets_out`, `rtp_bytes_in/out`,
     `rtp_loss_total`, `rtp_jitter_ms`.
9. **Integration tests**
   - `g711_bridge.rs` — two `pjsua` instances; verify actual audio bytes
     forwarded both ways for 10 s.
   - `codec_mismatch.rs` — caller offers only Opus, callee only G.711;
     engine returns 488.
   - `sdp_reinvite.rs` — codec change via re-INVITE; media reconfigured
     without call drop.

## Acceptance criteria

- [ ] Two `pjsua` instances call through the engine; audio is audible both
  ways (manual spot-check) and `wireshark` shows correct RTP on both legs.
- [ ] RTP loss on a 10-second silent test is 0 packets on localhost.
- [ ] Engine tears down media within 500 ms of BYE.
- [ ] Codec mismatch returns 488 with a correct SDP body.
- [ ] Port allocator never leaks ports across 1000 sequential calls.
- [ ] `smiths-media` unit tests cover SSRC rewrite, seq/ts rebase, and
  port allocation wrap-around.

## Out of scope

- Transcoding, DTMF relay, SRTP. Phase 6 covers SRTP passthrough.
- Adaptive jitter buffer.
- Conferencing / mixing.

## Risks & notes

- RTP clocks differ per codec (G.711 = 8 kHz, Opus = 48 kHz); rebase must
  preserve per-codec clock rate.
- Do not block the RTP reader task — offload anything non-trivial to a
  separate task with a bounded channel.
- Don't add plugin hooks to the media path yet; that's phase 3.
