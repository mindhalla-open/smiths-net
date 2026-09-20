//! RFC 3261 SIP signaling: transport, parser, transactions, dialogs,
//! digest auth.
//!
//! - [`transport`]: UDP / TCP / TLS transports behind the [`Transport`]
//!   seam.
//! - [`txn`]: the four RFC 3261 §17 transaction FSMs (timers A–K)
//!   plus the async [`TransactionDriver`] that runs them, and the
//!   §12 [`txn::DialogFsm`].
//! - [`uas`]: the User-Agent Server — INVITE / re-INVITE / UPDATE /
//!   CANCEL / BYE / OPTIONS / REGISTER, rendezvous bridging, RFC 4028
//!   session timers — with every request routed through the server
//!   FSMs.
//! - [`uac`]: the User-Agent Client for engine-initiated calls,
//!   routed through the client FSMs.
//! - [`auth`]: digest authentication behind the `CredentialStore` /
//!   `RegistrationStore` seams.

// : lint-level `warn` on unwraps. The existing call sites
// live under a forward-work bucket — new code fires a warning that
// reviewers can chase before merge, even while legacy usage is
// still being retired.
#![warn(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod auth;
pub mod error;
pub mod rate_limit;
pub mod response_router;
pub mod snapshot;
pub mod transport;
pub mod txn;
pub mod uac;
pub mod uas;
#[cfg(feature = "webtransport")]
pub mod webrtc;
#[cfg(feature = "webtransport")]
pub mod webtransport;

pub use error::Error;
pub use rate_limit::SipRateLimiter;
pub use response_router::ResponseRouter;
pub use snapshot::{SnapshotError, read_snapshot, write_snapshot};
pub use transport::{Datagram, Transport, tcp::TcpTransport, tls::TlsTransport, udp::UdpTransport};
pub use txn::{
    ClientNonInviteTxn, Role, TransactionAction, TransactionDriver, TransactionEvent,
    TransactionState, TuEvent,
};
pub use uac::{UacClient, UacError};
pub use uas::{
    ConferenceOrchestrator, FaxOrchestrator, SessionTimerConfig, TranscodeOrchestrator, UasServer,
};
#[cfg(feature = "webtransport")]
pub use webrtc::{
    WebRtcHandlerError, WebRtcListenError, WebRtcSession, WebRtcSessionHandler,
    WebRtcSignalingListener, WebSocketSignalingListener,
};
#[cfg(feature = "webtransport")]
pub use webtransport::{
    NullWebTransportListener, WebTransportListener, WebTransportSessionId, WtSignal, WtSignalKind,
};
