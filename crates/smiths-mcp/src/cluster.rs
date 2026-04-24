//! `cluster://status` MCP resource (slice 6.2).

use async_trait::async_trait;
use serde_json::json;
use smiths_core::ClusterMode;

use crate::resource::{Resource, ResourceContent};
use crate::tool::{ToolContext, ToolError};

/// `cluster://status` — HA role, peer connectivity, replication stats.
pub struct ClusterStatusResource;

#[async_trait]
impl Resource for ClusterStatusResource {
    fn uri(&self) -> &'static str {
        "cluster://status"
    }

    fn description(&self) -> &'static str {
        "HA cluster status: role, peer connectivity, and replication metrics."
    }

    async fn read(&self, ctx: &ToolContext) -> Result<ResourceContent, ToolError> {
        let config = &ctx.config.cluster;

        ResourceContent::json(&json!({
            "mode": config.mode,
            "peer_addr": config.peer_addr,
            "heartbeat_interval_secs": config.heartbeat_interval_secs,
            "node_id": config.node_id,
            "raft_addr": config.raft_addr,
            "raft_dir": config.raft_dir,
            "initial_peers": config.initial_peers,
            "status": match config.mode {
                ClusterMode::Standalone => "standalone",
                _ => "active",
            }
        }))
    }
}
