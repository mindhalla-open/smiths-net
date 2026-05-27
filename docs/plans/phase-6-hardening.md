# Phase 6 — Hardening & Ops

**Goal**: production-shaped release. TLS SIP, SRTP passthrough, Prometheus
metrics, Docker/systemd/K8s artifacts, a proper e2e test suite, and
documented failure modes.

## Deliverables

1. TLS 1.2+ SIP transport (`smiths-sip::transport::tls`), SNI-aware, mTLS
   optional.
2. SRTP passthrough — negotiate `RTP/SAVP`, forward encrypted frames
   unchanged (no key derivation in v1).
3. Prometheus metrics endpoint bound per `[observability].metrics_bind`
   exporting the metric set in `architecture/03-mcp-and-ops.md`.
4. pcap tap behind feature `pcap`, toggled via MCP `start_capture`.
5. Packaging artifacts:
   - `Dockerfile` (scratch base, < 20 MB image).
   - `systemd/smiths-net.service`.
   - `k8s/` example Deployment + ConfigMap + Service.
   - `examples/config.toml` fully commented.
6. Full e2e test suite in `crates/smiths-testkit/tests/e2e/`:
   - Two UAs bridged, WASM + sidecar plugins active, metrics scraped,
     graceful shutdown during an active call.
7. Release pipeline:
   - Cross-compile to `x86_64-unknown-linux-musl` and
     `aarch64-unknown-linux-musl`.
   - Tag-triggered release builds Docker image + uploads binaries.
8. Docs pass:
   - `docs/` re-read, inconsistencies corrected, `README.md` written for
     end users.
   - `CHANGELOG.md` from v0.0.0 onward.

## Step-by-step tasks

1. **TLS transport**
   - `rustls` via `tokio-rustls`. Load cert+key at startup; reload on
     SIGHUP (v2).
   - Accept both server-only TLS and mTLS (client cert optional).
   - Integration test with `sipp` over TLS.
2. **SRTP passthrough**
   - Detect `RTP/SAVP` in SDP; if both sides agree, forward frames
     without parsing payload.
   - Still track SSRC + seq for router bookkeeping — we parse RTP header
     (unencrypted) but leave the payload alone.
   - Reject mixed SAVP/AVP unless config allows downgrade.
3. **Metrics exporter**
   - `metrics` crate with a Prometheus recorder; `axum` endpoint at
     `metrics_bind`.
   - Emit the metric set defined in `03-mcp-and-ops.md`. Add CI check
     that all declared metrics appear under a synthetic workload.
4. **pcap tap**
   - Per-call file under `observability.pcap_dir` (default
     `/var/lib/smiths/pcap`).
   - Captures UDP 5060, TCP 5060, configured RTP port range only.
   - Rotation: one file per call, named `<call_id>.pcap`.
5. **Docker image**
   - `FROM scratch`, copy binary, copy CA certs, copy default config.
   - Multi-arch build via `docker buildx`.
6. **systemd unit**
   - Unprivileged user `smiths`, `CAP_NET_BIND_SERVICE` for port 5060/5061.
   - `ReadOnlyPaths`, `ProtectSystem=strict`, `PrivateTmp=true`.
7. **K8s manifests**
   - Deployment with resource requests/limits.
   - Service of type `LoadBalancer` (operator decides LB behavior).
   - ConfigMap mounted at `/etc/smiths/config.toml`.
   - Liveness: `/health`; readiness: same with stricter checks.
8. **e2e suite**
   - Reuses `smiths-testkit` helpers.
   - Covers: TLS SIP, SRTP passthrough, WASM + sidecar together, metrics
     assertions, graceful shutdown with draining.
9. **Fuzz sustained run** — 24-hour fuzz of SIP parser in CI nightly.
10. **Release script** (`scripts/release.sh`):
    - Bumps `Cargo.toml` versions, updates `CHANGELOG.md`, tags, pushes.
    - CI publishes artifacts on tag.

## Acceptance criteria

- [ ] `pjsua` TLS register + INVITE succeeds with a self-signed cert.
- [ ] Two UAs using SRTP (via pjsua `--use-srtp 2`) can bridge through
  the engine; media is audible; engine does not see plaintext.
- [ ] `curl :9090/metrics` returns a text response with the full metric
  set during a live call.
- [ ] `docker run` with the published image starts and answers OPTIONS
  within 2 s of boot.
- [ ] Graceful shutdown during an active call: engine waits up to
  `shutdown.grace` (default 30 s) for BYE, then forces teardown with
  proper SIP `BYE` sent to both legs.
- [ ] Nightly fuzz run finds no new panics for 24 h.

## Out of scope

- Full SRTP key negotiation / DTLS-SRTP (v2).
- Multi-node clustering and call replication (explicitly deferred per
  spec §7).
- OAuth/SAML for MCP (backlog).

## Risks & notes

- TLS cert reload without drop is tricky; v1 requires a restart.
  Document it clearly.
- SRTP passthrough metrics will show "zero loss, unknown jitter" — jitter
  depends on unencrypted RTP header fields, which we still have.
- Don't let the metrics endpoint become another plugin-reachable surface
  by accident; it stays HTTP-only, on a private bind.
