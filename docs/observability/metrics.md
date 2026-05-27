# Metrics catalog

Every Prometheus metric the engine publishes. Exported on:

- `GET /metrics` — plain Prometheus text format (for scrapers).
- MCP `list_metrics()` — full set as JSON.
- MCP `get_metric(name)` — single metric's current samples.

Both endpoints read from the same `Arc<Mutex<Registry>>`, so scrape
output and control-plane tool output can never disagree.

## SIP signaling (`smiths_core::Metrics`)

| Metric                           | Type    | Labels                 | Alert if                                  |
|----------------------------------|---------|------------------------|-------------------------------------------|
| `sip_requests_total`             | counter | `method`               | 5× baseline rate (DOS probe)              |
| `sip_responses_total`            | counter | `code`                 | `code="5xx"` rate > 1/sec                 |
| `sip_parse_errors_total`         | counter | —                      | > 10/sec (mangled peer or attacker)       |
| `sip_dialogs_active`             | gauge   | —                      | Slope + constant = leaked dialogs         |
| `sip_server_txns_active`         | gauge   | —                      | > 10× peak dialog count                   |
| `sip_invite_2xx_retransmits_total`| counter| —                      | Slope change = lossy ACK or non-ACKing UAC |
| `media_bridges_active`           | gauge   | —                      | Mismatch with `sip_dialogs_active`        |
| `rtp_packets_forwarded_total`    | counter | `direction`            | Asymmetry = one-way audio                 |
| `rtcp_sr_sent_total`             | counter | —                      | Stalled = bridge hung                     |

## AI dispatcher (`smiths_core::Metrics`)

| Metric                              | Type     | Labels                  | Alert if                                  |
|-------------------------------------|----------|-------------------------|-------------------------------------------|
| `smiths_ai_invocations_total`       | counter  | `capability`            | Rate = AI spend proxy                     |
| `smiths_ai_failovers_total`         | counter  | `capability`            | `failovers/invocations > 0.05` (5% fail) |
| `smiths_ai_tokens_total`            | counter  | `provider`, `dir`       | Slope = token spend rate                  |
| `smiths_ai_pipeline_duration_seconds`| histogram| `pipeline`             | p95 > SLO                                 |

## Tools + plugins (`smiths_core::Metrics`)

| Metric                               | Type     | Labels                  | Alert if                                  |
|--------------------------------------|----------|-------------------------|-------------------------------------------|
| `tool_invocations_total`             | counter  | `tool`, `outcome`       | `outcome="error"` rate > 0.05             |
| `tool_duration_seconds`              | histogram| `tool`                  | p99 > 2s                                  |
| `plugin_invocations_total`           | counter  | `plugin`, `outcome`     | Used by slice 5.9 canary                  |
| `plugin_invoke_duration_seconds`     | histogram| `plugin`                | p95 > SLO                                 |
| `sidecar_restarts_total`             | counter  | `plugin`                | > 1/5min = plugin crash-loop              |

## Transcoding (`smiths_transcode::TranscodeMetrics`)

| Metric                                         | Type    | Labels     | Alert if                                   |
|------------------------------------------------|---------|------------|--------------------------------------------|
| `smiths_transcode_active`                      | gauge   | —          | Near `max_concurrent_calls` cap            |
| `smiths_transcode_cpu_ms_total`                | counter | `codec`    | Divergence from predicted per-codec budget |
| `smiths_transcode_admissions_refused_total`    | counter | —          | > 0 = operators need to raise the cap      |

## Mixer / conferencing (`smiths_mixer::MixerMetrics`)

Added in slice 5.12.

| Metric                                     | Type    | Labels                               | Alert if                              |
|--------------------------------------------|---------|--------------------------------------|---------------------------------------|
| `smiths_mixer_conferences_active`          | gauge   | —                                    | Steady growth = create-leak           |
| `smiths_mixer_ticks_total`                 | counter | `conference`                         | Silent conference → tick stalled      |
| `smiths_mixer_dominant_switches_total`     | counter | `conference`                         | Operator UX — no alert                |
| `smiths_mixer_ingress_dropped_total`       | counter | `reason={queue_full,frame_size}`     | `queue_full` > 0 = slow tick          |
| `smiths_mixer_participants_active`         | gauge   | —                                    | `/ conferences_active` = avg size     |

## FAX / T.38 (`smiths_fax::FaxMetrics`)

Added in slice 5.12.

| Metric                                  | Type    | Labels                                                     | Alert if                              |
|-----------------------------------------|---------|------------------------------------------------------------|---------------------------------------|
| `smiths_fax_sessions_active`            | gauge   | —                                                          | Slope = fax volume                    |
| `smiths_fax_datagrams_forwarded_total`  | counter | `direction={a_to_b,b_to_a}`                                | Asymmetry = unidirectional            |
| `smiths_fax_parse_errors_total`         | counter | `kind={truncated,length_overflow,error_recovery,payload_too_large}` | > 0 = buggy peer or MitM     |

## How the MCP tools use this

```text
# List every metric by name + current value
$ curl -X POST http://localhost:7878/rpc \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/call",
         "params":{"name":"list_metrics","arguments":{}}}'

# Query one by name
$ curl -X POST http://localhost:7878/rpc \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/call",
         "params":{"name":"get_metric",
                   "arguments":{"name":"smiths_fax_sessions_active"}}}'
```

`get_metric` accepts both the "bare" metric name
(`plugin_invocations`) and the Prometheus-text form with the
`_total` suffix (`plugin_invocations_total`). Counters get both
aliases transparently.

## See also

- `crates/smiths-core/src/metrics.rs` — SIP + AI + tool metrics.
- `crates/smiths-transcode/src/metrics.rs` — transcoder metrics.
- `crates/smiths-mixer/src/metrics.rs` — mixer metrics.
- `crates/smiths-fax/src/metrics.rs` — FAX metrics.
- `crates/smiths-mcp/src/tools.rs` — `ListMetricsTool`,
  `GetMetricTool` impls.
