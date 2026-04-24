//! Replication service for HA (slice 6.2).

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use smiths_core::{DialogDelta, DialogKey, DialogRecord, Replicator};

/// Primary replicator that pushes deltas to a channel.
pub(crate) struct PrimaryReplicator {
    tx: mpsc::Sender<DialogDelta>,
}

impl PrimaryReplicator {
    pub(crate) fn new(tx: mpsc::Sender<DialogDelta>) -> Self {
        Self { tx }
    }
}

impl Replicator for PrimaryReplicator {
    fn replicate(&self, delta: DialogDelta) {
        let _ = self.tx.try_send(delta);
    }
}

/// Run the primary replication server.
/// Listens for a connection from the secondary and streams deltas.
pub(crate) async fn run_primary_service(
    addr: SocketAddr,
    mut rx: mpsc::Receiver<DialogDelta>,
) -> Result<(), std::io::Error> {
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "HA Primary replication service listening");

    loop {
        let (mut socket, peer) = listener.accept().await?;
        info!(%peer, "HA Secondary connected");

        // Simple MVP: only one secondary at a time.
        // We consume deltas and send them to the secondary.
        while let Some(delta) = rx.recv().await {
            let json = match serde_json::to_string(&delta) {
                Ok(j) => j,
                Err(e) => {
                    error!(?e, "Failed to serialize delta");
                    continue;
                }
            };
            if let Err(e) = socket.write_all(format!("{json}\n").as_bytes()).await {
                warn!(%peer, ?e, "Secondary disconnected");
                break;
            }
        }
    }
}

/// Run the secondary replication client.
/// Connects to the primary, receives deltas, and applies them to the local table.
pub(crate) async fn run_secondary_service(
    addr: SocketAddr,
    dialogs: Arc<dashmap::DashMap<DialogKey, DialogRecord>>,
) -> Result<(), std::io::Error> {
    loop {
        info!(%addr, "HA Secondary connecting to primary...");
        let socket = match TcpStream::connect(addr).await {
            Ok(s) => s,
            Err(e) => {
                warn!(%addr, ?e, "Failed to connect to primary, retrying in 5s");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        info!(%addr, "HA Secondary connected to primary");
        let mut reader = BufReader::new(socket).lines();

        while let Ok(Some(line)) = reader.next_line().await {
            let delta: DialogDelta = match serde_json::from_str(&line) {
                Ok(d) => d,
                Err(e) => {
                    error!(?e, "Failed to deserialize delta from primary");
                    continue;
                }
            };

            debug!(?delta, "Applying delta from primary");
            apply_delta(&dialogs, delta);
        }

        warn!(%addr, "Primary disconnected, retrying in 5s");
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

fn apply_delta(dialogs: &dashmap::DashMap<DialogKey, DialogRecord>, delta: DialogDelta) {
    match delta {
        DialogDelta::Upsert(record_box) => {
            let record = *record_box;
            dialogs.insert(record.key(), record);
        }
        DialogDelta::Delete(key) => {
            dialogs.remove(&key);
        }
    }
}
