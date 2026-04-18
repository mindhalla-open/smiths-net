//! Media plane for smiths-net.
//!
//! Today: a byte-transparent UDP bridge that forwards packets between
//! two legs of a call. Jitter buffer, SSRC rewriting, and per-frame
//! plugin hooks are follow-up work — the current forwarder treats the
//! UDP payload as opaque bytes and never parses RTP.

pub mod bridge;

pub use bridge::{Bridge, Leg};
