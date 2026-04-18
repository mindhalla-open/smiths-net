//! RFC 3261 SIP signaling: transport, parser, transactions, dialogs,
//! digest auth.
//!
//! Phase 1 scope: UDP transport, an OPTIONS-answering UAS, and the
//! `Transport` + `CredentialStore` trait seams (MVP guardrails for
//! later phases). TCP, TLS, full transaction FSMs, dialog FSM, digest
//! computation, and REGISTER land in follow-up passes.

pub mod auth;
pub mod error;
pub mod transport;
pub mod uas;

pub use error::Error;
pub use transport::{Datagram, Transport, udp::UdpTransport};
pub use uas::UasServer;
