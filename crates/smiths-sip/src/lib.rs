//! RFC 3261 SIP signaling: transport, parser, transactions, dialogs,
//! digest auth.
//!
//! Phase 1 scope: UDP / TCP / TLS transports, an OPTIONS-answering
//! UAS, a UAC for engine-initiated calls, and the `Transport` +
//! `CredentialStore` trait seams (MVP guardrails for later phases).
//! Full RFC 3261 transaction FSMs with timers A–K land in follow-up
//! passes.

// Slice 1.7: lint-level `warn` on unwraps. The existing call sites
// live under a forward-work bucket — new code fires a warning that
// reviewers can chase before merge, even while legacy usage is
// still being retired.
#![warn(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod auth;
pub mod error;
pub mod rate_limit;
pub mod response_router;
pub mod transport;
pub mod txn;
pub mod uac;
pub mod uas;

pub use error::Error;
pub use rate_limit::SipRateLimiter;
pub use response_router::ResponseRouter;
pub use transport::{Datagram, Transport, tcp::TcpTransport, tls::TlsTransport, udp::UdpTransport};
pub use txn::{
    ClientNonInviteTxn, Role, TransactionAction, TransactionDriver, TransactionEvent,
    TransactionState, TuEvent,
};
pub use uac::{UacClient, UacError};
pub use uas::UasServer;
