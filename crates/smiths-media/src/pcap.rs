//! Per-call packet-capture tap.
//!
//! Gated behind the `pcap` Cargo feature. When enabled, the bridge
//! hands each RTP + RTCP datagram it forwards through [`PcapWriter`]
//! so the operator can pull a per-call `.pcap` file for offline
//! triage (Wireshark, tshark).
//!
//! **Placeholder.** Slice 1.9 ships the feature knob + module skeleton
//! so the config surface is stable; a follow-on commit wires the
//! actual pcap-file encoder (likely `pcap-file = "2"` under this
//! feature) and the bridge-side callsite. Until then the writer is
//! a no-op — enabling the feature flag produces no files, only an
//! info-level log so operators can confirm the config lands.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

/// One tap per call. Holds the filesystem path the bridge writes
/// packets to and an optional backing writer (slice 1.9+).
pub struct PcapWriter {
    path: PathBuf,
    // `#[cfg(feature = "pcap")]` pcap_file::pcap::PcapWriter — a
    // follow-on wires this. Absent here so the tap compiles on all
    // targets; today the feature flag merely toggles the config knob.
}

impl PcapWriter {
    /// Open a new capture at `<dir>/<call_id>.pcap`. Returns `None`
    /// if the dir doesn't exist yet — operators should create it;
    /// we don't `mkdir -p` implicitly to avoid filesystem surprises.
    pub fn open(dir: &Path, call_id: &str) -> Option<Self> {
        if !dir.is_dir() {
            warn!(
                dir = %dir.display(),
                "pcap_dir does not exist; skipping per-call capture"
            );
            return None;
        }
        let safe_name = sanitize_call_id(call_id);
        let path = dir.join(format!("{safe_name}.pcap"));
        info!(%safe_name, path = %path.display(), "pcap tap opened");
        Some(Self { path })
    }

    /// Where this writer will flush when the real encoder lands.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Log one RTP packet. No-op placeholder until the encoder lands.
    pub fn write_rtp(&mut self, _peer: SocketAddr, _bytes: &[u8]) {
        // Intentional no-op. See module docs.
    }

    /// Log one RTCP packet. No-op placeholder.
    pub fn write_rtcp(&mut self, _peer: SocketAddr, _bytes: &[u8]) {
        // Intentional no-op. See module docs.
    }
}

/// Strip path separators and other hostile chars from a Call-ID
/// before using it as a filename. SIP Call-IDs are RFC 3261 §25.1
/// `callid = word [ "@" word ]` so `@` + `/` + `..` are all in the
/// legal set and need scrubbing.
fn sanitize_call_id(call_id: &str) -> String {
    call_id
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_unsafe_chars() {
        assert_eq!(sanitize_call_id("abc@example.com"), "abc_example.com");
        assert_eq!(sanitize_call_id("../etc/passwd"), ".._etc_passwd");
        assert_eq!(sanitize_call_id("plain-123"), "plain-123");
    }

    #[test]
    fn open_returns_none_on_missing_dir() {
        let result = PcapWriter::open(Path::new("/nonexistent/dir/ever"), "call-x");
        assert!(result.is_none());
    }
}
