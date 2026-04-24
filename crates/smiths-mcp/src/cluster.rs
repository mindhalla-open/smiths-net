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

        // Today we return the config view; the 6.2 follow-on in smiths-cli
        // will add live stats (last heartbeat, total deltas) to the
        // ToolContext / GlobalState if needed.
        ResourceContent::json(&json!({
            "mode": config.mode,
            "peer_addr": config.peer_addr,
            "heartbeat_interval_secs": config.heartbeat_interval_secs,
            "status": match config.mode {
                ClusterMode::Standalone => "standalone",
                _ => "active", // Simplified for MVP
            }
        }))
    }
}
