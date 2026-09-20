//! Prometheus registry introspection: `list_metrics`, `get_metric`.
//! Both read the same registry the `/metrics` endpoint encodes from,
//! so tool output and scrape output cannot diverge.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::require_str;
use crate::tool::{Tool, ToolContext, ToolError};

/// `list_metrics` — return every Prometheus series the engine
/// publishes as JSON.
pub struct ListMetricsTool;

#[async_trait]
impl Tool for ListMetricsTool {
    fn name(&self) -> &'static str {
        "list_metrics"
    }

    fn description(&self) -> &'static str {
        "Return every Prometheus metric the engine publishes as \
         JSON. Reads from the same registry `/metrics` encodes from."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn call(&self, _args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let text = encode_registry_as_text(ctx).await?;
        let series = parse_prometheus_text(&text);
        Ok(json!({
            "count": series.len(),
            "series": series,
        }))
    }
}

/// `get_metric(name)` — return just the series whose name matches
/// `name`. Returns `NotFound` when no series does; returns every
/// matching label combination when multiple do.
pub struct GetMetricTool;

#[async_trait]
impl Tool for GetMetricTool {
    fn name(&self) -> &'static str {
        "get_metric"
    }

    fn description(&self) -> &'static str {
        "Return one Prometheus metric by name. Name matches the \
         series' `# TYPE` identifier (e.g. `sip_dialogs_active`, \
         `smiths_mixer_conferences_active`)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Metric name, e.g. `smiths_fax_sessions_active`."
                }
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let name = require_str(&args, "name")?;
        let text = encode_registry_as_text(ctx).await?;
        let total = format!("{name}_total");
        let series: Vec<_> = parse_prometheus_text(&text)
            .into_iter()
            .filter(|s| {
                s.get("metric")
                    .and_then(Value::as_str)
                    .is_some_and(|m| m == name || m == total)
            })
            .collect();
        if series.is_empty() {
            return Err(ToolError::NotFound(format!("no such metric: {name}")));
        }
        Ok(json!({
            "name": name,
            "series": series,
        }))
    }
}

async fn encode_registry_as_text(ctx: &ToolContext) -> Result<String, ToolError> {
    let registry = ctx
        .metrics_registry
        .as_ref()
        .ok_or_else(|| ToolError::NotFound("metrics registry not wired on this engine".into()))?;
    let guard = registry.lock().await;
    let mut out = String::new();
    prometheus_client::encoding::text::encode(&mut out, &guard)
        .map_err(|e| ToolError::Internal(format!("metrics encode: {e}")))?;
    Ok(out)
}

/// Parse the Prometheus text-format output the encoder produces
/// into `[{"metric":..., "labels":..., "value":...},...]`. Skips
/// `# HELP` / `# TYPE` / `# EOF` lines and blank lines. Tolerates
/// float / integer values; rejects histograms (we don't render
/// them here — `/metrics` still does).
fn parse_prometheus_text(text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((lhs, value_part)) = line.rsplit_once(' ') else {
            continue;
        };
        let Ok(value) = value_part.parse::<f64>() else {
            continue;
        };
        let (metric, labels) = if let Some(brace_open) = lhs.find('{') {
            let metric = &lhs[..brace_open];
            let labels_str = &lhs[brace_open + 1..lhs.len().saturating_sub(1)];
            (metric, parse_label_set(labels_str))
        } else {
            (lhs, serde_json::Map::new())
        };
        out.push(json!({
            "metric": metric,
            "labels": Value::Object(labels),
            "value": value,
        }));
    }
    out
}

/// Parse the key-value pairs inside a Prometheus label set
/// (`k="v",k2="v2"`). Values are always quoted; commas inside
/// quoted values are rare enough in our cardinality space that we
/// don't bother with a full escape-aware parser.
fn parse_label_set(s: &str) -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    for pair in s.split(',') {
        let Some((k, v)) = pair.trim().split_once('=') else {
            continue;
        };
        let v = v.trim().trim_start_matches('"').trim_end_matches('"');
        map.insert(k.to_owned(), Value::String(v.to_owned()));
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::test_support::TestEngine;
    use prometheus_client::metrics::counter::Counter;
    use prometheus_client::metrics::gauge::Gauge;
    use prometheus_client::registry::Registry;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test(flavor = "multi_thread")]
    async fn list_metrics_without_registry_is_not_found() {
        let engine = TestEngine::new();
        let err = ListMetricsTool
            .call(json!({}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_metrics_returns_empty_series_for_empty_registry() {
        let engine = TestEngine::new();
        let ctx = engine
            .ctx
            .clone()
            .with_metrics_registry(Arc::new(Mutex::new(Registry::default())));
        let out = ListMetricsTool.call(json!({}), &ctx).await.unwrap();
        assert_eq!(out["count"], 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_metrics_renders_series_from_shared_registry() {
        let engine = TestEngine::new();
        let mut reg = Registry::default();
        let gauge: Gauge<i64, std::sync::atomic::AtomicI64> = Gauge::default();
        gauge.set(7);
        let ctr: Counter<u64, std::sync::atomic::AtomicU64> = Counter::default();
        ctr.inc_by(42);
        reg.register("sip_dialogs_active", "Dialogs", gauge);
        reg.register("plugin_invocations", "Plugin invocations", ctr);
        let ctx = engine
            .ctx
            .clone()
            .with_metrics_registry(Arc::new(Mutex::new(reg)));

        let out = ListMetricsTool.call(json!({}), &ctx).await.unwrap();
        assert!(out["count"].as_u64().unwrap() >= 2);

        let got = GetMetricTool
            .call(json!({"name": "sip_dialogs_active"}), &ctx)
            .await
            .unwrap();
        assert_eq!(got["name"], "sip_dialogs_active");
        let series = got["series"].as_array().unwrap();
        assert_eq!(series.len(), 1);
        assert_eq!(series[0]["value"], 7.0);

        // Counter emits `_total` suffix; matcher accepts both names.
        let ctr_got = GetMetricTool
            .call(json!({"name": "plugin_invocations"}), &ctx)
            .await
            .unwrap();
        let series = ctr_got["series"].as_array().unwrap();
        assert_eq!(series.len(), 1);
        assert_eq!(series[0]["value"], 42.0);

        let missing = GetMetricTool
            .call(json!({"name": "nope_not_here"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(missing, ToolError::NotFound(_)));
    }

    #[test]
    fn parses_labelled_series() {
        let text = "# HELP x y\n# TYPE x gauge\nx{a=\"1\",b=\"two\"} 3.5\nplain 2\n# EOF\n";
        let series = parse_prometheus_text(text);
        assert_eq!(series.len(), 2);
        assert_eq!(series[0]["metric"], "x");
        assert_eq!(series[0]["labels"]["b"], "two");
        assert_eq!(series[0]["value"], 3.5);
        assert_eq!(series[1]["metric"], "plain");
    }
}
