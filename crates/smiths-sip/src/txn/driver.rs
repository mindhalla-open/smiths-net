//! Async [`TransactionDriver`] — the glue between the pure-synchronous
//! FSMs in this module and the network / timer runtime.
//!
//! The driver owns:
//! - a [`DashMap`] of live transactions keyed by [`TransactionKey`];
//! - a [`Transport`] handle for egress `SendToPeer` actions;
//! - a [`ResponseRouter`] handle so inbound responses can be
//!   demultiplexed to the right FSM;
//! - one background tokio task per armed timer (cancel = `AbortHandle`);
//! - the ACK-for-2xx replay table used to answer retransmitted `2xx`
//!   responses after a client INVITE transaction has closed
//!   (RFC 3261 §13.2.2.4).
//!
//! Every FSM invocation runs under a short `std::sync::Mutex` — state
//! machines are `&mut self`-style data transforms and we never hold
//! the lock across an `.await`. The `execute_actions` step that
//! follows does the actual I/O / timer arming after the lock is
//! released.
//!
//! The caller ([`crate::UacClient`] for client FSMs,
//! [`crate::UasServer`] for server FSMs) registers an FSM via
//! [`TransactionDriver::start_client`] / [`TransactionDriver::start_server`]
//! and drains the returned [`mpsc::UnboundedReceiver<TuEvent>`] for
//! responses + a final `Terminated` marker. The driver selects the
//! FSM's timer profile from the transport's
//! [`crate::transport::TransportKind`], so TCP / TLS transactions
//! never retransmit.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use arc_swap::ArcSwapOption;
use bytes::Bytes;
use dashmap::DashMap;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use smiths_core::Metrics;

use crate::response_router::ResponseRouter;
use crate::transport::Transport;

use super::{
    Role, TIMEOUT_64T1, TimerId, Transaction, TransactionAction, TransactionEvent, TransactionKey,
    TransactionState, cseq_method, top_via_sent_by,
};

/// How long after a client INVITE transaction closes on a `2xx` the
/// driver keeps answering retransmitted `2xx` responses with the
/// registered ACK. Matches the peer's own retransmission ceiling
/// (RFC 3261 §13.3.1.4: the UAS gives up after 64·T1).
pub const ACK_2XX_WINDOW: Duration = TIMEOUT_64T1;

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
    metrics: ArcSwapOption<Metrics>,
    /// ACKs registered by the TU for a `2xx` to INVITE, keyed by the
    /// INVITE's Via branch. Replayed by the response listener while
    /// it lingers for [`ACK_2XX_WINDOW`] after the FSM terminates.
    acks_2xx: DashMap<String, AckFor2xx>,
}

/// TU-built end-to-end ACK for a `2xx` plus where it goes.
struct AckFor2xx {
    peer: SocketAddr,
    bytes: Bytes,
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
    /// Per-txn cancellation — flipped on Terminated so any in-flight
    /// timer task bows out before calling back into the driver and
    /// the response listener leaves its live phase.
    cancel: CancellationToken,
    /// FIFO queue for per-txn wire sends. A single dedicated consumer
    /// task (spawned at entry creation) drains this channel and
    /// `.await`s `transport.send` in order, so two FSM actions fired
    /// in rapid succession (e.g. 100 Trying followed by 200 OK on the
    /// same INVITE) reach the peer in FSM-emit order. Spawning one
    /// task per send would rely on spawn order matching lock
    /// acquisition order, which tokio's multi-thread scheduler does
    /// not guarantee.
    send_tx: mpsc::UnboundedSender<Bytes>,
    /// Via `sent-by` of the request that created a server
    /// transaction, when the TU supplied it. A request carrying a
    /// different `sent-by` is a different transaction that happens
    /// to share the branch (RFC 3261 §17.2.3), not a retransmit.
    sent_by: OnceLock<String>,
    /// Set once a client INVITE FSM delivers a `2xx` to the TU; the
    /// response listener then keeps the branch subscribed for
    /// [`ACK_2XX_WINDOW`] to re-ACK retransmitted `2xx`s.
    got_2xx: AtomicBool,
}

/// Lock a per-transaction mutex, recovering from poisoning. The
/// guarded data are an FSM (which absorbs any out-of-sequence event
/// by design) and a timer map, so a guard abandoned by a panicking
/// task leaves nothing the driver cannot continue with.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
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
                metrics: ArcSwapOption::empty(),
                acks_2xx: DashMap::new(),
            }),
        }
    }

    /// Builder: attach a metrics handle. With this set, the driver
    /// maintains the `sip_server_txns_active` gauge across
    /// [`Self::start_server`] / terminate. Without it (tests), the
    /// driver runs silently.
    #[must_use]
    pub fn with_metrics(self, metrics: Arc<Metrics>) -> Self {
        self.inner.metrics.store(Some(metrics));
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
    /// - response routing: the branch is subscribed on the
    ///   [`ResponseRouter`] *before* the request goes out, and a
    ///   background task feeds every response into the FSM (after
    ///   checking the `CSeq` method, RFC 3261 §17.1.3) until the FSM
    ///   terminates;
    /// - timer task management via the FSM's `ArmTimer` /
    ///   `CancelTimer` actions;
    /// - the transport's timer profile ([`Transaction::set_reliable`]).
    pub fn start_client(
        &self,
        mut txn: Box<dyn Transaction>,
        peer: SocketAddr,
    ) -> mpsc::UnboundedReceiver<TuEvent> {
        txn.set_reliable(self.inner.transport.kind().is_reliable());
        let (tu_tx, tu_rx) = mpsc::unbounded_channel();
        let (send_tx, send_rx) = mpsc::unbounded_channel::<Bytes>();
        let key = txn.key().clone();
        let entry = Arc::new(TxnEntry {
            fsm: Mutex::new(txn),
            peer,
            timers: Mutex::new(HashMap::new()),
            tu_tx,
            cancel: CancellationToken::new(),
            send_tx,
            sent_by: OnceLock::new(),
            got_2xx: AtomicBool::new(false),
        });
        self.inner.txns.insert(key.clone(), Arc::clone(&entry));
        spawn_send_loop(Arc::clone(&self.inner.transport), peer, send_rx);

        // Subscribe *before* StartClient so a peer that replies inside
        // a microsecond (tests!) can't race us and deliver a response
        // that has nowhere to go.
        let rx = self.inner.router.subscribe(&key.branch);
        self.spawn_response_listener(key.clone(), Arc::clone(&entry), rx);

        // Fire the opening event on the FSM. This will (typically)
        // return SendToPeer + ArmTimer actions; `execute_actions`
        // drives them out.
        let actions = lock(&entry.fsm).on_event(TransactionEvent::StartClient);
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
        let actions =
            lock(&entry.fsm).on_event(TransactionEvent::ResponseReceived { status, bytes });
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
    ///
    /// Prefer [`Self::start_server_with_sent_by`] when the request's
    /// Via `sent-by` is at hand, so retransmit matching follows
    /// RFC 3261 §17.2.3 in full.
    #[must_use]
    pub fn start_server(
        &self,
        txn: Box<dyn Transaction>,
        peer: SocketAddr,
    ) -> mpsc::UnboundedReceiver<TuEvent> {
        self.start_server_inner(txn, peer, None)
    }

    /// [`Self::start_server`] plus the Via `sent-by` of the request
    /// that opened the transaction. [`Self::deliver_request`] then
    /// refuses to treat a request from a different `sent-by` as a
    /// retransmission of this transaction.
    #[must_use]
    pub fn start_server_with_sent_by(
        &self,
        txn: Box<dyn Transaction>,
        peer: SocketAddr,
        sent_by: impl Into<String>,
    ) -> mpsc::UnboundedReceiver<TuEvent> {
        self.start_server_inner(txn, peer, Some(sent_by.into()))
    }

    fn start_server_inner(
        &self,
        mut txn: Box<dyn Transaction>,
        peer: SocketAddr,
        sent_by: Option<String>,
    ) -> mpsc::UnboundedReceiver<TuEvent> {
        txn.set_reliable(self.inner.transport.kind().is_reliable());
        let (tu_tx, tu_rx) = mpsc::unbounded_channel();
        let (send_tx, send_rx) = mpsc::unbounded_channel::<Bytes>();
        let key = txn.key().clone();
        let sent_by_cell = OnceLock::new();
        if let Some(s) = sent_by {
            let _ = sent_by_cell.set(s);
        }
        let entry = Arc::new(TxnEntry {
            fsm: Mutex::new(txn),
            peer,
            timers: Mutex::new(HashMap::new()),
            tu_tx,
            cancel: CancellationToken::new(),
            send_tx,
            sent_by: sent_by_cell,
            got_2xx: AtomicBool::new(false),
        });
        self.inner.txns.insert(key, Arc::clone(&entry));
        spawn_send_loop(Arc::clone(&self.inner.transport), peer, send_rx);
        if let Some(m) = self.inner.metrics.load().as_ref() {
            m.sip_server_txns_active.inc();
        }
        tu_rx
    }

    /// Feed an inbound request (or retransmit) into the server FSM
    /// identified by `key`. The FSM replays its cached response on
    /// retransmits; first-arrival delivery is the caller's job since
    /// the TU already consumed the original request to build its
    /// reply.
    ///
    /// When the transaction was opened with
    /// [`Self::start_server_with_sent_by`] and `bytes` carries a Via
    /// with a different `sent-by`, the request is not a retransmit
    /// of this transaction (RFC 3261 §17.2.3) and is dropped with a
    /// debug log instead of triggering a replay.
    pub fn deliver_request(&self, key: &TransactionKey, method: String, bytes: Bytes) {
        let Some(entry) = self.inner.txns.get(key).map(|e| Arc::clone(e.value())) else {
            debug!(?key, "request for unknown transaction; dropped");
            return;
        };
        if let Some(expected) = entry.sent_by.get()
            && let Some(actual) = top_via_sent_by(&bytes)
            && actual != *expected
        {
            debug!(
                ?key,
                expected, actual, "request sent-by does not match transaction; not a retransmit"
            );
            return;
        }
        let actions =
            lock(&entry.fsm).on_event(TransactionEvent::RequestReceived { method, bytes });
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
        let actions =
            lock(&entry.fsm).on_event(TransactionEvent::SendResponseFromTu { status, bytes });
        self.execute_actions(key, &entry, actions);
    }

    /// Register the end-to-end ACK the TU sent for a `2xx` to the
    /// INVITE on `branch`. For [`ACK_2XX_WINDOW`] after the client
    /// INVITE FSM closed, every retransmitted `2xx` arriving on that
    /// branch is answered by re-sending `ack` to `peer`
    /// (RFC 3261 §13.2.2.4) — the peer only stops retransmitting once
    /// an ACK gets through.
    pub fn register_ack_for_2xx(&self, branch: &str, peer: SocketAddr, ack: Bytes) {
        self.inner
            .acks_2xx
            .insert(branch.to_owned(), AckFor2xx { peer, bytes: ack });
    }

    /// `true` while the driver still answers retransmitted `2xx`s
    /// for `branch` with a registered ACK. Diagnostics / tests.
    #[must_use]
    pub fn has_ack_for_2xx(&self, branch: &str) -> bool {
        self.inner.acks_2xx.contains_key(branch)
    }

    /// Listener task for a client transaction: feeds every response
    /// on the branch into the FSM while the transaction is live, then
    /// — for an INVITE that closed on a `2xx` — lingers for
    /// [`ACK_2XX_WINDOW`] re-sending the registered ACK on each
    /// retransmitted `2xx`. Cancels the router subscription on exit.
    fn spawn_response_listener(
        &self,
        key: TransactionKey,
        entry: Arc<TxnEntry>,
        mut rx: mpsc::UnboundedReceiver<Bytes>,
    ) {
        let driver = self.clone();
        tokio::spawn(async move {
            let mut subscribed = true;
            loop {
                tokio::select! {
                                   () = entry.cancel.cancelled() => break,
                                   msg = rx.recv() => {
                                       let Some(bytes) = msg else {
                                           subscribed = false;
                                           break;
                                       };
                // §17.1.3: a response matches only if its
                // CSeq method matches the request's.
                                       if let Some(m) = cseq_method(&bytes)
                                           && m != key.method
                                       {
                                           debug!(?key, cseq_method = %m, "response CSeq method mismatch; dropped");
                                           continue;
                                       }
                                       let status = parse_status(&bytes).unwrap_or(0);
                                       driver.deliver_response(&key, status, bytes);
                                   }
                               }
            }

            if subscribed && key.method == "INVITE" && entry.got_2xx.load(Ordering::Acquire) {
                driver.absorb_2xx_retransmits(&key.branch, &mut rx).await;
            }
            driver.inner.acks_2xx.remove(&key.branch);
            if subscribed {
                driver.inner.router.cancel(&key.branch);
            }
            debug!(?key, "response listener exiting");
        });
    }

    /// Second phase of the client INVITE listener: answer each
    /// retransmitted `2xx` with the TU's registered ACK until the
    /// window closes or the subscription ends.
    async fn absorb_2xx_retransmits(&self, branch: &str, rx: &mut mpsc::UnboundedReceiver<Bytes>) {
        let window = sleep(ACK_2XX_WINDOW);
        tokio::pin!(window);
        loop {
            tokio::select! {
                () = &mut window => break,
                msg = rx.recv() => {
                    let Some(bytes) = msg else { break };
                    let status = parse_status(&bytes).unwrap_or(0);
                    if !(200..300).contains(&status) {
                        continue;
                    }
                    let Some((peer, ack)) = self
                        .inner
                        .acks_2xx
                        .get(branch)
                        .map(|a| (a.peer, a.bytes.clone()))
                    else {
                        debug!(branch, "2xx retransmit before ACK registered; dropped");
                        continue;
                    };
                    debug!(branch, %peer, "2xx retransmit; re-sending ACK");
                    if let Err(e) = self.inner.transport.send(ack, peer).await {
                        warn!(branch, %peer, ?e, "re-ACK of retransmitted 2xx failed");
                    }
                }
            }
        }
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
                    Self::spawn_send(entry, bytes);
                }
                TransactionAction::DeliverResponseToTu { status, bytes } => {
                    if key.role == Role::Client
                        && key.method == "INVITE"
                        && (200..300).contains(&status)
                    {
                        entry.got_2xx.store(true, Ordering::Release);
                    }
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

    /// Enqueue a wire send on the txn's FIFO queue. A dedicated
    /// consumer task (see [`spawn_send_loop`]) drains the queue and
    /// awaits each `transport.send` in order, guaranteeing that two
    /// FSM `SendToPeer` actions emitted back-to-back (e.g. 100 Trying
    /// + 200 OK on the same INVITE) reach the peer in FSM-emit order.
    fn spawn_send(entry: &Arc<TxnEntry>, bytes: Bytes) {
        if entry.send_tx.send(bytes).is_err() {
            debug!(peer = %entry.peer, "txn send queue closed; bytes dropped");
        }
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
        if let Some(old) = lock(&entry.timers).insert(id, handle) {
            old.abort();
        }
    }

    fn cancel_timer(entry: &Arc<TxnEntry>, id: TimerId) {
        if let Some(h) = lock(&entry.timers).remove(&id) {
            h.abort();
        }
    }

    fn fire_timer(&self, key: &TransactionKey, id: TimerId) {
        let Some(entry) = self.inner.txns.get(key).map(|e| Arc::clone(e.value())) else {
            return;
        };
        // Remove our handle from the map first so a re-arming FSM
        // doesn't double-abort itself mid-execute.
        let _ = lock(&entry.timers).remove(&id);
        let actions = lock(&entry.fsm).on_event(TransactionEvent::TimerFired(id));
        self.execute_actions(key, &entry, actions);
    }

    fn terminate(&self, key: &TransactionKey, entry: &Arc<TxnEntry>) {
        // Flip the cancel token first so any in-flight timer task
        // drops instead of calling back into us, and the response
        // listener leaves its live phase.
        entry.cancel.cancel();
        // Abort every still-armed timer explicitly — the cancel token
        // handles the sleep branch, but the map needs cleanup too.
        let timers: Vec<AbortHandle> = lock(&entry.timers).drain().map(|(_, h)| h).collect();
        for h in timers {
            h.abort();
        }
        let removed = self.inner.txns.remove(key).is_some();
        // Only decrement for server-side entries (see `with_metrics`
        // rationale) and only when the remove actually landed — a
        // double-terminate from two racing action batches could
        // otherwise send the gauge negative.
        if removed
            && key.role == Role::Server
            && let Some(m) = self.inner.metrics.load().as_ref()
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
            .map(|e| lock(&e.value().fsm).state())
    }
}

/// Run the per-txn wire-send loop. Drains `rx` in FIFO order and
/// awaits each `transport.send` sequentially, so FSM-ordered
/// `SendToPeer` actions hit the socket in the same order. The loop
/// exits when the channel closes (the last `TxnEntry` clone dropped,
/// which happens after `terminate` removes the `DashMap` entry and
/// the caller frames unwind). Deliberately does **not** race a
/// cancel token: a `Terminated` FSM action can fire immediately
/// after the final `SendToPeer`, so short-circuiting on cancel here
/// would drop the just-enqueued bytes and the dialog 2xx retransmit
/// loop would then be the first thing the peer sees.
fn spawn_send_loop<T: Transport>(
    transport: Arc<T>,
    peer: SocketAddr,
    mut rx: mpsc::UnboundedReceiver<Bytes>,
) {
    tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if let Err(e) = transport.send(bytes, peer).await {
                warn!(%peer, ?e, "transaction driver: transport send failed");
            }
        }
    });
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
    use crate::transport::TransportKind;
    use crate::txn::{ClientInviteTxn, ClientNonInviteTxn, ServerNonInviteTxn};
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

    async fn recv_prefixed(sock: &UdpSocket, prefix: &[u8], within: Duration) -> Option<Vec<u8>> {
        let mut buf = [0u8; 4096];
        let (n, _) = tokio::time::timeout(within, sock.recv_from(&mut buf))
            .await
            .ok()?
            .ok()?;
        assert!(
            buf[..n].starts_with(prefix),
            "expected {:?}, got {:?}",
            String::from_utf8_lossy(prefix),
            String::from_utf8_lossy(&buf[..n])
        );
        Some(buf[..n].to_vec())
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
        assert!(
            recv_prefixed(&peer_sock, b"BYE", Duration::from_millis(500))
                .await
                .is_some(),
            "peer never saw the BYE"
        );

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
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn back_to_back_provisional_and_final_both_reach_tu() {
        let (engine_transport, _engine_addr) = bound_udp().await;
        let (_peer_sock, peer_addr) = bound_peer().await;
        let router = Arc::new(ResponseRouter::new());
        let driver = TransactionDriver::new(engine_transport.clone(), Arc::clone(&router));

        let branch = "z9hG4bK-driver-b2b".to_string();
        let invite = Bytes::from_static(
            b"INVITE sip:alice@127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:0;branch=z9hG4bK-driver-b2b\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n",
        );
        let txn = ClientInviteTxn::new(branch.clone(), invite);
        let key = txn.key().clone();
        let mut tu_rx = driver.start_client(Box::new(txn), peer_addr);

        // 100 immediately followed by 200 — no yield in between.
        let trying = Bytes::from_static(
            b"SIP/2.0 100 Trying\r\nVia: SIP/2.0/UDP 127.0.0.1:0;branch=z9hG4bK-driver-b2b\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n",
        );
        let ok = Bytes::from_static(
            b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 127.0.0.1:0;branch=z9hG4bK-driver-b2b\r\nTo: <sip:alice@127.0.0.1>;tag=x\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n",
        );
        assert!(router.deliver(&branch, trying));
        assert!(router.deliver(&branch, ok));

        let mut seen = Vec::new();
        while seen.len() < 2 {
            match tokio::time::timeout(Duration::from_secs(1), tu_rx.recv())
                .await
                .expect("TU receiver timed out")
                .expect("TU channel closed")
            {
                TuEvent::Response { status, .. } => seen.push(status),
                TuEvent::Terminated => break,
            }
        }
        assert_eq!(seen, vec![100, 200], "both responses must reach the TU");
        // The FSM drops out of the driver's table from its own task,
        // which runs after the TU event is queued, so poll rather
        // than assume the removal already happened.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while driver.is_alive(&key) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            !driver.is_alive(&key),
            "2xx terminates the client INVITE FSM"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retransmitted_2xx_is_re_acked_after_transaction_closes() {
        let (engine_transport, _engine_addr) = bound_udp().await;
        let (peer_sock, peer_addr) = bound_peer().await;
        let router = Arc::new(ResponseRouter::new());
        let driver = TransactionDriver::new(engine_transport.clone(), Arc::clone(&router));

        let branch = "z9hG4bK-driver-reack".to_string();
        let invite = Bytes::from_static(
            b"INVITE sip:alice@127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:0;branch=z9hG4bK-driver-reack\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n",
        );
        let txn = ClientInviteTxn::new(branch.clone(), invite);
        let key = txn.key().clone();
        let mut tu_rx = driver.start_client(Box::new(txn), peer_addr);
        assert!(
            recv_prefixed(&peer_sock, b"INVITE", Duration::from_millis(500))
                .await
                .is_some()
        );

        let ok = Bytes::from_static(
            b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 127.0.0.1:0;branch=z9hG4bK-driver-reack\r\nTo: <sip:alice@127.0.0.1>;tag=x\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n",
        );
        assert!(router.deliver(&branch, ok.clone()));
        let ev = tokio::time::timeout(Duration::from_secs(1), tu_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev, TuEvent::Response { status: 200, .. }));
        // The TU acks end-to-end and registers the ACK for replay.
        let ack = Bytes::from_static(b"ACK sip:alice@127.0.0.1 SIP/2.0\r\nCSeq: 1 ACK\r\n\r\n");
        driver.register_ack_for_2xx(&branch, peer_addr, ack.clone());
        assert!(!driver.is_alive(&key));
        assert!(
            router.len() == 1,
            "branch stays subscribed for the re-ACK window"
        );

        // Peer never saw the ACK and retransmits the 200: the driver
        // must answer with the same ACK, twice over.
        for _ in 0..2 {
            assert!(router.deliver(&branch, ok.clone()));
            let got = recv_prefixed(&peer_sock, b"ACK", Duration::from_secs(1))
                .await
                .expect("retransmitted 2xx must be re-ACKed");
            assert_eq!(got, ack.to_vec());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn response_with_foreign_cseq_method_is_not_fed_to_fsm() {
        let (engine_transport, _engine_addr) = bound_udp().await;
        let (_peer_sock, peer_addr) = bound_peer().await;
        let router = Arc::new(ResponseRouter::new());
        let driver = TransactionDriver::new(engine_transport.clone(), Arc::clone(&router));

        let branch = "z9hG4bK-driver-cseq".to_string();
        let invite = Bytes::from_static(
            b"INVITE sip:alice@127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:0;branch=z9hG4bK-driver-cseq\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n",
        );
        let txn = ClientInviteTxn::new(branch.clone(), invite);
        let key = txn.key().clone();
        let mut tu_rx = driver.start_client(Box::new(txn), peer_addr);

        // A 200 to a CANCEL sharing the branch must not terminate the INVITE.
        let cancel_ok = Bytes::from_static(
            b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 127.0.0.1:0;branch=z9hG4bK-driver-cseq\r\nCSeq: 1 CANCEL\r\nContent-Length: 0\r\n\r\n",
        );
        assert!(router.deliver(&branch, cancel_ok));
        let ringing = Bytes::from_static(
            b"SIP/2.0 180 Ringing\r\nVia: SIP/2.0/UDP 127.0.0.1:0;branch=z9hG4bK-driver-cseq\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n",
        );
        assert!(router.deliver(&branch, ringing));
        let ev = tokio::time::timeout(Duration::from_secs(1), tu_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(ev, TuEvent::Response { status: 180, .. }),
            "first TU event must be the INVITE's own 180, got {ev:?}"
        );
        assert_eq!(driver.state(&key), Some(TransactionState::Proceeding));
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
    async fn request_from_different_sent_by_is_not_a_retransmit() {
        let (engine_transport, _engine_addr) = bound_udp().await;
        let (peer_sock, peer_addr) = bound_peer().await;
        let router = Arc::new(ResponseRouter::new());
        let driver = TransactionDriver::new(engine_transport.clone(), Arc::clone(&router));

        let branch = "z9hG4bK-srv-sentby".to_string();
        let txn = ServerNonInviteTxn::new(branch.clone(), "OPTIONS");
        let key = txn.key().clone();
        let _tu_rx = driver.start_server_with_sent_by(Box::new(txn), peer_addr, "10.0.0.1:5060");
        driver.send_response(
            &key,
            200,
            Bytes::from_static(b"SIP/2.0 200 OK\r\nContent-Length: 0\r\n\r\n"),
        );
        assert!(
            recv_prefixed(&peer_sock, b"SIP/2.0 200", Duration::from_millis(500))
                .await
                .is_some()
        );

        // Same branch, different sent-by: a distinct transaction per
        // §17.2.3 — nothing must be replayed.
        let foreign = Bytes::from_static(
            b"OPTIONS sip:x SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.2:5060;branch=z9hG4bK-srv-sentby\r\n\r\n",
        );
        driver.deliver_request(&key, "OPTIONS".into(), foreign);
        let mut buf = [0u8; 2048];
        assert!(
            tokio::time::timeout(Duration::from_millis(300), peer_sock.recv_from(&mut buf))
                .await
                .is_err(),
            "foreign sent-by must not trigger a replay"
        );

        // Matching sent-by: a genuine retransmit, replayed.
        let same = Bytes::from_static(
            b"OPTIONS sip:x SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-srv-sentby\r\n\r\n",
        );
        driver.deliver_request(&key, "OPTIONS".into(), same);
        assert!(
            recv_prefixed(&peer_sock, b"SIP/2.0 200", Duration::from_millis(500))
                .await
                .is_some(),
            "matching sent-by must replay the cached final"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn metrics_gauge_tracks_server_txn_lifecycle() {
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

    /// In-memory transport that records every send and reports a
    /// configurable [`TransportKind`], so timer behaviour can be
    /// observed under paused time without sockets.
    struct MockTransport {
        kind: TransportKind,
        sent: Mutex<Vec<Bytes>>,
    }

    impl MockTransport {
        fn new(kind: TransportKind) -> Arc<Self> {
            Arc::new(Self {
                kind,
                sent: Mutex::new(Vec::new()),
            })
        }
        fn sent(&self) -> usize {
            self.sent.lock().unwrap().len()
        }
    }

    impl Transport for MockTransport {
        async fn send(&self, bytes: Bytes, _peer: SocketAddr) -> std::io::Result<()> {
            self.sent.lock().unwrap().push(bytes);
            Ok(())
        }
        fn local_addr(&self) -> std::io::Result<SocketAddr> {
            Ok("127.0.0.1:5060".parse().unwrap())
        }
        fn kind(&self) -> TransportKind {
            self.kind
        }
    }

    /// Drive a client non-INVITE transaction on `kind` under paused
    /// time and report (sends after 3 s, FSM state right after the
    /// final response, next TU event).
    async fn run_non_invite_on(kind: TransportKind) -> (usize, Option<TransactionState>, TuEvent) {
        let transport = MockTransport::new(kind);
        let router = Arc::new(ResponseRouter::new());
        let driver = TransactionDriver::new(Arc::clone(&transport), Arc::clone(&router));
        let peer: SocketAddr = "127.0.0.1:5061".parse().unwrap();
        let branch = format!("z9hG4bK-reliable-{kind:?}");
        let request = Bytes::from(format!(
            "OPTIONS sip:peer SIP/2.0\r\nVia: SIP/2.0/{} 127.0.0.1:5060;branch={branch}\r\nCSeq: 1 OPTIONS\r\nContent-Length: 0\r\n\r\n",
            kind.via_token()
        ));
        let txn = ClientNonInviteTxn::new(branch.clone(), "OPTIONS", request);
        let key = txn.key().clone();
        let mut tu_rx = driver.start_client(Box::new(txn), peer);

        // T1 = 0.5 s, 2·T1 = 1 s: on UDP the request goes out at
        // 0 s, 0.5 s and 1.5 s → three sends by 3 s.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let sends = transport.sent();

        let response = Bytes::from(format!(
            "SIP/2.0 200 OK\r\nVia: SIP/2.0/{} 127.0.0.1:5060;branch={branch}\r\nCSeq: 1 OPTIONS\r\nContent-Length: 0\r\n\r\n",
            kind.via_token()
        ));
        assert!(router.deliver(&branch, response));
        let first = tu_rx.recv().await.unwrap();
        assert!(matches!(first, TuEvent::Response { status: 200, .. }));
        let state = driver.state(&key);
        let next = tokio::time::timeout(Duration::from_secs(10), tu_rx.recv())
            .await
            .expect("expected a Terminated marker")
            .unwrap();
        (sends, state, next)
    }

    #[tokio::test(start_paused = true)]
    async fn reliable_transport_skips_retransmits_and_zero_length_timer_k() {
        let (sends, state, next) = run_non_invite_on(TransportKind::Tcp).await;
        assert_eq!(sends, 1, "no timer-E retransmits on TCP");
        assert_eq!(
            state, None,
            "timer K is zero: the entry is released with the final"
        );
        assert!(matches!(next, TuEvent::Terminated));
    }

    #[tokio::test(start_paused = true)]
    async fn unreliable_transport_retransmits_and_waits_for_timer_k() {
        let (sends, state, next) = run_non_invite_on(TransportKind::Udp).await;
        assert_eq!(sends, 3, "timer E must retransmit at 0.5 s and 1.5 s");
        assert_eq!(state, Some(TransactionState::Completed));
        assert!(
            matches!(next, TuEvent::Terminated),
            "timer K (5 s) closes it"
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
