//! RFC 3261 SIP signaling: transport, parser, transactions, dialogs,
//! digest auth.
//!
//! Phase 1 scope: UDP / TCP / TLS transports, an OPTIONS-answering
//! UAS, a UAC for engine-initiated calls, and the `Transport` +
//! `CredentialStore` trait seams (MVP guardrails for later phases).
//! Full RFC 3261 transaction FSMs with timers A–K land in follow-up
//! passes.

pub mod auth;
pub mod error;
pub mod response_router;
pub mod transport;
pub mod uac;
pub mod uas;

pub use error::Error;
pub use response_router::ResponseRouter;
pub use transport::{Datagram, Transport, tcp::TcpTransport, tls::TlsTransport, udp::UdpTransport};
pub use uac::{UacClient, UacError};
pub use uas::UasServer;
