# smiths-net operator runbook

Living document — extend as new knobs land. Target audience: the
people who run the engine in production. Developer context lives
under `docs/architecture/`.

## Table of contents

1. [Sandbox hardening (Linux seccomp-BPF)](#sandbox-hardening-linux-seccomp-bpf)
2. [Graceful drain](#graceful-drain)
3. [Observability](#observability)
4. [Install / upgrade / rollback](#install--upgrade--rollback) — slice 1.8
5. [Full e2e validation](#full-e2e-validation) — slice 1.9
6. [Cold-start recovery — HA snapshot](#cold-start-recovery--ha-snapshot-slice-61)
7. [Live config changes](#live-config-changes-slices-58-b--58-c) — slices 5.8-b / 5.8-c
8. [Canary config changes + incident response](#canary-config-changes--incident-response-slice-59) — slice 5.9
9. [WebRTC tag-based rendezvous](#webrtc-tag-based-rendezvous-slice-510-bridge) — slice 5.10-bridge

---

## Sandbox hardening (Linux seccomp-BPF)

Introduced in **v0.29.0**. Applies only on Linux; macOS / BSD
ignores the knob silently because they don't expose seccomp.

### Turning it on

```toml
[plugins.sandbox]
no_new_privs = true        # recommended — required for unprivileged seccomp
seccomp      = "allowlist"
```

`no_new_privs` is mandatory unless the engine runs as root:
`seccomp(2)` requires either `CAP_SYS_ADMIN` or the
`PR_SET_NO_NEW_PRIVS` bit, and the latter is what you want in
production (privilege-drop fails closed rather than broadens the
plugin's blast radius).

### What the baseline allows

The allowlist covers what a typical Rust / Python / Node plugin
needs to boot and do work:

| Group                     | Example syscalls                                       |
|---------------------------|--------------------------------------------------------|
| File I/O                  | `read`, `write`, `openat`, `close`, `pread64`, `flock` |
| Memory                    | `mmap`, `mprotect`, `munmap`, `brk`, `madvise`         |
| Event loop                | `epoll_create1`, `epoll_ctl`, `eventfd2`, `futex`      |
| Sockets                   | `socket`, `connect`, `sendto`, `recvfrom`, `accept4`   |
| Process / clock           | `clock_gettime`, `nanosleep`, `rseq`, `prlimit64`      |
| Signals                   | `rt_sigaction`, `kill`, `tgkill`                       |

### What it deliberately denies

Every deny path returns **`EPERM`** (errno 1) rather than
`SIGSYS` — the failing plugin sees a normal "operation not
permitted" and can bail gracefully. You lose the ability to
distinguish "the plugin made a typo in its own code" from "seccomp
blocked this," but you gain a process that logs + exits cleanly
rather than dying mysteriously.

Explicit denials (cherry-picked from "things that let a compromised
plugin escape"):

| Syscall                    | Why we block it                                           |
|----------------------------|-----------------------------------------------------------|
| `mount` / `umount2`        | Filesystem rearrangement → escape container assumptions.  |
| `pivot_root`               | Same, more thorough.                                      |
| `reboot` / `kexec_load`    | Host takeover.                                            |
| `unshare` / `setns`        | Namespace escapes.                                        |
| `bpf`                      | Load arbitrary kernel programs.                           |
| `ptrace`                   | Attach to another process on the host.                    |
| `perf_event_open` / `kcmp` | Covert-channel primitives.                                |

### When a plugin legitimately needs more

Add the syscall to `seccomp_extra_allow`:

```toml
[plugins.sandbox]
seccomp             = "allowlist"
seccomp_extra_allow = ["io_uring_setup", "io_uring_enter"]
```

Unknown syscall names fail fast at spawn — a typo doesn't silently
widen the filter. The symptom to look for is
`seccomp install: unknown syscall in extra-allow list`.

### Debugging a filter-triggered failure

1. Check the plugin's own stderr/stdout. Plugins rarely log
   `EPERM` descriptively; the sidecar wrapper surfaces the exit
   code in `/health`.
2. Temporarily flip `seccomp = "off"` and see if the plugin still
   misbehaves. If it now works, the filter is the cause.
3. Run the plugin outside the engine with
   `strace -ff -e trace=all -o plugin.strace ./my_plugin` and look
   for the last syscall before the failure.
4. Add the missing entry to `seccomp_extra_allow` and restart.

### Observability hooks

The engine emits a `plugin_invocations_total{plugin="...",
outcome="error"}` metric when a sidecar exits abnormally. Combine
with `sidecar_restarts_total` to distinguish
"plugin's own bug" (high invocations.error, low restarts) from
"seccomp kill" (high restarts).

---

## Graceful drain

Send `SIGTERM` — the engine flips a drain flag and refuses new
INVITEs with `503 + Retry-After: 0`. Existing dialogs run to
completion. Tune with `SMITHS_DRAIN_SECS` (default 5 s) for the
wait window before the CLI cancel fires.

---

## Observability

- **Metrics**: Prometheus exposition at the health port, path
  `/metrics`. See `docs/architecture/03-mcp-and-ops.md` for the
  full surface.
- **Health**: `/health` returns JSON with draining, bind, plugin,
  and dialog counts.
- **Structured logs**: `log_format = "json"` in the `[observability]`
  block lands one JSON object per line.

---

## Install / upgrade / rollback

Three supported deployment shapes. Pick whichever matches your
existing infrastructure — the engine binary is identical across all
three.

### Bare-metal via systemd

1. Grab the tarball from the GitHub release page for your arch
   (`smiths-net-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz` or
   `-aarch64-unknown-linux-musl.tar.gz`).
2. Unpack `smiths-net` to `/usr/local/bin/smiths-net`; set
   `chmod +x`.
3. Drop your config at `/etc/smiths-net/config.toml` (example in
   `examples/config.toml`).
4. Install `systemd/smiths-net.service` to
   `/etc/systemd/system/`; run `systemctl daemon-reload` +
   `systemctl enable --now smiths-net`.
5. Watch `journalctl -u smiths-net -f` during first boot.

The unit runs as an unprivileged `smiths:smiths` user — create it
with `useradd --system --shell /usr/sbin/nologin smiths`. The
`CAP_NET_BIND_SERVICE` ambient cap lets it bind 5060 / 5061 without
root.

### Docker

```sh
docker run --rm -d \
  -p 5060:5060/udp -p 5060:5060/tcp -p 5061:5061/tcp \
  -p 8080:8080/tcp \
  -v $(pwd)/config.toml:/etc/smiths-net/config.toml:ro \
  --name smiths-net \
  ghcr.io/mindhalla/smiths-net:vX.Y.Z
```

Multi-arch — the manifest covers `linux/amd64` and `linux/arm64`; the
pull selects the right one automatically.

### Kubernetes

```sh
kubectl apply -f k8s/configmap.yaml
kubectl apply -f k8s/deployment.yaml
kubectl apply -f k8s/service.yaml
```

The deployment is opinionated: 2 replicas, rolling update with
zero `maxUnavailable`, readiness probe that flips false on
SIGTERM (so the LoadBalancer stops sending new traffic during
drain). `terminationGracePeriodSeconds: 30` gives drain room to
finish.

### Upgrade procedure

All three paths follow the same drain-then-swap pattern:

1. Pre-flight: check `/health` on the current deployment —
   `draining` should read `false`, `dialogs_active` should show
   live traffic. Scrape `/metrics` for the baseline.
2. Deploy the new version (systemd: `systemctl restart smiths-net`
   after swapping the binary; docker: `docker pull` + `docker stop`
   + re-run; k8s: `kubectl set image deployment/smiths-net
   smiths-net=<new-image>`).
3. Wait for the readiness probe / health endpoint to go green.
4. Confirm the same metric baseline — dialogs shouldn't have
   dropped below the pre-upgrade count (minus the naturally-ended
   ones).

### Rollback procedure

```sh
# k8s
kubectl rollout undo deployment/smiths-net

# docker
docker stop smiths-net && docker run ... ghcr.io/.../smiths-net:<previous>

# systemd
systemctl stop smiths-net
cp /usr/local/bin/smiths-net.backup /usr/local/bin/smiths-net
systemctl start smiths-net
```

Always verify with `/health` + `/metrics` that the rollback landed
before walking away. If the rollback also fails, check
[journalctl / kubectl logs] for the reason the new version was
unhappy — usually a config-schema change the upgrade step missed.

---

## Full e2e validation

Slice 1.9's full-stack e2e test + nightly 24 h fuzz. Placeholder
— populated in v0.32.0.

---

## Cold-start recovery — HA snapshot (slice 6.1)

The engine can persist its **dialog table** to a JSON file on
graceful shutdown and replay it on the next boot. MVP of the
Phase-6 HA story — a full primary/secondary replicator lands
later (6.2 + 6.3); today's feature covers the "single-node
restart" case.

### Enable

```bash
smiths-net --snapshot-path /var/lib/smiths-net/dialogs.snapshot.json
# or via env var
SMITHS_SNAPSHOT=/var/lib/smiths-net/dialogs.snapshot.json smiths-net
```

When set:

- **Boot**: if the file exists, every record in it is restored
  into the UAS's dialog table before the first UDP bind starts
  accepting traffic. `smiths_snapshot_replay_dialogs_total`
  bumps by the number of records.
- **Shutdown**: once SIP / adapter / health tasks have drained,
  the live dialog table is serialized to the same path. Written
  atomically via `<path>.snapshot.tmp` + rename so a partial
  write can't fool the next boot.

### What's NOT persisted

- Live UDP sockets, bridges, transcoded/UDPTL/conference sessions
- CDR in-progress rows (fire-on-BYE)
- Plugin sidecars (re-launched from `plugins` dir)

An in-progress RTP flow belonging to a pre-restart dialog goes
silent after recovery — the restored record carries endpoint ids
but the media plane starts cold. A subsequent BYE from either
side tears the restored record down.

### Troubleshooting

- **File missing at boot**: normal on first start. Log line
  `HA snapshot file absent; cold boot` confirms.
- **Magic mismatch / version too new**: warn log; cold boot
  proceeds. Delete the file or downgrade the binary.
- **File unwritable at shutdown**: warn log; nothing persisted.

### Limits

- No delta replication — a primary crashing between snapshots
  loses every dialog opened since the last graceful shutdown.
  That's the **6.1 MVP**; 6.2 adds the live replicator.
- Snapshot size ≈ 0.5 KB per record as JSON; 10 000 dialogs
  ≈ 5 MB, well within atomic-rename territory.

---

## Live config changes (slices 5.8-b / 5.8-c)

A running engine accepts config changes without a restart for a
fixed set of fields. Every path — SIGHUP, the `smiths-net reload`
subcommand, the future MCP `put_config` tool — funnels through
`Config::load` → `Config::validate` → `ConfigReloader::apply` so
the same atomic swap + canary / auto-rollback model applies no
matter who triggered it.

### The three trigger paths

| Trigger | When to use | Who watches |
|---------|-------------|-------------|
| `kill -HUP $pid` | Ops scripts that already have the engine's PID (systemd `ReloadSignal=SIGHUP`, `k8s lifecycle preStop`, nagios handlers) | The engine installs the handler at boot unless `--no-reload-signal` is set. |
| `smiths-net reload --pid N` | Humans, CI deploy gates. Validates the file locally first, then delivers SIGHUP with a clear error if the file is malformed. | Same handler as above — the subcommand just sends the signal. |
| MCP `put_config` (slice 7.3) | Agents that already hold an MCP session. | Lands on the same `ConfigReloader::apply` path. |

### Default canary deadline

`[canary] deadline_s` (default **300 s**) is the window between
`apply` and either operator confirm or automatic rollback. The
5.9 error-rate probe watches `plugin_invocations{outcome="error"}`
+ `sip_parse_errors` rates in parallel; whichever arm wins (probe
trip, deadline timer, operator confirm) ends the other two.
Override per-environment via the TOML block:

```toml
[canary]
deadline_s                        = 120
plugin_error_rate_ceiling         = 0.25
sip_parse_errors_per_sec_ceiling  = 5
```

Set `plugin_error_rate_ceiling = 1.0` and
`sip_parse_errors_per_sec_ceiling = u64::MAX` to disable the
error-rate probe entirely — the deadline timer remains active.

### Field × reloadability

| Field | Reload behavior | Adapter |
|-------|-----------------|---------|
| `observability.log_level` | Live | `tracing_subscriber::reload::Handle` |
| `observability.log_format` | **Restart** | JSON vs pretty is a subscriber-layer shape decision, not a runtime swap |
| `observability.health_bind` | **Restart** | Socket rebind |
| `sip.bind` / `sip.transports` / `sip.tls_cert_path` / `sip.tls_key_path` | **Restart** | Requires listener rebind |
| `sip.rate_limit` | Live | `SipRateLimiter::reconfigure` — per-source buckets keep their tokens |
| `media.transcode.max_concurrent_calls` | Live | `CpuBudget::set_max_concurrent` — existing leases unaffected |
| `media.prompts.capacity` | Live | `PromptLibrary::resize` — LRU evicts down to the new cap |
| `ai.openai_api_key` / `ai.anthropic_api_key` | Live (takes effect on next sidecar respawn) | `AiRegistry::set_env` — already-live sidecars keep their old env until reloaded |
| `mcp.*` binds / `a2a.*` binds | **Restart** | Socket rebind |
| `auth.backend` / `storage.backend` | **Restart** | Backends constructed once at boot |
| `plugins.dir` / `plugins.sandbox` | **Restart** | Directory scan + sandbox attach happen at sidecar spawn |

Attempting to SIGHUP a config that changes any **Restart**-marked
field is refused with `ApplyError::RestartRequired { fields }`;
the live config stays untouched and the refusal lands in the
engine's log.

### Worked examples

**Bump the log level without rolling the deployment:**

```sh
# 1. Edit config.toml → observability.log_level = "debug"
# 2. Pre-flight the change without touching the running engine.
smiths-net reload --config /etc/smiths/config.toml --dry-run --diff
#    reloadable: observability.log_level
# 3. Signal the engine.
smiths-net reload --config /etc/smiths/config.toml --pid $(pidof smiths-net)
#    sent SIGHUP to pid 12345; target reloads from /etc/smiths/config.toml
```

Confirm via the engine's logs:

```
INFO smiths_cli: SIGHUP received; reloading config
INFO smiths_cli: SIGHUP reload: canary window armed  id=apply-0 reloaded=["observability.log_level"] deadline_secs=300
```

Check `smiths_config_reloaded_fields_total{field="observability.log_level"}`
on the `/metrics` endpoint bumped by 1 and tail the engine's log
— the new level takes effect immediately on subsequent log calls.

**Rotate an `OPENAI_API_KEY` without dropping live sidecars:**

```sh
# 1. Edit config.toml → ai.openai_api_key = "sk-new-value"
# 2. Apply.
kill -HUP $(pidof smiths-net)
# 3. Respawn the plugin that depends on the key so it picks up
#    the new env — the MCP `reload_plugin` tool (or a full
#    engine restart during a maintenance window) is the honest
#    path for in-flight sidecars.
```

`smiths_config_reloaded_fields_total{field="ai.openai_api_key"}`
increments on the reload; already-running sidecars keep their
prior env until they're reloaded.

### Refusing a change that needs a restart

`Config::validate` catches cross-field invariants before the
apply (`sip.transports` includes `"tls"` but `tls_cert_path`
unset, `[sip.rate_limit]` with `per_sec > 0 && burst == 0`, …).
When a candidate trips a restart-required field, the engine
logs:

```
WARN smiths_cli: SIGHUP reload: apply rejected err=RestartRequired { fields: ["sip bind / transports / tls paths"] }
```

Operator follow-up is a planned maintenance-window restart. Use
`smiths-net reload --dry-run --diff --config new.toml` ahead of
time to see whether a change needs a restart before you schedule
one.

---

## Canary config changes + incident response (slice 5.9)

The 5.8-mvp substrate arms a **canary window** on every non-trivial
apply: the new config is live immediately, but if either the
deadline timer (`[canary] deadline_s`) fires without an operator
`confirm` or the 5.9 error-rate probe trips a ceiling, the prior
`Arc<Config>` is atomically swapped back in. Rollbacks carry a
`reason` label so dashboards tell one trigger from another.

### Dashboard signals

- `smiths_config_canary_active` — gauge, `1` during a pending
  change. Alert when this stays `1` past the configured
  deadline.
- `smiths_config_rollbacks_total{reason}` — histogram counter.
  Reasons: `manual` / `timeout` / `error_budget`.
- `smiths_config_probe_triggered_total{probe}` — which probe
  fired (`plugin_error_rate` / `sip_parse_errors`).
- `smiths_config_reloaded_fields_total{field}` — per-adapter
  bump when the subsystem actually applied the change.

### Incident response — probe-triggered rollback

```
WARN smiths_core::probe: config canary probe tripped; rolling back
      id=apply-17  probe=plugin_error_rate
```

1. **Confirm the revert landed.** `/metrics` should show
   `smiths_config_rollbacks_total{reason="error_budget"}` bumped
   by 1 and `smiths_config_canary_active = 0`.
2. **Identify the culprit.** The probe trips on aggregate plugin
   errors; drill into
   `plugin_invocations_total{outcome="error", plugin=…}` to see
   which plugin went sideways. SIP-parse trips point at an
   upstream that started sending malformed traffic (peer rolled
   to an incompatible build).
3. **Decide**:
   - Config was actually fine and the ceiling is too tight →
     widen `[canary] plugin_error_rate_ceiling` and retry.
   - New plugin version has a real bug → back the plugin out
     (git revert, `plugins.dir` pointed at prior build) and
     re-SIGHUP.
   - Upstream peer is misbehaving → fix on their side; ours was
     a false positive — consider exempting `sip_parse_errors`
     for that apply (`sip_parse_errors_per_sec_ceiling = u64::MAX`
     during the migration).
4. **Re-apply.** Once the underlying cause is fixed, repeat the
   SIGHUP. The canary arms again from scratch.

### Incident response — deadline timeout

```
WARN smiths_core::reloader: config canary deadline fired; rolled back
      id=apply-17
```

Usually means the operator's `confirm` signal didn't land in
time. The prior config is back, so the deployment keeps working
— the next step is to understand *why* confirmation was delayed
(human in the loop, CI job hung, MCP session dropped). Lengthen
`deadline_s` only after confirming the automation path is
reliable; otherwise you're just extending the blast radius.

### Disabling the probe (emergency)

During a planned risky change where the probe would get in the
way:

```toml
[canary]
plugin_error_rate_ceiling         = 1.0          # disable
sip_parse_errors_per_sec_ceiling  = 18446744073709551615  # u64::MAX
deadline_s                        = 60           # shorter window
```

Re-enable afterwards through a second SIGHUP once the rollout is
confirmed healthy.

---

## WebRTC tag-based rendezvous (slice 5.10-bridge)

The WebRTC-native signaling adapter pairs two legs by a shared
`tag` string on their `session-init` frame. A pair can be two
WebRTC sessions (browser-to-browser through one engine) or one
WebRTC leg plus a SIP INVITE carrying the same tag (in a
header the SIP → rendezvous bridge lands with a dedicated
follow-on). The underlying media flows through
`MediaFabric::bridge` — the same path used by plain SIP.

### Semantics

- **First leg with tag `X` arrives** → handler negotiates the
  offer, allocates a media endpoint, runs the DTLS-SRTP
  handshake if the offer used `UDP/TLS/RTP/SAVP[F]`, then
  **parks** the leg under tag `X` in the rendezvous map.
  The answer goes back to the client; the session waits.
- **Second leg with tag `X` arrives** → same negotiate +
  allocate + handshake, then **pulls** the parked partner,
  calls `MediaFabric::bridge`, and bumps
  `smiths_webrtc_sessions_paired_total{partner="webrtc"}`
  (or `partner="sip"` once the SIP path is wired). RTP flows
  in both directions as soon as the bridge returns.
- **`bye` from either side** → releases the bridge; the
  partner leg keeps its own answer buffered and its own
  endpoint until it too sends `bye`.

### Deadline for unpaired legs

Default: **30 seconds** (`smiths_cli::webrtc::DEFAULT_RENDEZVOUS_DEADLINE`).
No operator-facing TOML knob today — if a real deployment
needs a tighter window, open an issue and we'll light up
`[webrtc] rendezvous_deadline_s` in a dedicated slice.

When the deadline fires without a partner:

- The parked endpoint is released — no leaked UDP sockets.
- `smiths_webrtc_sessions_paired_total{partner="none"}`
  increments — dashboards should alert when this rises.
- The parked session is **not** force-byed on the wire today
  (the WebSocket remains open). Browsers observing the
  deadline themselves close the session on their side and
  the engine cleans up its endpoint on the next `bye`. A
  future slice will send an explicit `WtSignal::Bye` when
  the evictor fires.

### SIP INVITEs joining the same map (slice 5.10-sipjoin)

A SIP INVITE carrying `X-Smiths-Webrtc-Tag: <tag>` joins the
same rendezvous map the WebRTC handler uses. If a WebRTC leg
is already parked under that tag, the SIP dialog pairs with
it and a bridge is installed before the 200 OK goes out. If
no WebRTC partner is parked yet, the SIP leg parks — awaiting
its WebRTC half, same deadline applies.

```
INVITE sip:ignored@engine SIP/2.0
...
X-Smiths-Webrtc-Tag: room-42
Content-Type: application/sdp
...
```

`smiths_webrtc_sessions_paired_total{partner="sip"}` bumps
on a successful pair. The SIP UAS stores the resulting
`BridgeId` on its dialog; `BYE` from either side releases
the bridge through the media fabric's idempotent path.

**Security note:** the engine trusts the client's claim on a
tag — there's no pre-authorization of which tag a given
caller can join. For multi-tenant deployments, front the
SIP UAS with an MCP tool that validates the dialed
Request-URI + the header against an allow-list before the
INVITE reaches the engine.

### Troubleshooting

- **Legs never pair** — confirm both clients sent the same tag
  on their `session-init`. The engine logs
  `webrtc rendezvous: leg parked awaiting partner` when the
  first arrives; the second leg should log
  `webrtc rendezvous paired; bridge installed` with matching
  `session=` ids. A mismatch = clients aren't agreeing on the
  tag string.
- **Bridge installs but audio is silent** — usually a
  firewall / SNAT issue between the engine's fabric ports
  and the browser. The DTLS handshake already completed, so
  the peer's SRTP keys are right; the RTP packets are being
  dropped by the network. Check
  `smiths_rtp_packets_forwarded_total` — if it's ticking up,
  the engine side is fine and the client side has the
  problem.
- **`partner="none"` rising slope** — a client flow is
  broken: one side finishes signaling and the other never
  shows up. The evictor logs the tag + session id; pair
  those with client-side telemetry to find the stuck state.
