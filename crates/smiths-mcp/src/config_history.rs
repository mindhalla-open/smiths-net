//! `config://history` MCP resource (slice 7.3).
//!
//! In-memory ring buffer of the last N `put_config` calls.
//! Each entry captures timestamp, path, before/after values,
//! and whether it was a dry run.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::resource::{Resource, ResourceContent};
use crate::tool::{ToolContext, ToolError};

/// Maximum entries in the history ring buffer.
const MAX_ENTRIES: usize = 256;

/// One entry in the config change history.
#[derive(Clone, Debug, Serialize)]
pub struct ConfigHistoryEntry {
    /// Unix timestamp of the change.
    pub timestamp_unix: u64,
    /// Dotted config path that was changed.
    pub path: String,
    /// Value before the change (`null` if the path didn't exist).
    pub before: Value,
    /// Value after the change.
    pub after: Value,
    /// Whether this was a dry-run (preview only, not applied).
    pub dry_run: bool,
}

/// Thread-safe ring buffer of config changes.
#[derive(Clone)]
pub struct ConfigHistory {
    entries: Arc<Mutex<VecDeque<ConfigHistoryEntry>>>,
}

impl ConfigHistory {
    /// Create a new empty history.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(VecDeque::with_capacity(MAX_ENTRIES))),
        }
    }

    /// Record a config change. Drops the oldest entry when full.
    pub fn record(&self, path: &str, before: &Option<Value>, after: &Value, dry_run: bool) {
        let entry = ConfigHistoryEntry {
            timestamp_unix: unix_seconds(),
            path: path.to_owned(),
            before: before.clone().unwrap_or(Value::Null),
            after: after.clone(),
            dry_run,
        };
        // Use try_lock to avoid blocking the async runtime; if the
        // lock is contended we drop the entry — acceptable for an
        // in-memory audit trail.
        if let Ok(mut ring) = self.entries.try_lock() {
            if ring.len() >= MAX_ENTRIES {
                ring.pop_front();
            }
            ring.push_back(entry);
        }
    }

    /// Snapshot all entries (oldest first).
    pub async fn snapshot(&self) -> Vec<ConfigHistoryEntry> {
        self.entries.lock().await.iter().cloned().collect()
    }
}

impl Default for ConfigHistory {
    fn default() -> Self {
        Self::new()
    }
}

/// `config://history` MCP resource.
pub struct ConfigHistoryResource;

#[async_trait]
impl Resource for ConfigHistoryResource {
    fn uri(&self) -> &'static str {
        "config://history"
    }

    fn description(&self) -> &'static str {
        "Ring buffer of the last 256 put_config calls with path, before/after, and timestamp."
    }

    async fn read(&self, ctx: &ToolContext) -> Result<ResourceContent, ToolError> {
        let entries = match ctx.config_history.as_ref() {
            Some(h) => h.snapshot().await,
            None => Vec::new(),
        };
        ResourceContent::json(&json!({
            "count": entries.len(),
            "max": MAX_ENTRIES,
            "entries": entries,
        }))
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn record_and_snapshot() {
        let history = ConfigHistory::new();
        history.record(
            "observability.log_level",
            &Some(json!("info")),
            &json!("debug"),
            false,
        );
        history.record("sip.rate_limit", &None, &json!({"per_sec": 100}), true);

        let entries = history.snapshot().await;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "observability.log_level");
        assert!(!entries[0].dry_run);
        assert_eq!(entries[1].path, "sip.rate_limit");
        assert!(entries[1].dry_run);
    }

    #[tokio::test]
    async fn ring_buffer_evicts_oldest() {
        let history = ConfigHistory::new();
        for i in 0..MAX_ENTRIES + 10 {
            history.record(&format!("field_{i}"), &None, &json!(i), false);
        }
        let entries = history.snapshot().await;
        assert_eq!(entries.len(), MAX_ENTRIES);
        // Oldest should be field_10 (first 10 evicted).
        assert_eq!(entries[0].path, "field_10");
    }
}
