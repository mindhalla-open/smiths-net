//! `cluster://status` MCP resource.
//!
//! The resource always reports the configured HA shape (`mode`,
//! peers, Raft settings). Whether the cluster is actually healthy
//! is only known to the replication layer, which the engine exposes
//! through a [`ClusterStatusSource`] attached with
//! [`crate::ToolContext::with_cluster_status`]. Without one, the
//! resource says so explicitly (`status: "unverified"`,
//! `verified: false`) rather than guessing.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::resource::{Resource, ResourceContent};
use crate::tool::{ToolContext, ToolError};

/// Live view over the HA layer: role, peer connectivity, replication
/// lag. Implemented by the replication / Raft runtime; the resource
/// merges the returned object into its payload verbatim.
pub trait ClusterStatusSource: Send + Sync {
    /// Current cluster status as a JSON object. A `status` string
    /// (e.g. `"leader"`, `"follower"`, `"degraded"`) is expected;
    /// every other key is passed through to the reader.
    fn status(&self) -> Value;
}

impl<F> ClusterStatusSource for F
where
    F: Fn() -> Value + Send + Sync,
{
    fn status(&self) -> Value {
        self()
    }
}

/// `cluster://status` — configured HA role plus, when a
/// [`ClusterStatusSource`] is attached, the live role, peer
/// connectivity, and replication stats.
pub struct ClusterStatusResource;

#[async_trait]
impl Resource for ClusterStatusResource {
    fn uri(&self) -> &'static str {
        "cluster://status"
    }

    fn description(&self) -> &'static str {
        "HA cluster status: configured role plus live role, peer connectivity, \
         and replication metrics when a status source is attached."
    }

    async fn read(&self, ctx: &ToolContext) -> Result<ResourceContent, ToolError> {
        let config = &ctx.config.cluster;
        let mut out = Map::new();
        out.insert("mode".into(), json!(config.mode));
        out.insert("peer_addr".into(), json!(config.peer_addr));
        out.insert(
            "heartbeat_interval_secs".into(),
            json!(config.heartbeat_interval_secs),
        );
        out.insert("node_id".into(), json!(config.node_id));
        out.insert("raft_addr".into(), json!(config.raft_addr));
        out.insert("raft_dir".into(), json!(config.raft_dir));
        out.insert("initial_peers".into(), json!(config.initial_peers));

        match ctx.cluster_status.as_ref().map(|s| s.status()) {
            Some(Value::Object(live)) => {
                out.insert("verified".into(), json!(true));
                out.insert("status".into(), json!("unknown"));
                for (k, v) in live {
                    out.insert(k, v);
                }
            }
            Some(other) => {
                out.insert("verified".into(), json!(true));
                out.insert("status".into(), json!("unknown"));
                out.insert("live".into(), other);
            }
            None => {
                out.insert("verified".into(), json!(false));
                out.insert("status".into(), json!("unverified"));
            }
        }
        ResourceContent::json(&Value::Object(out))
    }
}

/// Convenience for embedders: wrap a closure as a status source.
#[must_use]
pub fn status_source<F>(f: F) -> Arc<dyn ClusterStatusSource>
where
    F: Fn() -> Value + Send + Sync + 'static,
{
    Arc::new(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ControlState;
    use crate::tool::test_support::{empty_registry, null_media};
    use smiths_core::{ClusterMode, Config, EventBus};
    use tokio_util::sync::CancellationToken;

    fn ctx(mode: ClusterMode) -> (ToolContext, CancellationToken) {
        let bus = EventBus::new(8);
        let cancel = CancellationToken::new();
        let (state, _task) = ControlState::spawn(&bus, cancel.clone());
        let mut config = Config::default();
        config.cluster.mode = mode;
        (
            ToolContext::new(state, empty_registry(), Arc::new(config), null_media()),
            cancel,
        )
    }

    async fn read(ctx: &ToolContext) -> Value {
        let content = ClusterStatusResource.read(ctx).await.unwrap();
        serde_json::from_str(content.text()).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn without_a_source_status_is_unverified_for_every_mode() {
        for mode in [
            ClusterMode::Standalone,
            ClusterMode::Primary,
            ClusterMode::Secondary,
        ] {
            let (c, _cancel) = ctx(mode);
            let v = read(&c).await;
            assert_eq!(v["status"], "unverified", "{mode:?}: {v}");
            assert_eq!(v["verified"], false, "{mode:?}: {v}");
            assert_ne!(v["status"], "active");
            assert_eq!(v["mode"], json!(mode));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn with_a_source_live_fields_are_merged_and_verified() {
        let (c, _cancel) = ctx(ClusterMode::Primary);
        let c = c.with_cluster_status(status_source(|| {
            json!({
                "status": "leader",
                "peers": [{"addr": "10.0.0.2:8000", "connected": true}],
                "replication_lag_ms": 12,
            })
        }));
        let v = read(&c).await;
        assert_eq!(v["verified"], true);
        assert_eq!(v["status"], "leader");
        assert_eq!(v["peers"][0]["connected"], true);
        assert_eq!(v["replication_lag_ms"], 12);
        // Config-derived fields survive the merge.
        assert_eq!(v["mode"], "primary");
        assert_eq!(v["node_id"], 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn source_without_status_key_reports_unknown_not_active() {
        let (c, _cancel) = ctx(ClusterMode::Secondary);
        let c = c.with_cluster_status(status_source(|| json!({"peers": []})));
        let v = read(&c).await;
        assert_eq!(v["verified"], true);
        assert_eq!(v["status"], "unknown");
    }
}
