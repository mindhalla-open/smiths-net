//! Recording retention sweeper for the filesystem store.

use std::sync::Arc;
use std::time::Duration;

use smiths_core::storage::{RecordingStore, StorageError};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Delete blobs older than `max_age`. The store's prune walks the
/// directory synchronously, so it runs on the blocking pool rather
/// than a runtime worker.
pub(crate) async fn sweep_once(
    store: Arc<dyn RecordingStore>,
    max_age: Duration,
) -> Result<usize, StorageError> {
    tokio::task::spawn_blocking(move || store.prune_older_than(max_age))
        .await
        .map_err(|e| StorageError::Backend(format!("retention sweep task failed: {e}")))?
}

/// Sweep once at boot then hourly until `cancel` fires.
/// `retention_days = 0` disables the sweeper (returns `None`).
pub(crate) fn spawn_recording_retention_sweeper(
    store: Arc<dyn RecordingStore>,
    retention_days: u32,
    cancel: CancellationToken,
) -> Option<JoinHandle<()>> {
    if retention_days == 0 {
        return None;
    }
    let max_age = Duration::from_secs(u64::from(retention_days) * 86_400);
    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_hours(1));
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                _ = ticker.tick() => {
                    match sweep_once(Arc::clone(&store), max_age).await {
                        Ok(n) if n > 0 => {
                            info!(removed = n, retention_days,
                                "recording retention sweep removed expired blobs");
                        }
                        Ok(_) => {}
                        Err(e) => warn!(?e, "recording retention sweep failed"),
                    }
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smiths_core::FsRecordingStore;

    #[tokio::test]
    async fn sweep_once_prunes_only_expired_blobs_off_the_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn RecordingStore> = Arc::new(FsRecordingStore::new(dir.path()).unwrap());
        store.put("old", b"x").unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        store.put("new", b"y").unwrap();
        let removed = sweep_once(Arc::clone(&store), Duration::from_millis(30))
            .await
            .unwrap();
        assert_eq!(removed, 1);
        assert_eq!(store.list().unwrap(), vec!["new".to_string()]);
    }

    #[test]
    fn zero_retention_disables_sweeper() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn RecordingStore> = Arc::new(FsRecordingStore::new(dir.path()).unwrap());
        assert!(spawn_recording_retention_sweeper(store, 0, CancellationToken::new()).is_none());
    }
}
