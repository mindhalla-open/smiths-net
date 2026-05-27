# Implementation TODO

Checkable list across MVP phases. Per-phase docs (`phase-N-*.md`) hold
the context and acceptance criteria; this is the day-to-day checklist.

Legend: `[ ]` todo · `[x]` done · `[-]` skipped / won't do (with note)

---

## Phase 0 — Foundation _(complete 2026-04-17)_

### Workspace + tooling

- [x] Convert to Cargo workspace (`resolver = "3"`, edition 2024)
- [x] `rust-toolchain.toml` (channel stable, rustfmt + clippy, minimal profile)
- [x] `[workspace.lints]` — forbid unsafe, pedantic clippy warn
- [x] `[workspace.dependencies]` — pinned shared deps
- [x] Shell crates under `crates/` for all planned modules
- [x] `examples/config.toml` with commented defaults
- [x] GitHub Actions CI: fmt + clippy + test + build
- [x] `[profile.release]` — thin LTO, 1 codegen unit, strip symbols
- [x] `cargo build --release` artifact under 20 MB (actual: **2.4 MB**)
- [x] CONTRIBUTING.md
- [x] LICENSE (Apache-2.0)
- [x] `.gitignore`: add `.DS_Store`
- [x] CHANGELOG.md with `[Unreleased]` placeholder for phase 1+ work

### smiths-core

- [x] `config::Config` + `CoreConfig` + `ObservabilityConfig` + `LogFormat`
- [x] `config::Config::load` via `figment` (TOML file + `SMITHS__*` env)
- [x] `bus::EventBus` over `tokio::sync::broadcast`
- [x] `event::Event` + `SystemEvent` (Ready / ShutdownRequested / ShutdownComplete)
- [x] `shutdown::Shutdown` over `CancellationToken` with SIGINT/SIGTERM
- [x] `error::Error` (thiserror)
- [x] Unit tests: config defaults, env overrides, bus round-trip, shutdown cancel
- [x] `#[instrument]` coverage audit — spans on UAS `run` / `handle_invite` /
      `handle_register` / `handle_bye`, `UdpTransport` + `TcpTransport`
      `spawn_reader`, `UdpMediaFabric::{allocate,bridge}`, plugin
      `load_plugins` / `load_one`, `Sidecar::{spawn, call_with_timeout}`

### smiths-cli

- [x] `clap` CLI (`--config`, `--log`)
- [x] Tracing init (JSON + pretty, `RUST_LOG` honored as override)
- [x] Health HTTP server (`GET /health`)
- [x] Graceful shutdown triggered by signal or cancel token
- [x] Structured startup log line with version + bind

### Acceptance

- [x] `cargo fmt --check` green
- [x] `cargo clippy --workspace --all-targets -- -D warnings` green
- [x] `cargo test --workspace` green (7 unit tests pass)
- [x] `cargo run --release -- --config examples/config.toml` stays up
- [x] `curl http://127.0.0.1:8080/health` → `{"status":"ok"}`
- [x] `SIGTERM` → exits within 2 s with "graceful shutdown complete"
- [x] Release binary < 20 MB target (actual: **2.4 MB**)

---

## Phase 1 — SIP Core

### Landed in v0.1.0 (2026-04-18)

- [x] UDP transport (`smiths-sip::transport::udp`)
- [x] `Transport` trait (send-message / recv-message, not raw bytes) — **MVP guardrail for HTTP/3, QUIC, proxies**
- [x] Parser via `rsip` with `ParseError` bus event
- [x] `CredentialStore` trait in `smiths-sip::auth` + in-memory impl — **MVP guardrail for subscriber DB**
- [x] `SipEvent::{RequestReceived, ResponseSent, ParseError}` on bus
- [x] UAS answers `OPTIONS` with 200, rejects others with 405
- [x] UDP retransmission dedupe (per-`Via`-branch response cache)
- [x] Integration tests: `options_returns_200_ok`, `unknown_method_returns_405`, `retransmission_replays_cached_response`
- [x] Full-binary e2e test (spawn, health, OPTIONS, 405, SIGTERM, clean exit)
- [x] INVITE → 100 Trying → 200 OK (with Contact), ACK confirms dialog, BYE → 200 OK (481 if unknown)
- [x] `SipEvent::{DialogCreated, DialogTerminated}` on bus
- [x] Integration tests: `invite_establishes_dialog_ack_then_bye`, `bye_without_dialog_returns_481`, `invite_retransmit_replays_same_200`
- [x] `smiths-sdp` crate: parser, generator, types, offer/answer negotiator with `NegotiationResult::Mismatch` branch
- [x] INVITE with SDP offer → 200 OK carrying SDP answer (engine-allocated media port); 488 on codec mismatch
- [x] Per-dialog UDP media socket held on the dialog record (step 3 will forward through it)
- [x] Integration tests: `invite_with_sdp_offer_gets_sdp_answer`, `invite_with_only_unknown_codecs_returns_488`
- [x] `smiths-media::bridge` — two-leg byte-transparent UDP forwarder with cancel-driven shutdown
- [x] UAS rendezvous bridging — two INVITEs to the same `sip:<key>@engine` get paired, BYE from either side tears the bridge down
- [x] `smiths-testkit`: `TestUac`, RTP packet, μ-law codec, sine generator, WAV writer
- [x] End-to-end audio test `two_uas_call_preserves_audio_byte_for_byte` — byte-for-byte μ-law round-trip through the engine, playable WAV at `/tmp/smiths-call-received.wav`

### Clean-architecture refactor (landed after v0.1.0)

- [x] **Trait seams in `smiths-core`**: `MediaFabric` + `EndpointId` / `BridgeId`
      tokens (`core::media`), `SdpNegotiator` + `NegotiationOutcome`
      (`core::sdp`). `smiths-sip` consumes both as `Arc<dyn …>` at
      construction — no direct deps on sibling crates.
- [x] **`DialogRecord` with `Serialize`** in `smiths-core::call` —
      runtime handles (socket, bridge) stay in the fabric; the dialog
      table snapshots cleanly. **MVP guardrail for HA** satisfied.
- [x] **`BindSpec` newtype** in `smiths-core::config` — accepts
      `"ip:port"` today, reserves interface-name syntax for P16
      (proxy/VPN). **MVP guardrail for proxy/VPN** satisfied.
- [x] `UdpMediaFabric` impl in `smiths-media` — fabric owns every RTP
      socket; SIP only sees opaque tokens.
- [x] `Negotiator: SdpNegotiator` impl in `smiths-sdp` — UAS sees only
      the trait; no SDP types cross the SIP boundary.
- [x] `smiths-sip/Cargo.toml` no longer lists `smiths-sdp` or
      `smiths-media` as deps (they are dev-deps in integration tests
      only, where wiring the real impls is the point).

### Landed (unreleased after v0.5.0)

- [x] Digest challenge/response (MD5 + SHA-256, `qop=auth`, nonce cache with TTL)
- [x] REGISTER handling as registrar — 401 challenge + 200 OK on valid auth
- [x] Integration test: `register_challenge_then_authenticate` (+ wrong-password re-challenge + no-registrar dev-mode)

### Landed (unreleased, after v0.5.0 refactor)

- [x] TCP transport (`smiths-sip::transport::tcp`) — framed reads
      (Content-Length + double-CRLF), per-peer mpsc writer, inbound accept
      loop + lazy outbound connect, wired into `smiths-cli` alongside UDP
- [x] `smiths-testkit::{fake_uac, fake_uas}` helpers — `FakeUac`
      (renamed from `TestUac`) plus a minimal `FakeUas` that captures
      requests and replays canned responses
- [x] INVITE digest auth — registrar-attached UAS challenges every
      unauthenticated INVITE with 401 + WWW-Authenticate
- [x] Integration test: `invite_401_cancel` — INVITE → 401 → ACK → BYE
      returns 481, proving no dialog state leaked
- [x] Dedupe fix: ACK carrying the rejected-INVITE branch no longer
      replays the cached 401 (RFC 3261 §17.1.1.3)
- [x] SIP parser fuzz harness (`fuzz/` via `cargo-fuzz` +
      `libfuzzer-sys`, target `sip_parser`, workspace-excluded)
- [x] sipp REGISTER load scenario (`scenarios/sipp/register.xml`) with
      digest auth + run-command docs

### Deferred to a dedicated session

- [x] Full transaction FSMs (4 flavors, RFC 3261 timers A–K) —
      landed across v0.16.0–v0.19.0. All four FSMs (client
      non-INVITE §17.1.2, client INVITE §17.1.1, server INVITE
      §17.2.1, server non-INVITE §17.2.2) hosted by an async
      `TransactionDriver` (timer wheel + response router). UAC
      migrations (`hangup`, `place_call`) complete.
- [x] Dialog FSM driver (Idle → Early → Confirmed → Terminated) —
      landed in v0.19.0 as `smiths-sip::txn::DialogFsm`; typed
      `DialogTransitionError` for illegal events, projection to
      serializable `smiths_core::DialogState` for HA snapshots.

### External validation (no code blocker)

- [ ] sipp perf run: median auth round-trip < 1 s for 100 concurrent
      REGISTERs — scenario ready, needs a measurement session against a
      release build

## Phase 2 — Media Passthrough

### Landed in v0.1.0

- [x] SDP parse/generate + offer/answer (landed with Phase 1 audio e2e)
- [x] `NegotiationOutcome::Mismatch` branch — **MVP guardrail for transcoding**
- [x] `MediaFabric` trait + `UdpMediaFabric` passthrough impl — **MVP guardrail for conferencing**

### Landed (unreleased, after v0.5.0)

- [x] `MediaEndpoint` trait in `smiths-core::media` — default `Endpoint`
      impl for host candidates plus an `EndpointKind` enum
      (`Host` / `ServerReflexive` / `Relayed`) reserved for ICE. Fabric
      now returns `Arc<dyn MediaEndpoint>`.
- [x] `MediaSession` trait in `smiths-core::media` — `Bridge` implements
      it; guardrail for T.38 / WebTransport / mixer session types.
- [x] Port allocator even-RTP / odd-RTCP (`smiths-media::port_allocator`) —
      retrying bind loop, each endpoint now owns a proper pair.
- [x] SSRC-rewriting passthrough router — `smiths-media::bridge` parses
      RTP headers, rewrites SSRC per leg (stable per-direction engine
      SSRC), drops non-RTP traffic.
- [x] Integration test `g711_bridge` — verifies payload round-trip **and**
      egress SSRC is different + stable across the leg.

### Landed in v0.11.0 (2026-04-20)

- [x] RTP session stats + RTCP Sender Report emission. Per-direction
      `StreamStats` (packets, bytes, max seq, last RTP ts, jitter per
      RFC 3550 §A.8); `Bridge` spawns a per-direction SR emitter that
      writes RFC 3550 §6.4.1 packets to the peer's RTCP port on the
      configured interval (default 5 s). `UdpMediaFabric::bridge`
      uses the already-allocated RTCP sockets that had been idle.
      RR blocks + jitter-buffer + codec-mismatch path remain for the
      next media slice.

### Deferred to a dedicated session

- [ ] RTCP Receiver Report (RR) blocks embedded in SR — loss%,
      jitter, last-SR handshake for two-way quality reporting.
- [x] Integration test `codec_mismatch` (4 richer scenarios:
      multi-codec, video-only, SAVP-without-crypto, SAVP-with-
      unknown-suite) landed v0.20.0 at
      `crates/smiths-sip/tests/codec_mismatch.rs`.
- [ ] Integration test `sdp_reinvite` — needs re-INVITE support
      in the UAS (blocks on session-modification handling).

## Phase 3 — WASM Plugins

### Landed (unreleased, walking skeleton)

- [x] `wasmtime` engine + per-call fuel metering
      (`smiths-wasm::WasmEngine`) — typed `WasmError` surface covering
      compile / link / trap / fuel-exhausted / missing-export. Epoch
      interruption deferred to the richer-host-surface session.
- [x] Host function `smiths::log(ptr, len)` — reads UTF-8 from guest
      memory and emits a tracing event tagged with plugin name.
      Out-of-bounds / non-UTF-8 is a trap.
- [x] `Dispatcher` trait + `MemoryDispatcher` in `smiths-plugin` —
      priority-ordered fan-out, re-register semantics, per-event time
      budget skipping the slow tail.
- [x] `rust-logger` example plugin (`plugins/examples/rust-logger/`) —
      `no_std` cdylib targeting `wasm32-unknown-unknown`, calls
      `smiths::log` from its `run` export; README + build command.
- [x] WASM engine tests (`crates/smiths-wasm/tests/engine.rs`) using
      inline WAT: host-log round-trip, trap isolation (engine survives,
      next module runs), fuel exhaustion, missing export, log OOB trap.

### Landed in v0.8.0 (2026-04-18)

- [x] Plugin manifest loader for WASM tier — `type = "wasm"`
      manifests compile through `WasmProvider::load`, register in
      `AiRegistry` alongside sidecars, advertise capabilities via
      the guest's `describe() -> i64` export.
- [x] Epoch-interruption deadline + background ticker. `WasmEngine::
      run_with_deadline(Duration)` arms a cancellable one-shot timer
      that increments the engine epoch on expiry; guest traps with
      `WasmError::Timeout`. Deadline-free `run_entry` still available.
- [x] Host `state_set` / `state_get` (per-plugin persistent KV) —
      survives `Store` drops across invocations, namespaced per
      plugin name.
- [x] `rust-logger` grew a `describe()` export advertising `ai.log`.
      WASM loader integration test (`wasm_loader.rs`) stages an
      inline-WAT module end-to-end through `load_plugins` and asserts
      the capability is surfaced.

### Landed in v0.9.0 (2026-04-19)

- [x] WASM plugin invocation dispatch. `WasmEngine::call_invoke`
      implements the full guest ABI (`alloc` + `invoke`) with a
      `{"result"|"error"}` response envelope; `WasmProvider::invoke`
      is wired. Real WASM plugins now round-trip describe + invoke
      through `AiRegistry`.
- [x] Permission checks against manifest. `plugin.toml` grew
      `permissions: Vec<String>` (empty default); engine maintains
      a per-plugin `Arc<HashSet<String>>`; `state_{get,set}` gated
      behind `"state"`. Typed `WasmError::PermissionDenied` for
      distinguishable traps. Future host fns key off the same list.
- [x] `rust-logger` grew `alloc` + `invoke` exports — complete
      reference for the WASM tier.

### Landed in v0.10.0 (2026-04-20)

- [x] Host surface expansion: `publish_event` + `timer_set`.
      Guest forwards `(topic, bytes)` onto the engine bus as
      `PluginEvent::Published`; schedules a host-side one-shot timer
      that fires back `PluginEvent::TimerFired`. Gated behind
      `"events"` / `"timers"` permissions. `WasmEngine::with_bus`
      wires the bus at engine construction time.
- [x] Hot reload file watcher (`smiths-plugin::watcher`). Polls
      plugin dir, debounces editor save bursts, calls
      `AiRegistry::reload` on change. `PollWatcher` with
      `compare_contents(true)` so it works on macOS HFS+ and
      tempdirs. Integration test covers the full round-trip.
- [x] Proto schema v1 freeze in `smiths-proto`. Envelope +
      Request/Response/Notification messages with prost derive; no
      build.rs. Round-trip tests per variant.

### Landed in v0.11.0 (2026-04-20)

- [x] WASM `send_rtp` host fn. Plugins can push audio into a live
      call via `smiths::send_rtp(call_id, bytes)` — engine looks up
      the endpoint via the new `smiths-core::CallLookup` trait
      (implemented by `ControlState`) and dispatches through
      `MediaFabric::send_packet`. Gated behind `"send_rtp"` permission;
      engine builder `with_media(lookup, fabric)`.

### Landed in v0.12.0 (2026-04-20) — operability slice

- [x] Metrics coverage across SIP / media / plugin. `Metrics` grew
      `sip_parse_errors`, `bridges_active`, `rtp_packets_forwarded`,
      `rtcp_sr_sent`, `plugin_invocations`, `plugin_invoke_duration`,
      `sidecar_restarts`. Threaded via `LoaderOpts::metrics`,
      `UdpMediaFabric::with_metrics`, `Sidecar::set_metrics`. First
      slice of the prod-readiness roadmap
      (`.vscode/prod-readiness-roadmap.md`).

### Landed in v0.13.0 (2026-04-20) — graceful drain + deep health

- [x] `smiths-core::Drain` primitive + UAS integration. New
      INVITEs during drain are refused with `503` +
      `Retry-After: 0`; existing dialogs finish naturally. CLI
      flips on shutdown signal, sleeps `SMITHS_DRAIN_SECS`
      (default 5), then fires the hard cancel.
- [x] Deep `/health` endpoint. Returns JSON `{ status, draining,
      uptime_secs, sip.binds, plugins.{loaded,failed},
      dialogs_active, bridges_active }` — enough for k8s liveness
      probes and load balancer health checks to see live state.
- [x] sipp perf run (2026-04-20). Validated REGISTER throughput
      against the default no-auth engine on loopback. Key result:
      the run found + locked down a **dedupe-eviction deadlock**
      (DashMap Iter holding a shard read guard across a `remove()`
      on the same shard). Fix shipped in **v0.13.1**; post-fix
      the engine sustains ~10 k cps REGISTER with 0 retransmits.
      Blind variant scenario at `scenarios/sipp/register_noauth.xml`.

### Landed in v0.14.0 (2026-04-20) — hardening slice

- [x] Fuzz harness prove-out. `cargo +nightly fuzz run sip_parser`
      for ~4 min = **5.2M runs, 0 crashes** across `rsip` +
      `summarize_request` + `extract_via_branch`. Seed corpus at
      `fuzz/corpus/sip_parser/` primed for future runs.
- [x] Per-source-IP SIP rate limiting. Token bucket via
      `SipRateLimiter`; config `sip.rate_limit.{per_sec, burst}`
      (disabled by default). Drops over-limit datagrams before
      parse. Shared limiter across UDP/TCP/TLS in the CLI.
- [x] RTCP Receiver Report blocks. Bridge emitter embeds an RB in
      each outgoing SR describing the peer's inbound stream;
      separate listener parses incoming RRs (logs at debug for
      now). `ReportBlock` / `build_sr_with_rb` / `build_rr` /
      `parse_rr` in `smiths-media::rtcp`.

### Landed in v0.15.0 (2026-04-20) — SRTP slice

- [x] SRTP (SDES). `SrtpTransform` trait seam in `smiths-core`;
      `webrtc-srtp`-backed `AesCmHmacSha1_80Transform` in
      `smiths-media`; `SdesCrypto` parser/generator in `smiths-sdp`;
      Bridge decrypt → SSRC rewrite → re-encrypt with integration
      test verifying end-to-end (UA-A encrypts → engine → UA-B
      decrypts identical plaintext). SDP-negotiator wiring closed
      in v0.20.0; DTLS-SRTP still deferred (co-dependent with ICE).
- [x] Cumulative-loss tracking in `StreamStats`. RFC 3550 §A.3
      base_seq + cycles + max_seq → `expected − received`; SR emitter
      feeds real numbers into RR blocks (peers see loss).
- [x] `SMITHS_TEST_CREDS` env seed for dev registrar. Enables
      `scenarios/sipp/register.xml` to run auth-exercised. Fixed a
      digest URI-mismatch for clients signing only the host
      authority (sipp default). Prove-out: 5000 auth round-trips
      @ 1000 cps, 100% success.

### Landed in v0.16.0 (2026-04-20) — FSM slice 1

- [x] RFC 3261 §17 transaction framework. Pure-synchronous FSM
      types in `smiths-sip::txn` (`TransactionState`, `Role`,
      `TransactionKey`, `TimerId` A–K, `TransactionEvent`,
      `TransactionAction`, `Transaction` trait). Timers module with
      RFC constants `T1`/`T2`/`T4`/`TIMEOUT_64T1` + doubling-backoff
      helper.
- [x] `ClientNonInviteTxn` (RFC 3261 §17.1.2) — first FSM. Trying
      → Proceeding → Completed → Terminated; timers E (retransmit,
      doubling up to T2), F (timeout, 64·T1), K (wait for dup
      responses, T4). 14 unit tests across every transition + every
      timer path. Additive — UAS/UAC unchanged.

### Landed in v0.17.0 (2026-04-20) — FSM slice 2

- [x] Async `TransactionDriver<T: Transport>` — transaction table +
      per-timer tokio tasks + response-listener on the router.
      `TuEvent` stream (Response / Terminated) the TU drains.
- [x] Two end-to-end driver integration tests on live UDP sockets
      (send→response round-trip; timer-E actually retransmits after
      T1=500 ms).
- [x] `UacClient::hangup` migrated onto the FSM path. First real
      call-site consuming the transaction layer. Signature
      unchanged; existing e2e test passes through migrated path.
      BYE now retransmits at T1/2T1/4T1/…/T2 instead of silent
      burn-through on UDP loss.

### Landed in v0.18.0 (2026-04-20) — FSM slices 3 + 4

- [x] `ClientInviteTxn` (RFC 3261 §17.1.1) — timers A/B/D,
      `Calling`/`Proceeding`/`Completed`/`Terminated`, 2xx bypass
      to Terminated, auto-ACK for 3xx-6xx per §17.1.1.3. 12 tests.
- [x] Byte-level `build_non_ok_ack` helper — reuses INVITE Via
      branch, pulls `To` from response. 4 tests.
- [x] `ServerInviteTxn` (RFC 3261 §17.2.1) — timers G/H/I,
      `Proceeding`/`Completed`/`Confirmed`/`Terminated`, caches
      last response for retransmit dedupe. 13 tests.
      **Library only — UAS wiring = slice 5.**
- [x] `UacClient::place_call` migrated onto the client INVITE FSM.
      INVITE now retransmits on UDP loss via timer A (prior code
      was one-shot + silent 30 s burn). Dead `wait_for_final` /
      `parse_status` helpers removed; `router` field removed from
      `UacClient` (driver owns it).

### Landed in v0.19.0 (2026-04-19) — FSM slice 5 (final)

- [x] `ServerNonInviteTxn` (RFC 3261 §17.2.2) — timer J,
      `Trying`/`Proceeding`/`Completed`/`Terminated`, request
      retransmits replay cached response. 10 tests.
- [x] `DialogFsm` (RFC 3261 §12) — `Early`/`Confirmed`/`Terminated`,
      typed `DialogTransitionError` on illegal events (dialogs
      long-lived, stray events = app bug), projection to
      serializable `smiths_core::DialogState`. 10 tests.
- [x] `TransactionDriver::start_server` / `send_response` /
      `deliver_request` — driver now hosts server FSMs
      symmetrically to client FSMs. 2 driver integration tests.

### Landed in v0.20.0 (2026-04-20) — SDES end-to-end

- [x] `smiths-core::sdp::SrtpKeys` + `NegotiationOutcome::Accepted
      { srtp }` — the negotiator now reports the negotiated SDES
      keys alongside the answer body. `Debug` is redacted so key
      bytes never leak into logs.
- [x] `smiths-core::media::BridgeLeg` — new spec struct; the
      `MediaFabric::bridge` signature collapsed to
      `bridge(a: BridgeLeg, b: BridgeLeg)`, each leg optionally
      carrying `SrtpKeys`. All in-tree callers migrated.
- [x] `smiths-sdp::MediaDescription::crypto` — parser + serializer
      for `a=crypto:` lines. Parse errors on a single line are
      soft (logged + skipped) so malformed crypto doesn't kill
      the SDP document.
- [x] `smiths-sdp::Negotiator` — `RTP/SAVP` offers with supported
      `a=crypto:` get answered with a matching engine-generated
      key; SAVP without acceptable crypto → `Mismatch` per
      RFC 4568 §5.1.2.
- [x] `smiths-sip::uas` — threads `SrtpKeys` from the negotiator
      outcome into `PendingLeg` → `BridgeLeg`, so rendezvous
      pairing automatically instantiates SRTP transforms on
      both legs.
- [x] Integration tests: `sdp_srtp.rs` (two UAs ↔ engine SRTP
      end-to-end through a real rendezvous bridge, verifies
      engine doesn't echo peer keys), `codec_mismatch.rs` (4
      richer mismatch scenarios including SAVP-without-crypto
      and unsupported-suite). 11 new tests total (5 negotiator
      + 4 mismatch + 1 SDES end-to-end + 1 fresh-key helper).

### Landed in v0.20.0 (2026-04-20) — ops doc refresh

- [x] `docs/architecture/03-mcp-and-ops.md` rewritten MCP section
      — three transports (stdio, HTTP, SSE), A2A adapter, the
      actual shipped tool + resource sets, SSE notifications,
      bearer/rate-limit/audit posture. Closes the Phase 5
      pending item.

### Landed in v0.21.0 (2026-04-20) — UAS FSM migration (non-INVITE)

- [x] UAS non-INVITE path (OPTIONS / BYE / REGISTER / CANCEL /
      unknown-405) migrated off the legacy `dedupe` DashMap onto
      `ServerNonInviteTxn` entries in the shared
      `TransactionDriver`. Retransmits replay the FSM's cached
      `last_response`; timer J absorbs for `64 · T1 = 32 s`.
- [x] New CLI test
      `mcp_stdio_initialize_list_call_over_spawned_binary` —
      spawns the real binary with `--mcp stdio`, drives JSON-RPC
      over stdin, verifies `initialize` / `tools/list` /
      `tools/call health`. Closes the last Phase 5 pending item.
- [x] Workspace lints: `dbg_macro` / `print_stdout` /
      `print_stderr` / `todo` / `unimplemented` promoted to
      `warn` (zero current fires).

### Landed in v0.23.0 (2026-04-20) — sidecar sandboxing (MVP)

- [x] `smiths_core::SandboxConfig` on `PluginsConfig.sandbox` with
      `max_fds` / `max_memory_bytes` / `max_cpu_seconds` /
      `max_processes` / `no_new_privs`.
- [x] `smiths_sidecar::sandbox::apply_in_child` — async-signal-safe
      `setrlimit` + Linux `PR_SET_NO_NEW_PRIVS` via the `rustix`
      safe-wrapper crate. `Sidecar::spawn_with` attaches the
      sandbox to every spawn + respawn.
- [x] `LoaderOpts.sandbox` wired through the CLI so
      `config.plugins.sandbox` flows into every loaded plugin.
- [x] `examples/config.toml` gained a documented
      `[plugins.sandbox]` section.
- [x] Integration test
      `sandbox_rlimit_nofile_is_applied_to_child` spawns a bash
      script, asks it to report `ulimit -n`, asserts the
      configured cap is observed end-to-end.
- [x] Workspace `unsafe_code` policy: `forbid` → `deny` with a
      single documented exception at the `pre_exec` callsite.
      Every other crate stays unsafe-free.

Out of scope for this slice (tracked as a follow-on): seccomp-BPF
syscall filtering, user-namespace isolation, cgroups v2 resource
containers.

### Landed in v0.22.0 (2026-04-20) — UAS FSM migration (complete)

- [x] UAS INVITE path migrated onto `ServerInviteTxn` (G/H/I
      retransmit timers + 2xx bypass). `send_provisional` (100
      Trying) and `respond` both route through
      `driver.send_response`; `handle_ack` delivers ACK to the
      INVITE FSM for Completed → Confirmed.
- [x] Legacy `dedupe` `DashMap` removed. Narrow
      `invite_2xx_cache` retains 2xx retransmit replay (TU-owned
      per RFC 3261 §13.3.1.4) with the 4096-entry LRU cap +
      v0.13.1 shard-scoped eviction pattern.
- [x] `sip_server_txns_active` Prometheus gauge — operator
      visibility for FSM entry count (replaces the old LRU-cap
      signal). `TransactionDriver::with_metrics` builder wired;
      unit test `metrics_gauge_tracks_server_txn_lifecycle`.
- [x] `clippy::unwrap_used = warn` on `smiths-core` +
      `smiths-sdp` via crate-level `#![warn(...)]` attributes +
      `cfg_attr(test, allow(...))`. Both crates have zero
      production-code unwraps.
- [x] New `sdes_crypto` fuzz target exercising
      `SdesCrypto::parse` + the `a=crypto:` SDP dispatcher.

### Deferred (cleanup, not correctness)

- [ ] Per-dialog 2xx INVITE retransmit loop (RFC 3261 §13.3.1.4).
      Today's `invite_2xx_cache` passes the bytes back on simple
      peer retry; a proper TU loop would emit timer T1/2T1/… up
      to T1 · 64 until ACK lands. ~150 LOC, unblocks removing
      the narrow cache entirely.
- [ ] Rest of host surface: `send_sip`. Needs dialog-context design
      (which dialog, which transaction) — not a pure additive slice.
- [ ] `tinygo-hdr` example (needs Go + tinygo toolchain).
- [ ] Tests: rtp mutation.

## Phase 3 — WASM Plugins

- [ ] Freeze proto schema v1 in `smiths-proto`
- [ ] Plugin manifest loader — include `provides = [...]` field — **MVP guardrail for AI + storage**
- [ ] `Dispatcher` with priority + per-hook budget
- [ ] `wasmtime` engine + per-call store with fuel + epoch interruption
- [ ] Host function surface (`log`, `send_sip`, `send_rtp`, timers, state, events)
- [ ] Permission checks against manifest
- [ ] Example plugins: `rust-logger`, `tinygo-hdr`
- [ ] Tests: dispatch order, trap isolation, rtp mutation, hot reload

## Phase 4 — Sidecar Plugins

### Landed (unreleased after v0.4.0)

- [x] Subprocess supervisor — `smiths-sidecar::Sidecar` with JSON-RPC 2.0 newline-delimited stdio, request/response correlation, per-call timeouts, `kill_on_drop`
- [x] Plugin manifest + loader (`smiths-plugin`): `Manifest`, `CapabilityDescriptor` (with open `extra` JSON for per-capability fields), `AiRegistry`, `load_plugins(root, registry)`
- [x] `describe_capabilities` handshake at load; fail-partial load with per-plugin error reports
- [x] MCP tools `list_ai_providers` / `describe_provider` (shared by MCP + A2A)
- [x] CLI wiring: `[plugins] dir` config, load at startup, drain on shutdown
- [x] Reference plugin `plugins/examples/ai-tts-mock/` with realistic `ai.tts` descriptor (Python stdlib)

### Landed (unreleased after v0.5.0)

- [x] Control validation pass — `smiths_plugin::validate_controls` with `type` / `minimum` / `maximum` / `enum`; strict-reject unknown keys with `supported: [...]` diagnostic
- [x] Invocation tool: `synthesize(plugin, text, voice?, controls?, output?)` — full engine → plugin → audio-bytes loop
- [x] `ai-tts-mock` implements a real `synthesize` via macOS `say` (silent-audio fallback on Linux)
- [x] `voice_agent.py` refactored: uses `call_tool("synthesize", ...)` instead of a local `say` shell-out; MCP request/response correlation in Python
- [x] MCP tool `transcribe(plugin, audio_base64, language?, controls?)` → dispatches to `ai.asr` plugin
- [x] MCP tool `llm_chat(plugin, messages, controls?)` → dispatches to `ai.llm.chat` plugin
- [x] Reference plugins `ai-asr-mock` + `ai-llm-mock` with realistic descriptors (Python stdlib)
- [x] `voice_agent.py` goes **zero-AI-code** — every hop (STT/LLM/TTS) now flows MCP → plugin

### Landed (unreleased, sidecar hardening)

- [x] Restart policy with exponential backoff (`smiths-sidecar::RestartPolicy`).
      Supervisor task detects crash via stdout EOF, respawns up to
      `max_retries` with configurable `initial_backoff` / `max_backoff` /
      `backoff_multiplier`. In-flight RPCs at crash time resolve to
      `Error::Closed`; `no_restart()` policy preserves the old
      suicide-on-crash behaviour.
- [x] `embed(plugin, inputs[], controls?)` tool + `ai-embed-mock`
      reference plugin. Completes the AI quartet
      (synthesize / transcribe / llm_chat / embed). Deterministic
      SHA-256-seeded 128-dim vectors so tests stay repeatable.
- [x] Lifecycle + crash + backpressure tests —
      `sidecar_respawns_after_crash`, `no_restart_policy_stays_down`,
      `concurrent_calls_all_complete` (32 in-flight RPCs through the
      echo script).

### Landed (unreleased, engine-side speak)

- [x] Engine-side `speak(call_id, plugin, text, voice?, controls?)` tool.
      Full agent-driven audio injection: looks up the call's media
      endpoint → invokes the plugin's `synthesize` → decodes PCM16 →
      downsamples to 8 kHz → μ-law encodes → chunks into 20 ms RTP
      frames with stable SSRC → paces through `MediaFabric::send_packet`.
      `SipEvent::DialogCreated` now carries `media_endpoint` +
      `remote_rtp`; `ControlState` stores them on each `CallSnapshot`.
- [x] `MediaFabric::send_packet(src, dest, bytes)` trait method and
      `UdpMediaFabric` impl — the primitive the `speak` tool uses.
- [x] `smiths-core::{rtp, codec}` modules — pure RTP packet
      builder/parser + G.711 μ-law conversion. `smiths-media` and
      `smiths-testkit` re-export for backward compat.
- [x] `ToolContext` carries `Arc<dyn MediaFabric>` so tools that inject
      audio have a first-class handle.
- [x] Integration test `speak_injects_rtp_into_live_call`: real
      engine, real `ai-tts-mock` plugin subprocess, verifies UA
      receives PCMU RTP with stable SSRC.

### Landed in v0.8.0 (2026-04-18)

- [x] Bidirectional RPCs — plugin-initiated JSON-RPC notifications
      (no `id`) broadcast from `Sidecar` via
      `subscribe_notifications()`. Loader bridge republishes them as
      `Event::Plugin(PluginEvent::Notification { plugin, method,
      params })` on the engine bus. MCP session forwards each as
      `notifications/plugin/{method}` with `{plugin, data}` params.
      `ai-asr-mock` demonstrates `emit_partial` streaming; integration
      test `smiths-plugin/tests/streaming.rs` verifies end-to-end
      delivery through the bus.

### Still pending for Phase 4 completion

- [ ] Plugin → engine request/response host calls (as opposed to
      fire-and-forget notifications) — e.g. `send_audio_chunk` with
      acknowledgement.
- [ ] Length-prefixed protobuf stdio codec (currently JSON-RPC) — optional; for perf-critical `on_rtp_frame` paths
- [ ] `WireFormat` trait in `smiths-proto` — **MVP guardrail for FlatBuffers**
- [ ] Host-call router (plugin → core → reply)
- [ ] `Dispatcher` routing for sidecar tier
- [ ] Optional gRPC-over-UDS transport (feature `sidecar-grpc`)

## Phase 5 — MCP Server

### Landed (unreleased after v0.2.0)

- [x] `Tool` + `ToolRegistry` traits in `smiths-mcp`
- [x] `ControlState` subscriber on event bus with live-call view
- [x] Built-in tools: `list_calls`, `get_call_status`, `health`
- [x] MCP stdio transport (JSON-RPC 2.0 line-delimited)
- [x] CLI `--mcp stdio` mode that suppresses SIP + health HTTP
- [x] A2A HTTP adapter serving the same tool set + agent-card discovery
- [x] Python demos: `mcp_demo.py` (spawns engine), `a2a_demo.py` (HTTP client)
- [x] Claude Code MCP config snippet in the Python README

### Landed (unreleased, control-plane hardening after v0.5.0)

- [x] `Resource` concrete impls + `ResourceRegistry`: `health://status`,
      `sip://calls`, `config://current` (secrets redacted). Adapter-
      agnostic; both MCP stdio and A2A HTTP expose `resources/list` +
      `resources/read`.
- [x] Per-tool token-bucket rate limiter (`smiths-mcp::RateLimiter`),
      wired into a shared `invoke_audited` helper that both adapters
      route every tool call through. Configured via
      `[mcp.rate_limit] per_sec = N, burst = M` (0 = disabled).
- [x] Structured audit log — tracing event per call at target
      `smiths_mcp::audit` with `actor`, `tool`, `args_hash` (SHA-256),
      `outcome`, `duration_ms`, `error`.
- [x] Bearer-token auth for A2A HTTP — `a2a.bearer_token` config gates
      `/a2a`; `/health` and `/.well-known/agent.json` stay public. 5
      integration tests spinning the real axum server.
- [x] `reload_plugin` tool + `AiRegistry::reload` trait method. Drains
      the current sidecar and respawns from the same directory.
- [x] `ToolContext` now carries `Arc<Config>` so resources / tools can
      read engine settings without reaching into CLI wiring.

### Landed (unreleased, MCP over HTTP + SSE)

- [x] `POST /mcp` — JSON-RPC over HTTP. Shares `dispatch` + audit
      + rate-limit + metrics paths with the stdio server, so tools
      behave identically across transports. Actor label `mcp-http`
      distinguishes them in audit events.
- [x] `GET /mcp/events` — server-sent-event stream that forwards
      bus-originated MCP notifications (`call/created`, `call/terminated`)
      as `text/event-stream` with 15 s keep-alive pings.
- [x] `[mcp] enabled_http / http_bind` wired in the CLI — runs
      alongside SIP / health / A2A / stdio.
- [x] 3 integration tests: POST `tools/call`, POST `initialize`
      (resource capability advertised), SSE end-to-end round-trip.

### Landed (unreleased, UAC + outbound call control)

- [x] `smiths-sip::UacClient` — engine-side User Agent Client.
      `place_call(target)` sends an INVITE with an SDP offer built by
      the shared `SdpNegotiator`, waits for 100/200 via the new
      `ResponseRouter`, sends ACK, records the dialog, publishes
      `DialogCreated`. `hangup(call_id)` sends BYE and publishes
      `DialogTerminated`.
- [x] `smiths-sip::ResponseRouter` — branch-keyed oneshot correlator
      shared between UAS (delivers any response it sees) and UAC
      (subscribes per outbound request).
- [x] `smiths-core::call::CallOriginator` trait — the MCP control
      plane consumes this, not the SIP crate directly.
      `SdpNegotiator` gained `build_offer` + `parse_remote_rtp` so
      the UAC can negotiate without depending on `smiths-sdp`.
- [x] `make_call(target)` + `end_call(call_id)` MCP tools. Exposed
      over MCP stdio, MCP HTTP, and A2A. When the UAC isn't configured
      the tools return `NotFound` with a clear message.
- [x] CLI wires a `UacClient` from the first UDP bind, shares
      `ResponseRouter` + `UdpTransport` + `MediaFabric` with the UAS,
      attaches `Arc<dyn CallOriginator>` to the `ToolContext`.
- [x] Integration test `uac_places_call_and_hangs_up_against_fake_uas`
      — engine-side UAC places a call to `FakeUas`, fake answers 200
      + SDP, UAC ACKs, `DialogCreated` fires with media info,
      `hangup` sends BYE → 200, `DialogTerminated` fires.

### Still pending for Phase 5 completion

- [x] Integration tests that spawn the real binary over MCP stdio —
      landed v0.21.0 as
      `crates/smiths-cli/tests/mcp_stdio.rs`.
- [x] Update `docs/architecture/03-mcp-and-ops.md` to document the
      HTTP + SSE transport alongside stdio — landed v0.20.0.

## Phase 5 — MCP Server (original checklist, for reference)

- [ ] `Tool` + `Resource` traits in `smiths-mcp` — **MVP guardrail for A2A**
- [ ] stdio MCP transport (JSON-RPC 2.0)
- [ ] HTTP/SSE MCP transport (feature `mcp-http`)
- [ ] Tool handlers: make_call, end_call, get_call_status, list_calls, plugins.\*
- [ ] Resource handlers: `sip://calls/*`, `plugin://manifests/*`, `config://current`, `metrics://snapshot`, `health://status`
- [ ] Bearer-token auth, per-tool rate limits, audit log

## Phase 6 — Hardening & Ops

### Landed (unreleased, first P6 slice)

- [x] TLS SIP transport (`smiths-sip::TlsTransport`). Rustls + SNI,
      inbound-only for now. PEM cert + key paths in `[sip]
      tls_cert_path / tls_key_path`. Self-signed integration test via
      `rcgen` (`cargo test -p smiths-sip --test tls`). Shared
      stream-framing module with the TCP transport.
- [x] Prometheus exporter. `smiths-core::metrics` registers a core
      set (`sip_requests_total{method}`, `sip_responses_total{code}`,
      `sip_dialogs_active`, `tool_invocations_total{tool,outcome}`,
      `tool_duration_seconds{tool}` histogram). UAS + MCP/A2A tool
      dispatch increment at the hot paths; CLI exposes
      `/metrics` (OpenMetrics text) on the existing health HTTP
      server next to `/health`.

### Still pending for Phase 6

- [ ] SRTP passthrough (`RTP/SAVP` negotiation, opaque payload forwarding)
- [ ] pcap tap (feature `pcap`)
- [ ] Multi-arch Docker images (`x86_64-musl`, `aarch64-musl`)
- [ ] systemd unit + K8s manifests
- [ ] Full e2e suite (TLS + SRTP + WASM + sidecar + metrics + drain)
- [ ] Nightly 24 h SIP fuzz run
- [ ] Release script + tag pipeline
- [ ] Tighten workspace lints (`missing_docs = "warn"`, `clippy::unwrap_used = "warn"`)
      — first slice landed in v0.21.0 (`dbg_macro` / `print_stdout` /
      `print_stderr` / `todo` / `unimplemented` promoted to `warn`,
      zero fires). Remaining two are larger cliffs tracked as
      dedicated slices.

---

## Cross-cutting (ongoing)

- [ ] Keep `docs/architecture/*` in sync when decisions change
- [ ] PR-review check: each new module respects **MVP guardrails** in
      `docs/architecture/04-post-mvp-scope.md §Summary matrix`
- [ ] Bench: add `criterion` benches as hot paths appear
- [ ] Fuzz: parsers continuously
