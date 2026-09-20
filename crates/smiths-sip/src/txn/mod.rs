//! RFC 3261 §17 transaction layer — framework types.
//!
//! This module houses the **pure** data types every transaction FSM
//! shares: the state enum, the event vocabulary (what the network /
//! timer wheel feed into an FSM), the action vocabulary (what an FSM
//! tells the driver to do), and the `Transaction` trait.
//!
//! **Pure** means: no async, no I/O, no locks. FSMs are `on_event →
//! actions`, synchronously. [`driver::TransactionDriver`] wraps them
//! with tokio tasks + timers to actually send bytes and fire timers.
//! This split is deliberate — state machines are exhaustively
//! testable without booting a runtime.
//!
//! All four RFC 3261 FSMs live here and are wired into the engine:
//! the UAC drives [`ClientInviteTxn`] / [`ClientNonInviteTxn`] and
//! the UAS drives [`ServerInviteTxn`] / [`ServerNonInviteTxn`], all
//! through the same driver. Each FSM has a reliable-transport mode
//! ([`Transaction::set_reliable`], selected by the driver from the
//! transport's [`crate::transport::TransportKind`]) that skips the
//! retransmission timers and uses zero-length absorb windows.

pub mod ack;
pub mod client_invite;
pub mod client_non_invite;
pub mod dialog;
pub mod driver;
pub mod server_invite;
pub mod server_non_invite;
pub mod timers;

pub use ack::build_non_ok_ack;
pub use client_invite::ClientInviteTxn;
pub use client_non_invite::ClientNonInviteTxn;
pub use dialog::{DialogEvent, DialogFsm, DialogTransitionError};
pub use driver::{TransactionDriver, TuEvent};
pub use server_invite::ServerInviteTxn;
pub use server_non_invite::ServerNonInviteTxn;
pub use timers::{T1, T2, T4, TIMEOUT_64T1, Timer};

/// Canonical transaction state (RFC 3261 §17, collapsed across the
/// four flavors). Not every FSM uses every variant — e.g., non-INVITE
/// FSMs skip `Calling` / `Confirmed`, the server FSMs skip `Trying`
/// at different boundaries — but having one enum keeps the driver
/// generic. Each FSM module documents its own state subset.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TransactionState {
    /// Initial state for client non-INVITE (before first response).
    Trying,
    /// Initial state for client INVITE (before 1xx).
    Calling,
    /// At least one provisional (1xx) response seen / sent.
    Proceeding,
    /// Final response received / sent; retransmit buffer alive.
    Completed,
    /// INVITE server: 2xx received + ACK seen (stays briefly for
    /// ACK retransmits).
    Confirmed,
    /// Transaction done, driver may release the entry.
    Terminated,
}

/// Per-FSM role. Drives which set of timers is legal and which
/// `on_event` branches fire. A single `TransactionKey` with the same
/// branch + method but different roles represents *two* distinct
/// transactions (the client side vs. the server side of the same
/// hop-to-hop exchange).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    /// Originator of the request.
    Client,
    /// Receiver of the request (sends the response).
    Server,
}

/// Lookup key for the driver's transaction table.
///
/// RFC 3261 §17.2.3 matches an inbound request to a server
/// transaction on Via `branch` + `sent-by` + method; §17.1.3 matches
/// a response to a client transaction on `branch` + `CSeq` method. The
/// table is keyed on `(branch, method, role)`; the driver keeps the
/// request's `sent-by` alongside each server entry
/// ([`driver::TransactionDriver::start_server_with_sent_by`]) and
/// refuses to treat a request from a different `sent-by` as a
/// retransmission, and checks the `CSeq` method of every response
/// before feeding it to a client FSM.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TransactionKey {
    /// `Via` branch parameter (RFC 3261 magic cookie + hex).
    pub branch: String,
    /// Method (`INVITE`, `BYE`, `REGISTER`, …). CANCEL is a separate
    /// key even though it shares the branch of the target INVITE.
    pub method: String,
    /// Client / Server side of the hop-to-hop exchange.
    pub role: Role,
}

/// Well-known timer ids. FSMs return these in [`TransactionAction`];
/// the driver owns the actual sleeping/firing. Not every FSM uses
/// every timer — client non-INVITE uses E, F, K.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TimerId {
    /// INVITE client retransmit — doubling (§17.1.1.2).
    A,
    /// INVITE client transaction timeout — `64 * T1` (§17.1.1.2).
    B,
    /// Proxy INVITE transaction timeout — not used by a UA (§16.6).
    C,
    /// INVITE client wait for response retransmits — `32 s` on
    /// unreliable transport (§17.1.1.2).
    D,
    /// Non-INVITE client retransmit — doubling up to T2 (§17.1.2.2).
    E,
    /// Non-INVITE client transaction timeout — `64 * T1` (§17.1.2.2).
    F,
    /// INVITE server 2xx retransmit (§17.2.1).
    G,
    /// INVITE server wait for ACK — `64 * T1` (§17.2.1).
    H,
    /// INVITE server wait for ACK retransmits — `T4` (§17.2.1).
    I,
    /// Non-INVITE server wait for request retransmits — `64 * T1`
    /// (§17.2.2).
    J,
    /// Non-INVITE client wait for response retransmits — `T4`
    /// (§17.1.2.2).
    K,
}

/// Something the outside world tells an FSM.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum TransactionEvent {
    /// The owning Transaction User (dialog / UAC application) asked
    /// us to start the transaction by sending a request. For client
    /// FSMs this is the initial trigger; ignored by server FSMs.
    StartClient,
    /// Response bytes received from the network.
    ResponseReceived {
        /// Numeric status code from the response line.
        status: u16,
        /// Raw response bytes — the driver captured them from the
        /// socket. FSMs don't re-parse; they peek at the status code
        /// and hand the opaque blob upward if the TU needs it.
        bytes: bytes::Bytes,
    },
    /// Request bytes received from the network (server FSMs).
    RequestReceived {
        /// Method token from the request line.
        method: String,
        /// Raw request bytes.
        bytes: bytes::Bytes,
    },
    /// The TU asked the server FSM to emit a response. The FSM
    /// forwards it to the wire (via `SendToPeer`) and updates its
    /// state machine accordingly (e.g. a non-2xx final arms G/H,
    /// a 2xx bypasses to Terminated).
    SendResponseFromTu {
        /// Numeric status code of the response.
        status: u16,
        /// Fully-built response bytes, ready for the wire.
        bytes: bytes::Bytes,
    },
    /// A timer the FSM previously armed has fired.
    TimerFired(TimerId),
}

/// Something the FSM asks the driver to do. The driver is free to
/// batch, reorder within the constraints of the FSM's sequencing, or
/// log — but every action must eventually execute for the FSM to
/// make progress.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum TransactionAction {
    /// Write these bytes to the peer's socket.
    SendToPeer(bytes::Bytes),
    /// Deliver a response to the Transaction User (dialog / UAC app).
    /// The TU handles dialog / call state; the transaction layer just
    /// shepherds correlated retransmissions.
    DeliverResponseToTu {
        /// Final or provisional status code.
        status: u16,
        /// Raw response bytes.
        bytes: bytes::Bytes,
    },
    /// Start a timer. If one with the same id was armed, it is
    /// cancelled first (matches RFC 3261's "restart" semantics).
    ArmTimer {
        /// Which timer.
        id: TimerId,
        /// Wall-clock interval.
        after: std::time::Duration,
    },
    /// Cancel a previously-armed timer. No-op if it wasn't armed or
    /// already fired.
    CancelTimer(TimerId),
    /// FSM reached [`TransactionState::Terminated`]; the driver can
    /// release all resources and drop the entry.
    Terminated,
}

/// The interface every FSM implements. Pure synchronous data
/// transformation: feed it one event, receive zero or more actions.
/// Multiple actions per event are common (e.g. on a 200 OK we need
/// to deliver it upward, cancel retransmit, and arm the K timer).
pub trait Transaction: Send {
    /// Stable identifying key for the driver's lookup table.
    fn key(&self) -> &TransactionKey;

    /// Current state. Mostly for diagnostics + tests; drivers
    /// typically care about the `Terminated` action rather than
    /// polling this.
    fn state(&self) -> TransactionState;

    /// Select the timer profile for the underlying transport
    /// (RFC 3261 §17). `true` — TCP / TLS — disables the
    /// retransmission timers (A, E, G) and collapses the absorb
    /// windows (D, I, J, K) to zero so the FSM terminates as soon as
    /// its final exchange is done. Called by the driver before the
    /// first event; the default keeps the unreliable (UDP) profile.
    fn set_reliable(&mut self, reliable: bool) {
        let _ = reliable;
    }

    /// Drive the FSM with one event. The returned vector is the
    /// ordered set of actions the driver must execute. Empty vec =
    /// event was valid but produced no side-effects (e.g. a stray
    /// 1xx when we're already in Proceeding).
    fn on_event(&mut self, event: TransactionEvent) -> Vec<TransactionAction>;
}

/// `sent-by` (host[:port]) of the topmost `Via` header in a raw SIP
/// message, as the peer wrote it. `None` when the message has no
/// parseable `Via`.
#[must_use]
pub fn top_via_sent_by(bytes: &[u8]) -> Option<String> {
    let value = header_value(bytes, &["via", "v"])?;
    // "SIP/2.0/UDP host:port;branch=..." — sent-by is the token after
    // the protocol, up to the first parameter.
    let after_proto = value.split_whitespace().nth(1)?;
    let sent_by = after_proto.split([';', ',']).next()?.trim();
    (!sent_by.is_empty()).then(|| sent_by.to_owned())
}

/// Method token of the `CSeq` header in a raw SIP message
/// (`CSeq: 1 INVITE` → `INVITE`).
#[must_use]
pub fn cseq_method(bytes: &[u8]) -> Option<String> {
    let value = header_value(bytes, &["cseq"])?;
    value.split_whitespace().nth(1).map(str::to_owned)
}

/// Value of the first header whose name matches one of `names`
/// (case-insensitive, compact forms included), trimmed.
fn header_value<'a>(bytes: &'a [u8], names: &[&str]) -> Option<&'a str> {
    let text = std::str::from_utf8(bytes).ok()?;
    let (headers, _) = text.split_once("\r\n\r\n").unwrap_or((text, ""));
    headers.split("\r\n").skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        let name = name.trim();
        names
            .iter()
            .any(|n| name.eq_ignore_ascii_case(n))
            .then(|| value.trim())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQ: &[u8] = b"INVITE sip:a@b SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-1;rport\r\nCSeq: 7 INVITE\r\nContent-Length: 0\r\n\r\n";

    #[test]
    fn top_via_sent_by_extracts_host_port() {
        assert_eq!(top_via_sent_by(REQ).as_deref(), Some("10.0.0.1:5060"));
        let compact = b"SIP/2.0 200 OK\r\nv: SIP/2.0/TCP [::1]:5061;branch=x\r\n\r\n";
        assert_eq!(top_via_sent_by(compact).as_deref(), Some("[::1]:5061"));
        assert!(top_via_sent_by(b"OPTIONS sip:a SIP/2.0\r\n\r\n").is_none());
    }

    #[test]
    fn cseq_method_extracts_token() {
        assert_eq!(cseq_method(REQ).as_deref(), Some("INVITE"));
        assert!(cseq_method(b"SIP/2.0 200 OK\r\n\r\n").is_none());
    }
}
