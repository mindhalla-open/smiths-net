//! SIP listener bring-up shared by the UDP, TCP and TLS transports.
//!
//! [`spawn_sip`] binds the transport named by [`SipListenerKind`]
//! and hands it to one generic builder that composes the UAS from a
//! [`SipListenerDeps`] bundle, so the three transports cannot drift
//! apart in what they wire. Every listener also exposes a
//! [`DialogHangup`] handle the shutdown driver uses to BYE live
//! dialogs during drain.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context as _;
use async_trait::async_trait;
use bytes::Bytes;
use smiths_core::call::CallOriginator;
use smiths_core::{
    Drain, Event, EventBus, MediaFabric, Metrics, Replicator, SdpNegotiator, SipEvent,
    SipProxyConfig, WebRtcRendezvous,
};
use smiths_sdp::Negotiator;
use smiths_sip::uas::SessionTimerConfig;
use smiths_sip::{
    ConferenceOrchestrator, Datagram, ResponseRouter, SipRateLimiter, TcpTransport, TlsTransport,
    TranscodeOrchestrator, Transport, UacClient, UasServer, UdpTransport,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::replication_service::DialogTable;

/// Which transport a listener speaks.
#[derive(Clone, Debug)]
pub(crate) enum SipListenerKind {
    /// RFC 3261 UDP.
    Udp,
    /// RFC 3261 TCP (outbound connects honor `[sip.proxy]`).
    Tcp,
    /// RFC 5630 TLS over TCP.
    Tls {
        /// PEM certificate path.
        cert: PathBuf,
        /// PEM private-key path.
        key: PathBuf,
    },
}

impl SipListenerKind {
    /// URI scheme used in `/health` and log lines.
    pub(crate) fn scheme(&self) -> &'static str {
        match self {
            Self::Udp => "udp",
            Self::Tcp => "tcp",
            Self::Tls { .. } => "tls",
        }
    }
}

/// Engine-wide handles every SIP listener composes into its UAS.
#[derive(Clone)]
pub(crate) struct SipListenerDeps {
    pub bus: EventBus,
    pub cancel: CancellationToken,
    pub media_fabric: Arc<dyn MediaFabric>,
    pub metrics: Arc<Metrics>,
    pub router: Arc<ResponseRouter>,
    pub drain: Drain,
    pub rate_limit: SipRateLimiter,
    pub registrar: Option<smiths_sip::auth::digest::Registrar>,
    pub webrtc_rendezvous: Option<Arc<dyn WebRtcRendezvous>>,
    pub replicator: Arc<dyn Replicator>,
    pub dialogs_shared: Option<Arc<DialogTable>>,
    pub conference_orchestrator: Option<Arc<dyn ConferenceOrchestrator>>,
    pub conference_prefix: Option<String>,
    pub transcode_orchestrator: Option<Arc<dyn TranscodeOrchestrator>>,
    pub sdp_advertise_ip: Option<IpAddr>,
    pub proxy: SipProxyConfig,
    pub session_timer: SessionTimerConfig,
    pub max_call_duration: Option<Duration>,
}

/// What a listener bring-up produced.
pub(crate) struct SpawnedSip {
    /// Reader + UAS tasks.
    pub handles: Vec<JoinHandle<()>>,
    /// Outbound-call origin, only on the bind designated for it.
    pub originator: Option<Arc<dyn CallOriginator>>,
    /// This listener's dialog table (the shared one in HA modes).
    pub dialogs: Arc<DialogTable>,
    /// Drain-time hangup handle.
    pub hangup: Arc<dyn DialogHangup>,
}

/// Bind `kind` on `bind` and run a UAS on it. `build_uac` makes
/// this listener the engine's outbound-call origin; `restore`
/// replays HA snapshot records into the UAS before it starts.
pub(crate) async fn spawn_sip(
    kind: &SipListenerKind,
    bind: SocketAddr,
    deps: &SipListenerDeps,
    build_uac: bool,
    restore: Vec<smiths_core::DialogRecord>,
) -> anyhow::Result<SpawnedSip> {
    let (tx, rx) = mpsc::channel::<Datagram>(1024);
    match kind {
        SipListenerKind::Udp => {
            let transport = Arc::new(
                UdpTransport::bind(bind)
                    .await
                    .with_context(|| format!("binding UDP on {bind}"))?,
            );
            let reader = transport.spawn_reader(tx, deps.cancel.clone());
            run_uas(kind, transport, rx, reader, deps, build_uac, restore)
        }
        SipListenerKind::Tcp => {
            let mut transport = TcpTransport::bind(bind)
                .await
                .with_context(|| format!("binding TCP on {bind}"))?;
            // Outbound connects ride the operator's proxy; inbound
            // accepts are untouched.
            let connector = smiths_sip::transport::proxy::connector_from_config(&deps.proxy)
                .context("building sip.proxy connector")?;
            let label = connector.label();
            transport = transport.with_proxy(connector);
            if label != "direct" {
                info!(mode = label, ?deps.proxy.address, "sip outbound proxy engaged");
            }
            let transport = Arc::new(transport);
            let reader = transport.spawn_reader(tx, deps.cancel.clone());
            run_uas(kind, transport, rx, reader, deps, build_uac, restore)
        }
        SipListenerKind::Tls { cert, key } => {
            let transport = Arc::new(
                TlsTransport::bind(bind, cert, key)
                    .await
                    .with_context(|| format!("binding TLS on {bind}"))?,
            );
            let reader = transport.spawn_reader(tx, deps.cancel.clone());
            run_uas(kind, transport, rx, reader, deps, build_uac, restore)
        }
    }
}

/// Transport-agnostic half of [`spawn_sip`]: compose the UAS, replay
/// the snapshot, start the loop, optionally build the UAC.
fn run_uas<T: Transport>(
    kind: &SipListenerKind,
    transport: Arc<T>,
    rx: mpsc::Receiver<Datagram>,
    reader: JoinHandle<()>,
    deps: &SipListenerDeps,
    build_uac: bool,
    restore: Vec<smiths_core::DialogRecord>,
) -> anyhow::Result<SpawnedSip> {
    let local = transport.local_addr()?;
    // One negotiator per bind so `o=` / `c=` carry that bind's IP.
    let negotiator: Arc<dyn SdpNegotiator> = Arc::new(Negotiator::with_default_codecs(local.ip()));
    let mut server = UasServer::new(
        Arc::clone(&transport),
        deps.bus.clone(),
        Arc::clone(&deps.media_fabric),
        Arc::clone(&negotiator),
    )
    .with_context(|| format!("building UAS on {local}"))?
    .with_metrics(Arc::clone(&deps.metrics))
    .with_response_router(Arc::clone(&deps.router))
    .with_drain(deps.drain.clone())
    .with_rate_limit(deps.rate_limit.clone())
    .with_replicator(Arc::clone(&deps.replicator))
    .with_session_timer(deps.session_timer)
    .with_max_call_duration(deps.max_call_duration);
    if let Some(ip) = deps.sdp_advertise_ip {
        server = server.with_sdp_advertise_ip(ip);
    }
    if let Some(orch) = &deps.conference_orchestrator {
        server = server.with_conference_orchestrator(Arc::clone(orch));
        if let Some(prefix) = &deps.conference_prefix {
            server = server.with_conference_rooms(prefix.clone());
        }
    }
    if let Some(orch) = &deps.transcode_orchestrator {
        server = server.with_transcode_orchestrator(Arc::clone(orch));
    }
    if let Some(d) = &deps.dialogs_shared {
        server = server.with_dialogs(Arc::clone(d));
    }
    if let Some(reg) = &deps.registrar {
        server = server.with_registrar(reg.clone());
    }
    if let Some(rdv) = &deps.webrtc_rendezvous {
        server = server.with_webrtc_rendezvous(Arc::clone(rdv));
    }
    let restored = server.restore_dialogs(restore);
    if restored > 0 {
        deps.metrics.snapshot_replay_dialogs.inc_by(restored as u64);
        info!(restored, "HA snapshot replay: dialog records restored");
    }
    let dialogs = server.dialogs_handle();
    let hangup: Arc<dyn DialogHangup> = Arc::new(UasHangup {
        transport: Arc::clone(&transport),
        local,
        dialogs: Arc::clone(&dialogs),
        media: Arc::clone(&deps.media_fabric),
        bus: deps.bus.clone(),
        metrics: Arc::clone(&deps.metrics),
    });
    let server_handle = tokio::spawn(server.run(rx, deps.cancel.clone()));
    info!(%local, scheme = kind.scheme(), "SIP listening");

    let originator: Option<Arc<dyn CallOriginator>> = if build_uac {
        let uac = Arc::new(UacClient::new(
            transport,
            deps.bus.clone(),
            Arc::clone(&deps.media_fabric),
            negotiator,
            Arc::clone(&deps.router),
            local,
            Arc::clone(&deps.metrics),
        ));
        info!(%local, "SIP UAC ready");
        Some(uac)
    } else {
        None
    };

    Ok(SpawnedSip {
        handles: vec![reader, server_handle],
        originator,
        dialogs,
        hangup,
    })
}

/// Drain-time control over a listener's live dialogs.
#[async_trait]
pub(crate) trait DialogHangup: Send + Sync {
    /// Send an in-dialog BYE to every peer, release the dialogs'
    /// media and drop their records. Returns how many were hung up.
    async fn hangup_all(&self) -> usize;
    /// Dialogs still recorded on this listener.
    fn active_dialogs(&self) -> usize;
}

/// [`DialogHangup`] over a UAS's dialog table + transport.
struct UasHangup<T: Transport> {
    transport: Arc<T>,
    local: SocketAddr,
    dialogs: Arc<DialogTable>,
    media: Arc<dyn MediaFabric>,
    bus: EventBus,
    metrics: Arc<Metrics>,
}

#[async_trait]
impl<T: Transport> DialogHangup for UasHangup<T> {
    async fn hangup_all(&self) -> usize {
        let keys: Vec<_> = self.dialogs.iter().map(|e| e.key().clone()).collect();
        let mut hung_up = 0;
        for key in keys {
            let Some((_, record)) = self.dialogs.remove(&key) else {
                continue;
            };
            hung_up += 1;
            self.metrics.dialogs_active.dec();
            let bye = Bytes::from(build_bye(&record, self.local));
            let dest = record.peer_signal;
            let call_id = record.call_id.clone();
            // Two sends 500 ms apart cover one lost UDP datagram
            // without holding the drain open on an unresponsive
            // peer; a duplicate BYE is harmless (200 then 481).
            let transport = Arc::clone(&self.transport);
            tokio::spawn(async move {
                for attempt in 0..2u8 {
                    if let Err(e) = transport.send(bye.clone(), dest).await {
                        warn!(%call_id, peer = %dest, ?e, "drain BYE send failed");
                        break;
                    }
                    debug!(%call_id, peer = %dest, attempt, "drain BYE sent");
                    if attempt == 0 {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            });
            if let Some(ep) = record.media {
                self.media.release_endpoint(ep).await;
            }
            let _ = self.bus.publish(Event::Sip(SipEvent::DialogTerminated {
                call_id: record.call_id,
            }));
        }
        hung_up
    }

    fn active_dialogs(&self) -> usize {
        self.dialogs.len()
    }
}

/// Build the in-dialog BYE the engine sends to `record`'s peer at
/// drain. The engine never originated a request in a UAS-accepted
/// dialog, so `CSeq: 1` is fresh in its own sequence space; the
/// peer matches on Call-ID + tags regardless of the Request-URI.
pub(crate) fn build_bye(record: &smiths_core::DialogRecord, via: SocketAddr) -> Vec<u8> {
    static BRANCH: AtomicU64 = AtomicU64::new(0);
    let branch = format!(
        "z9hG4bK-drain-{}-{}",
        std::process::id(),
        BRANCH.fetch_add(1, Ordering::Relaxed)
    );
    let peer_uri = format!("sip:{}", record.peer_signal);
    format!(
        "BYE {peer_uri} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {via};branch={branch};rport\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:smiths@{via}>;tag={ftag}\r\n\
         To: <{peer_uri}>;tag={ttag}\r\n\
         Call-ID: {cid}\r\n\
         CSeq: 1 BYE\r\n\
         Content-Length: 0\r\n\r\n",
        ftag = record.local_tag,
        ttag = record.remote_tag,
        cid = record.call_id,
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use smiths_media::UdpMediaFabric;
    use tokio::net::UdpSocket;

    fn record(peer: SocketAddr) -> smiths_core::DialogRecord {
        serde_json::from_value(serde_json::json!({
            "call_id": "drain-1@test",
            "local_tag": "lt",
            "remote_tag": "rt",
            "state": "confirmed",
            "peer_signal": peer.to_string(),
            "rendezvous": null,
            "media": null,
            "remote_media": null,
        }))
        .unwrap()
    }

    #[test]
    fn bye_carries_dialog_identity() {
        let rec = record("192.0.2.1:5060".parse().unwrap());
        let bye = String::from_utf8(build_bye(&rec, "127.0.0.1:5060".parse().unwrap())).unwrap();
        assert!(
            bye.starts_with("BYE sip:192.0.2.1:5060 SIP/2.0\r\n"),
            "{bye}"
        );
        assert!(bye.contains("Call-ID: drain-1@test\r\n"));
        assert!(bye.contains(";tag=lt\r\n"));
        assert!(bye.contains(";tag=rt\r\n"));
        assert!(bye.contains("CSeq: 1 BYE\r\n"));
        assert!(bye.ends_with("Content-Length: 0\r\n\r\n"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hangup_all_sends_bye_and_clears_table() {
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer.local_addr().unwrap();
        let transport = Arc::new(
            UdpTransport::bind("127.0.0.1:0".parse().unwrap())
                .await
                .unwrap(),
        );
        let local = transport.local_addr().unwrap();
        let dialogs: Arc<DialogTable> = Arc::new(DialogTable::new());
        let rec = record(peer_addr);
        dialogs.insert(rec.key(), rec);
        let metrics = Metrics::noop();
        metrics.dialogs_active.inc();
        let media: Arc<dyn MediaFabric> = Arc::new(UdpMediaFabric::new());
        let hangup = UasHangup {
            transport,
            local,
            dialogs: Arc::clone(&dialogs),
            media,
            bus: EventBus::new(8),
            metrics: Arc::clone(&metrics),
        };
        assert_eq!(hangup.active_dialogs(), 1);
        assert_eq!(hangup.hangup_all().await, 1);
        assert_eq!(hangup.active_dialogs(), 0);
        assert_eq!(metrics.dialogs_active.get(), 0);

        let mut buf = vec![0u8; 2048];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut buf))
            .await
            .expect("BYE must arrive")
            .unwrap();
        let bye = String::from_utf8_lossy(&buf[..n]);
        assert!(bye.starts_with("BYE "), "{bye}");
        assert!(bye.contains("Call-ID: drain-1@test"), "{bye}");
    }
}
