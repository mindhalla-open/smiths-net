//! Async [`TransactionDriver`] — the glue between the pure-synchronous
//! FSMs in this module and the network / timer runtime.
//!
//! The driver owns:
//! - a [`DashMap`] of live transactions keyed by [`TransactionKey`];
//! - a [`Transport`] handle for egress `SendToPeer` actions;
//! - a [`ResponseRouter`] handle so inbound responses can be
//!   demultiplexed to the right FSM;
//! - one background tokio task per armed timer (cancel = `AbortHandle`).
//!
//! Every FSM invocation runs under a short `std::sync::Mutex` — state
//! machines are `&mut self`-style data transforms and we never hold
//! the lock across an `.await`. The `execute_actions` step that
//! follows does the actual I/O / timer arming after the lock is
//! released.
//!
//! The caller (today: [`crate::UacClient`]) registers a client FSM
//! via [`TransactionDriver::start_client`] and drains the returned
//! [`mpsc::UnboundedReceiver<TuEvent>`] for responses + a final
//! `Terminated` marker.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use dashmap::DashMap;
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use smiths_core::Metrics;

use crate::response_router::ResponseRouter;
use crate::transport::Transport;

use super::{
    Role, TimerId, Transaction, TransactionAction, TransactionEvent, TransactionKey,
    TransactionState,
};

/// Event delivered from a transaction to its Transaction User.
///
/// Client FSMs report every received response (provisionals and
/// finals) and a final `Terminated` marker once the FSM reaches
/// [`TransactionState::Terminated`]. The caller can stop reading the
/// channel after observing the first final response — the `Terminated`
/// follow-up is informational.
#[derive(Clone, Debug)]
pub enum TuEvent {
    /// Response received from the peer (1xx-6xx).
    Response {
        /// Parsed status code.
        status: u16,
        /// Raw response bytes (same shape as what came off the wire).
        bytes: Bytes,
    },
    /// FSM reached `Terminated`. Driver has released the txn entry
    /// by the time this lands on the channel.
    Terminated,
}

/// Async driver hosting one or more transaction FSMs.
///
/// Cheap to clone — internally `Arc`-backed. A single driver per
/// UAC / UAS instance is the expected deployment.
///
/// Generic over [`Transport`] rather than `dyn Transport` because the
/// trait uses `impl Future` (AFIT) and isn't dyn-compatible; this
/// matches [`crate::UacClient`]'s shape so the UAC's existing
/// transport handle plugs in without an adapter.
pub struct TransactionDriver<T: Transport> {
    inner: Arc<Inner<T>>,
}

impl<T: Transport> Clone for TransactionDriver<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct Inner<T: Transport> {
    transport: Arc<T>,
    router: Arc<ResponseRouter>,
    txns: DashMap<TransactionKey, Arc<TxnEntry>>,
    /// Optional metrics handle. When present, the driver increments
    /// `sip_server_txns_active` on server-FSM registration and
    /// decrements it on termination. Client-side FSMs aren't
    /// counted — they live for the call's own timeout window and
    /// aren't a pressure signal.
    metrics: Option<Arc<Metrics>>,
}

/// One live transaction's runtime state. The FSM itself lives behind
/// a `std::sync::Mutex` because `Transaction::on_event` is
/// synchronous and we never hold the lock across an await.
struct TxnEntry {
    fsm: Mutex<Box<dyn Transaction>>,
    peer: SocketAddr,
    /// Armed timers, keyed by id. We store each timer's
    /// `AbortHandle` so `CancelTimer` can stop it even if it's
    /// already sleeping.
    timers: Mutex<HashMap<TimerId, AbortHandle>>,
    /// Channel to the Transaction User. Unbounded so `execute_actions`
    /// can never deadlock on a slow reader; unbounded is safe because
    /// a well-behaved TU drains the channel promptly and a buggy one
    /// simply buffers up to its first missed poll.
    tu_tx: mpsc::UnboundedSender<TuEvent>,
    /// Handle for the background response-listener task. Aborted on
    /// Terminated so it can stop re-subscribing to the router.
    listener: Mutex<Option<JoinHandle<()>>>,
    /// Per-txn cancellation — flipped on Terminated so any in-flight
    /// timer tasks bow out before calling back into the driver.
    cancel: CancellationToken,
    /// Serializes per-txn wire sends. Each `spawn_send` task acquires
    /// this before calling `transport.send`, so two FSM actions fired
    /// in rapid succession (e.g. 100 Trying followed by 200 OK on the
    /// same INVITE) reach the peer in the order the FSM emitted them.
    /// Without this, both sends were fire-and-forget tokio tasks on
    /// the multi-thread runtime and could race to the socket —
    /// `drain.rs`'s `non_draining_uas_still_accepts_invite` caught
    /// that regression before users would.
    send_serialize: tokio::sync::Mutex<()>,
}

impl<T: Transport> TransactionDriver<T> {
    /// Build a driver bound to a concrete transport + response
    /// router. The router must be the same one the UAS / reader
    /// forwards inbound responses into.
    #[must_use]
    pub fn new(transport: Arc<T>, router: Arc<ResponseRouter>) -> Self {
        Self {
            inner: Arc::new(Inner {
                transport,
                router,
                txns: DashMap::new(),
                metrics: None,
            }),
        }
    }

    /// Builder: attach a metrics handle. With this set, the driver
    /// maintains the `sip_server_txns_active` gauge across
    /// [`Self::start_server`] / terminate. Without it (tests), the
    /// driver runs silently.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        // Cheap: `self.inner` is `Arc`-backed, but we only mutate at
        // construction time so the `Arc::get_mut` is guaranteed.
        Arc::get_mut(&mut self.inner)
            .expect("with_metrics must be called before the driver is shared")
            .metrics = Some(metrics);
        self
    }

    /// Number of currently-live transactions — diagnostic / tests.
    #[must_use]
    pub fn active(&self) -> usize {
        self.inner.txns.len()
    }

    /// Register a client FSM, fire its `StartClient` event, and
    /// return the [`TuEvent`] receiver the caller awaits.
    ///
    /// The driver sets up:
    /// - SSRC-agnostic response routing: a background task
    ///   subscribes to the [`ResponseRouter`] by branch and re-feeds
    ///   any bytes into the FSM via `deliver_response`;
    /// - timer task management via the FSM's `ArmTimer` /
    ///   `CancelTimer` actions.
    pub fn start_client(
        &self,
        txn: Box<dyn Transaction>,
        peer: SocketAddr,
    ) -> mpsc::UnboundedReceiver<TuEvent> {
        let (tu_tx, tu_rx) = mpsc::unbounded_channel();
        let key = txn.key().clone();
        let branch = key.branch.clone();
        let entry = Arc::new(TxnEntry {
            fsm: Mutex::new(txn),
            peer,
            timers: Mutex::new(HashMap::new()),
            tu_tx,
            listener: Mutex::new(None),
            cancel: CancellationToken::new(),
            send_serialize: tokio::sync::Mutex::new(()),
        });
        self.inner.txns.insert(key.clone(), Arc::clone(&entry));

        // Spawn the response-listener *before* StartClient so a
        // peer that replies inside a microsecond (tests!) can't race
        // us and deliver a response that has nowhere to go.
        let listener = self.spawn_response_listener(key.clone(), branch);
        *entry.listener.lock().expect("txn listener mutex") = Some(listener);

        // Fire the opening event on the FSM. This will (typically)
        // return SendToPeer + ArmTimer actions; `execute_actions`
        // drives them out.
        let actions = {
            let mut fsm = entry.fsm.lock().expect("fsm mutex");
            fsm.on_event(TransactionEvent::StartClient)
        };
        self.execute_actions(&key, &entry, actions);
        tu_rx
    }

    /// Feed a parsed response into the transaction identified by
    /// `key`. Public so upstream demultiplexers (e.g. a
    /// richer-than-[`ResponseRouter`] future) can bypass the
    /// built-in listener.
    pub fn deliver_response(&self, key: &TransactionKey, status: u16, bytes: Bytes) {
        let Some(entry) = self.inner.txns.get(key).map(|e| Arc::clone(e.value())) else {
            // Transaction already terminated — late / stale response.
            debug!(?key, status, "response for unknown transaction; dropped");
            return;
        };
        let actions = {
            let mut fsm = entry.fsm.lock().expect("fsm mutex");
            fsm.on_event(TransactionEvent::ResponseReceived { status, bytes })
        };
        self.execute_actions(key, &entry, actions);
    }

    /// Register a server FSM for an inbound request. Unlike
    /// `start_client`, no `StartClient` event is fired — server FSMs
    /// begin in their initial state (Trying for non-INVITE, Proceeding
    /// for INVITE) and wait for the TU to push a response via
    /// [`TransactionDriver::send_response`].
    ///
    /// Returns a [`TuEvent`] receiver; the TU observes `Terminated`
    /// when the FSM's retransmit window closes (timer J / I / H).
    /// Response delivery events (`TuEvent::Response`) are not produced
    /// by server FSMs — the inbound request path is the trigger, and
    /// the TU already has it.
    #[must_use]
    pub fn start_server(
        &self,
        txn: Box<dyn Transaction>,
        peer: SocketAddr,
    ) -> mpsc::UnboundedReceiver<TuEvent> {
        let (tu_tx, tu_rx) = mpsc::unbounded_channel();
        let key = txn.key().clone();
        let entry = Arc::new(TxnEntry {
            fsm: Mutex::new(txn),
            peer,
            timers: Mutex::new(HashMap::new()),
            tu_tx,
            listener: Mutex::new(None),
            cancel: CancellationToken::new(),
            send_serialize: tokio::sync::Mutex::new(()),
        });
        self.inner.txns.insert(key, Arc::clone(&entry));
        if let Some(m) = &self.inner.metrics {
            m.sip_server_txns_active.inc();
        }
        tu_rx
    }

    /// Feed an inbound request (or retransmit) into the server FSM
    /// identified by `key`. The FSM replays its cached response on
    /// retransmits; first-arrival delivery is the caller's job since
    /// the TU already consumed the original request to build its
    /// reply.
    pub fn deliver_request(&self, key: &TransactionKey, method: String, bytes: Bytes) {
        let Some(entry) = self.inner.txns.get(key).map(|e| Arc::clone(e.value())) else {
            debug!(?key, "request for unknown transaction; dropped");
            return;
        };
        let actions = {
            let mut fsm = entry.fsm.lock().expect("fsm mutex");
            fsm.on_event(TransactionEvent::RequestReceived { method, bytes })
        };
        self.execute_actions(key, &entry, actions);
    }

    /// Push a TU-built response through the server FSM. The FSM will
    /// emit `SendToPeer` (wire the response) and, for finals, arm the
    /// retransmit-absorb timer (J for non-INVITE, G/H for INVITE).
    pub fn send_response(&self, key: &TransactionKey, status: u16, bytes: Bytes) {
        let Some(entry) = self.inner.txns.get(key).map(|e| Arc::clone(e.value())) else {
            debug!(
                ?key,
                status, "send_response on unknown transaction; dropped"
            );
            return;
        };
        let actions = {
            let mut fsm = entry.fsm.lock().expect("fsm mutex");
            fsm.on_event(TransactionEvent::SendResponseFromTu { status, bytes })
        };
        self.execute_actions(key, &entry, actions);
    }

    /// Listener task: repeatedly subscribes to the router by branch
    /// and feeds each response into the FSM until the txn entry is
    /// gone. Re-subscribing per message matches how the pre-FSM
    /// `wait_for_final` helper works (router oneshots are single-
    /// shot).
    fn spawn_response_listener(&self, key: TransactionKey, branch: String) -> JoinHandle<()> {
        let driver = self.clone();
        tokio::spawn(async move {
            loop {
                let Some(entry) = driver.inner.txns.get(&key).map(|e| Arc::clone(e.value())) else {
                    break;
                };
                let rx = driver.inner.router.subscribe(&branch);
                // Race the router against the per-txn cancel token so
                // that a Terminated event doesn't leave a dangling
                // subscriber waiting forever.
                tokio::select! {
                    () = entry.cancel.cancelled() => break,
                    result = rx => match result {
                        Ok(bytes) => {
                            let status = parse_status(&bytes).unwrap_or(0);
                            driver.deliver_response(&key, status, bytes);
                        }
                        Err(_) => break,
                    }
                }
                // Bail out early if the txn is gone now.
                if !driver.inner.txns.contains_key(&key) {
                    break;
                }
            }
            debug!(?key, "response listener exiting");
        })
    }

    /// Execute a batch of FSM-produced actions. Runs synchronously
    /// as far as the FSM is concerned — all awaits (transport send,
    /// timer sleep) happen inside spawned tasks.
    fn execute_actions(
        &self,
        key: &TransactionKey,
        entry: &Arc<TxnEntry>,
        actions: Vec<TransactionAction>,
    ) {
        for action in actions {
            match action {
                TransactionAction::SendToPeer(bytes) => {
                    self.spawn_send(entry, bytes);
                }
                TransactionAction::DeliverResponseToTu { status, bytes } => {
                    let _ = entry.tu_tx.send(TuEvent::Response { status, bytes });
                }
                TransactionAction::ArmTimer { id, after } => {
                    self.arm_timer(key, entry, id, after);
                }
                TransactionAction::CancelTimer(id) => {
                    Self::cancel_timer(entry, id);
                }
                TransactionAction::Terminated => {
                    self.terminate(key, entry);
                }
            }
        }
    }

    /// Spawn a per-txn send task. Every send on the same transaction
    /// acquires the entry's `send_serialize` mutex first, so two FSM
    /// actions fired back-to-back (e.g. 100 Trying + 200 OK on the
    /// same INVITE) reach the wire in FSM-emit order. Tokio's
    /// `Mutex` is FIFO so the queued ordering is preserved even on
    /// multi-thread runtimes.
    fn spawn_send(&self, entry: &Arc<TxnEntry>, bytes: Bytes) {
        let transport = Arc::clone(&self.inner.transport);
        let entry = Arc::clone(entry);
        let peer = entry.peer;
        tokio::spawn(async move {
            let _guard = entry.send_serialize.lock().await;
            if let Err(e) = transport.send(bytes, peer).await {
                warn!(%peer, ?e, "transaction driver: transport send failed");
            }
        });
    }

    fn arm_timer(&self, key: &TransactionKey, entry: &Arc<TxnEntry>, id: TimerId, after: Duration) {
        let driver = self.clone();
        let key = key.clone();
        let cancel = entry.cancel.clone();
        let handle = tokio::spawn(async move {
            tokio::select! {
                () = cancel.cancelled() => {}
                () = sleep(after) => {
                    driver.fire_timer(&key, id);
                }
            }
        })
        .abort_handle();
        // If a timer with this id was already armed (Arm replaces
        // Arm, per RFC 3261 §17 "restart" semantics), abort the old.
        let mut timers = entry.timers.lock().expect("timers mutex");
        if let Some(old) = timers.insert(id, handle) {
            old.abort();
        }
    }

    fn cancel_timer(entry: &Arc<TxnEntry>, id: TimerId) {
        let mut timers = entry.timers.lock().expect("timers mutex");
        if let Some(h) = timers.remove(&id) {
            h.abort();
        }
    }

    fn fire_timer(&self, key: &TransactionKey, id: TimerId) {
        let Some(entry) = self.inner.txns.get(key).map(|e| Arc::clone(e.value())) else {
            return;
        };
        // Remove our handle from the map first so a re-arming FSM
        // doesn't double-abort itself mid-execute.
        let _ = entry.timers.lock().expect("timers mutex").remove(&id);
        let actions = {
            let mut fsm = entry.fsm.lock().expect("fsm mutex");
            fsm.on_event(TransactionEvent::TimerFired(id))
        };
        self.execute_actions(key, &entry, actions);
    }

    fn terminate(&self, key: &TransactionKey, entry: &Arc<TxnEntry>) {
        // Flip the cancel token first so any in-flight timer / listener
        // task drops instead of calling back into us.
        entry.cancel.cancel();
        // Abort every still-armed timer explicitly — the cancel token
        // handles the sleep branch, but the map needs cleanup too.
        let timers: Vec<AbortHandle> = entry
            .timers
            .lock()
            .expect("timers mutex")
            .drain()
            .map(|(_, h)| h)
            .collect();
        for h in timers {
            h.abort();
        }
        if let Some(h) = entry.listener.lock().expect("listener mutex").take() {
            h.abort();
        }
        let removed = self.inner.txns.remove(key).is_some();
        // Only decrement for server-side entries (see `with_metrics`
        // rationale) and only when the remove actually landed — a
        // double-terminate from two racing action batches could
        // otherwise send the gauge negative.
        if removed
            && key.role == Role::Server
            && let Some(m) = &self.inner.metrics
        {
            m.sip_server_txns_active.dec();
        }
        let _ = entry.tu_tx.send(TuEvent::Terminated);
    }

    /// `true` when `key` still names a live transaction in the
    /// driver's table. Tests-only helper today.
    #[must_use]
    pub fn is_alive(&self, key: &TransactionKey) -> bool {
        self.inner.txns.contains_key(key)
    }

    /// Snapshot the FSM state for `key`. `None` when the txn has
    /// been released. Diagnostic / tests.
    #[must_use]
    pub fn state(&self, key: &TransactionKey) -> Option<TransactionState> {
        self.inner
            .txns
            .get(key)
            .map(|e| e.value().fsm.lock().expect("fsm mutex").state())
    }
}

/// Parse the numeric status code out of a response's first line.
/// Returns `None` if the bytes aren't a valid SIP response line.
fn parse_status(bytes: &[u8]) -> Option<u16> {
    let first_line_end = bytes.iter().position(|&b| b == b'\r')?;
    let line = std::str::from_utf8(&bytes[..first_line_end]).ok()?;
    let mut parts = line.split_whitespace();
    let _version = parts.next()?;
    let code = parts.next()?;
    code.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UdpTransport;
    use crate::txn::ClientNonInviteTxn;
    use tokio::net::UdpSocket;

    /// Build a live UDP transport bound on loopback for integration
    /// tests. Returns the transport + its local `SocketAddr`.
    async fn bound_udp() -> (Arc<UdpTransport>, SocketAddr) {
        let t = UdpTransport::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let local = t.local_addr().unwrap();
        (Arc::new(t), local)
    }

    /// Bind a UDP socket as a stand-in "peer" the driver sends to.
    /// The peer just `recv_from`s; the test feeds responses into the
    /// router directly so we can control timing.
    async fn bound_peer() -> (UdpSocket, SocketAddr) {
        let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = s.local_addr().unwrap();
        (s, addr)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_client_sends_request_and_delivers_final_response() {
        let (engine_transport, _engine_addr) = bound_udp().await;
        let (peer_sock, peer_addr) = bound_peer().await;

        // Wire a response router so the driver can demux responses.
        let router = Arc::new(ResponseRouter::new());
        let driver = TransactionDriver::new(engine_transport.clone(), Arc::clone(&router));

        // Build a BYE and start the client txn.
        let branch = "z9hG4bK-driver-t1".to_string();
        let request = Bytes::from_static(
            b"BYE sip:alice@127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:0;branch=z9hG4bK-driver-t1\r\nContent-Length: 0\r\n\r\n",
        );
        let txn = ClientNonInviteTxn::new(branch.clone(), "BYE", request);
        let key = txn.key().clone();
        let mut tu_rx = driver.start_client(Box::new(txn), peer_addr);

        // Wait for the BYE to arrive at the fake peer.
        let mut buf = [0u8; 2048];
        let (n, from) =
            tokio::time::timeout(Duration::from_millis(500), peer_sock.recv_from(&mut buf))
                .await
                .expect("peer never saw the BYE")
                .unwrap();
        assert!(n > 0);
        assert!(buf[..n].starts_with(b"BYE"));

        // Fake peer sends a 200 OK back — but via `router.deliver`
        // because our driver listens on the router for responses.
        let response = Bytes::from_static(
            b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 127.0.0.1:0;branch=z9hG4bK-driver-t1\r\nContent-Length: 0\r\n\r\n",
        );
        let delivered = router.deliver(&branch, response.clone());
        assert!(delivered, "router should have had a subscriber waiting");

        // TU must see the 200 + then Terminated once timer K fires
        // (we don't wait for K — just confirm the response).
        let ev = tokio::time::timeout(Duration::from_secs(1), tu_rx.recv())
            .await
            .expect("TU receiver timed out")
            .expect("TU channel closed unexpectedly");
        match ev {
            TuEvent::Response { status, .. } => assert_eq!(status, 200),
            TuEvent::Terminated => panic!("expected Response(200), got Terminated"),
        }

        // Driver still has the entry (Completed, waiting on K).
        assert_eq!(driver.state(&key), Some(TransactionState::Completed));

        // Drop the driver; running timers will cancel cleanly.
        let _ = from;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn timer_e_retransmits_until_response_arrives() {
        let (engine_transport, _engine_addr) = bound_udp().await;
        let (peer_sock, peer_addr) = bound_peer().await;

        let router = Arc::new(ResponseRouter::new());
        let driver = TransactionDriver::new(engine_transport.clone(), Arc::clone(&router));

        let branch = "z9hG4bK-driver-retrans".to_string();
        let request = Bytes::from_static(
            b"BYE sip:alice@127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:0;branch=z9hG4bK-driver-retrans\r\nContent-Length: 0\r\n\r\n",
        );
        let txn = ClientNonInviteTxn::new(branch.clone(), "BYE", request);
        let mut tu_rx = driver.start_client(Box::new(txn), peer_addr);

        // First send arrives ~immediately.
        let mut buf = [0u8; 2048];
        let (_, _) =
            tokio::time::timeout(Duration::from_millis(200), peer_sock.recv_from(&mut buf))
                .await
                .expect("first BYE never arrived")
                .unwrap();

        // Second send should arrive after T1 = 500 ms, allow up to 1 s.
        let (_, _) =
            tokio::time::timeout(Duration::from_millis(900), peer_sock.recv_from(&mut buf))
                .await
                .expect("retransmit never arrived — timer E not wired")
                .unwrap();

        // Now deliver a 200 and verify the TU sees it.
        let response = Bytes::from_static(
            b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 127.0.0.1:0;branch=z9hG4bK-driver-retrans\r\nContent-Length: 0\r\n\r\n",
        );
        router.deliver(&branch, response);
        let ev = tokio::time::timeout(Duration::from_secs(1), tu_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev, TuEvent::Response { status: 200, .. }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_server_registers_and_sends_response_to_peer() {
        use crate::txn::ServerNonInviteTxn;

        let (engine_transport, _engine_addr) = bound_udp().await;
        let (peer_sock, peer_addr) = bound_peer().await;

        let router = Arc::new(ResponseRouter::new());
        let driver = TransactionDriver::new(engine_transport.clone(), Arc::clone(&router));

        let branch = "z9hG4bK-srv-ni-1".to_string();
        let txn = ServerNonInviteTxn::new(branch.clone(), "OPTIONS");
        let key = txn.key().clone();
        let _tu_rx = driver.start_server(Box::new(txn), peer_addr);

        assert!(driver.is_alive(&key));
        assert_eq!(driver.state(&key), Some(TransactionState::Trying));

        // TU pushes a 200 OK; driver forwards to peer + arms timer J.
        let response = Bytes::from_static(b"SIP/2.0 200 OK\r\nContent-Length: 0\r\n\r\n");
        driver.send_response(&key, 200, response.clone());

        let mut buf = [0u8; 2048];
        let (n, _) =
            tokio::time::timeout(Duration::from_millis(500), peer_sock.recv_from(&mut buf))
                .await
                .expect("peer never saw the 200")
                .unwrap();
        assert!(buf[..n].starts_with(b"SIP/2.0 200"));
        assert_eq!(driver.state(&key), Some(TransactionState::Completed));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn server_request_retransmit_replays_cached_final() {
        use crate::txn::ServerNonInviteTxn;

        let (engine_transport, _engine_addr) = bound_udp().await;
        let (peer_sock, peer_addr) = bound_peer().await;

        let router = Arc::new(ResponseRouter::new());
        let driver = TransactionDriver::new(engine_transport.clone(), Arc::clone(&router));

        let branch = "z9hG4bK-srv-ni-retrans".to_string();
        let txn = ServerNonInviteTxn::new(branch.clone(), "OPTIONS");
        let key = txn.key().clone();
        let _tu_rx = driver.start_server(Box::new(txn), peer_addr);

        let response = Bytes::from_static(b"SIP/2.0 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        driver.send_response(&key, 404, response.clone());

        // Drain the first send.
        let mut buf = [0u8; 2048];
        tokio::time::timeout(Duration::from_millis(500), peer_sock.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();

        // Now simulate a request retransmit; the FSM must replay the cached 404.
        driver.deliver_request(&key, "OPTIONS".into(), Bytes::new());
        let (n, _) =
            tokio::time::timeout(Duration::from_millis(500), peer_sock.recv_from(&mut buf))
                .await
                .expect("replay never arrived")
                .unwrap();
        assert!(buf[..n].starts_with(b"SIP/2.0 404"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn metrics_gauge_tracks_server_txn_lifecycle() {
        use crate::txn::ServerNonInviteTxn;

        let (engine_transport, _engine_addr) = bound_udp().await;
        let (_peer_sock, peer_addr) = bound_peer().await;

        // Noop `Metrics` has a real `sip_server_txns_active` gauge —
        // it just isn't registered on the engine's `/metrics`
        // registry. Tests inspect the handle directly.
        let metrics = Metrics::noop();
        let router = Arc::new(ResponseRouter::new());
        let driver = TransactionDriver::new(engine_transport.clone(), Arc::clone(&router))
            .with_metrics(Arc::clone(&metrics));

        assert_eq!(metrics.sip_server_txns_active.get(), 0);

        let branch = "z9hG4bK-metric-1".to_string();
        let txn = ServerNonInviteTxn::new(branch.clone(), "OPTIONS");
        let key = txn.key().clone();
        let _tu_rx = driver.start_server(Box::new(txn), peer_addr);

        assert_eq!(
            metrics.sip_server_txns_active.get(),
            1,
            "start_server must inc the gauge"
        );

        // Drive the FSM to Terminated: send a final, then fire timer J.
        driver.send_response(&key, 200, Bytes::from_static(b"SIP/2.0 200 OK\r\n\r\n"));
        driver.fire_timer(&key, TimerId::J);

        assert!(
            !driver.is_alive(&key),
            "FSM should be terminated after timer J"
        );
        assert_eq!(
            metrics.sip_server_txns_active.get(),
            0,
            "terminate must dec the gauge"
        );
    }

    #[test]
    fn parse_status_extracts_code() {
        assert_eq!(parse_status(b"SIP/2.0 200 OK\r\n"), Some(200));
        assert_eq!(parse_status(b"SIP/2.0 401 Unauthorized\r\n"), Some(401));
        assert_eq!(parse_status(b""), None);
        assert_eq!(parse_status(b"not sip\r\n"), None);
    }
}
