//! SDP data model.
//!
//! The structs mirror the on-the-wire SDP grammar closely; the parser
//! in `crate::parse` fills them in, and the `Display` implementations
//! serialize them back.

use std::fmt::{self, Write as _};
use std::net::IpAddr;

use crate::srtp_attr::SdesCrypto;

/// A full SDP session description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionDescription {
    /// `o=` line — originator identity and version.
    pub origin: Origin,
    /// `s=` line — session name (required, often `-`).
    pub session_name: String,
    /// Session-level `c=` line. May be omitted if every media has its
    /// own `c=`.
    pub connection: Option<ConnectionInfo>,
    /// Media-level blocks. At least one is required in practice.
    pub media: Vec<MediaDescription>,
}

/// `o=` line: username, session-id, version, network / address type
/// derived from `address`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    /// Originator login (often `-`).
    pub username: String,
    /// Session identifier.
    pub session_id: u64,
    /// Monotonic session version.
    pub session_version: u64,
    /// Unicast address of the session originator.
    pub address: IpAddr,
}

/// `c=` line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionInfo {
    /// IPv4 or IPv6 address.
    pub address: IpAddr,
}

/// A media-level block (`m=` + attributes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaDescription {
    /// Media kind — audio, video, application, ...
    pub kind: MediaKind,
    /// RTP port. `0` in an offer/answer signals a disabled stream.
    pub port: u16,
    /// Protocol string (e.g. `RTP/AVP`, `RTP/SAVP`, `UDP/TLS/RTP/SAVP`).
    pub protocol: String,
    /// Ordered list of RTP payload types (format numbers from `m=`).
    pub formats: Vec<u8>,
    /// Parsed `a=rtpmap` attributes keyed by payload type.
    pub rtpmap: Vec<RtpMap>,
    /// Parsed `a=crypto:` lines for SDES. Empty for plain `RTP/AVP`
    /// streams; one or more entries when the offerer proposed
    /// `RTP/SAVP` with SDES keying.
    pub crypto: Vec<SdesCrypto>,
    /// Media-level direction (`sendrecv` default if omitted).
    pub direction: Direction,
    /// Media-level `c=` overriding the session-level one.
    pub connection: Option<ConnectionInfo>,
    /// Parsed `a=fingerprint:` line (RFC 8122). Present on DTLS-SRTP
    /// and WebRTC offers; absent on plain `RTP/AVP` or SDES `RTP/SAVP`.
    pub fingerprint: Option<Fingerprint>,
    /// Parsed `a=setup:` line (RFC 5763 §5). Tells the peer which side
    /// acts as DTLS client/server. Required for DTLS-SRTP interop.
    pub setup: Option<DtlsSetup>,
    /// ICE username fragment (`a=ice-ufrag:`, RFC 8839 §5.4).
    pub ice_ufrag: Option<String>,
    /// ICE password (`a=ice-pwd:`, RFC 8839 §5.4). Redacted from Debug.
    pub ice_pwd: Option<IcePassword>,
    /// `a=ice-options:` tokens (RFC 8839 §5.6). Each whitespace-
    /// separated token on the offer becomes an entry; `"trickle"` is
    /// the common one that matters for WebRTC interop.
    pub ice_options: Vec<String>,
    /// `a=candidate:` lines (RFC 8839 §5.1). Preserved in offer order
    /// — the pairing algorithm cares about foundation/priority, which
    /// both live on the typed struct.
    pub candidates: Vec<IceCandidate>,
    /// `true` once the peer emits `a=end-of-candidates` (trickle ICE,
    /// RFC 8840 §4.1.1). Until the flag flips, the engine keeps the
    /// candidate set "open" so a follow-on re-INVITE can piggyback
    /// more candidates without triggering a re-negotiation.
    pub end_of_candidates: bool,
}

/// Hash algorithm + colon-separated hex fingerprint bytes per RFC 8122.
///
/// Parsed case-insensitively for the algorithm token (`sha-256` ≈
/// `SHA-256`); hex bytes are preserved verbatim on the wire (standard
/// form is uppercase colon-separated pairs, e.g.
/// `AA:BB:CC:...`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    /// Hash algorithm token — `sha-256`, `sha-1`, etc.
    pub algorithm: String,
    /// Fingerprint value exactly as it appeared on the wire. Keep the
    /// colon-separated hex form so the comparison against the peer's
    /// DTLS cert can be byte-exact.
    pub value: String,
}

/// `a=setup:` attribute (RFC 5763 §5). Controls which DTLS role each
/// endpoint takes during the handshake.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DtlsSetup {
    /// `a=setup:active` — this endpoint initiates the DTLS handshake.
    Active,
    /// `a=setup:passive` — this endpoint awaits the DTLS `ClientHello`.
    Passive,
    /// `a=setup:actpass` — either role acceptable (typical offer).
    ActPass,
    /// `a=setup:holdconn` — reuse of existing DTLS association.
    HoldConn,
}

impl DtlsSetup {
    /// Parse the wire-format token. Case-insensitive per RFC.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "active" => Some(Self::Active),
            "passive" => Some(Self::Passive),
            "actpass" => Some(Self::ActPass),
            "holdconn" => Some(Self::HoldConn),
            _ => None,
        }
    }

    /// Wire-format attribute value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Passive => "passive",
            Self::ActPass => "actpass",
            Self::HoldConn => "holdconn",
        }
    }

    /// Complementary role the answerer should take, per RFC 5763 §5:
    /// `active` ↔ `passive`; `actpass` on offer → `active` on answer
    /// (responder picks the concrete role); `holdconn` also collapses
    /// to `active` because re-use is a client-driven handshake.
    #[must_use]
    pub fn reverse(self) -> Self {
        match self {
            Self::Active => Self::Passive,
            Self::Passive | Self::ActPass | Self::HoldConn => Self::Active,
        }
    }
}

/// ICE password, redacted in `Debug`. ICE pwd is a short-lived
/// integrity secret per RFC 8445; the cheap wrapper prevents accidental
/// leakage through `tracing::debug!` / `dbg!`.
#[derive(Clone, PartialEq, Eq)]
pub struct IcePassword(pub String);

impl fmt::Debug for IcePassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "IcePassword(<redacted; {} bytes>)", self.0.len())
    }
}

/// `a=candidate:` line (RFC 8839 §5.1). Only the fields the pairing
/// algorithm + SDP emitter need are typed; exotic `generation` /
/// `tcptype` / relay-specific tokens land in [`Self::raw_params`] so
/// a round-trip never drops information.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IceCandidate {
    /// Foundation string — opaque grouping key.
    pub foundation: String,
    /// Component id: 1 = RTP, 2 = RTCP (when mux isn't in play).
    pub component: u8,
    /// Transport token — typically `UDP`; TCP variants live in extras.
    pub transport: String,
    /// Priority computed per RFC 8445 §5.1.2.
    pub priority: u32,
    /// Candidate IP address.
    pub address: IpAddr,
    /// Candidate port.
    pub port: u16,
    /// Candidate type: `host`, `srflx`, `prflx`, `relay`.
    pub candidate_type: String,
    /// Optional `raddr` (related address for reflexive / relay).
    pub related_address: Option<IpAddr>,
    /// Optional `rport`.
    pub related_port: Option<u16>,
    /// Everything else (`generation 0`, `network-id 1`, `tcptype
    /// active`) verbatim. Appended to the emitted line in order.
    pub raw_params: Vec<(String, String)>,
}

/// `m=<kind>` — the media type token on the `m=` line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaKind {
    /// `audio`
    Audio,
    /// `video`
    Video,
    /// `application`
    Application,
    /// Anything else, preserved verbatim.
    Other(String),
}

impl MediaKind {
    /// Parse the `<kind>` token on an `m=` line.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            "audio" => Self::Audio,
            "video" => Self::Video,
            "application" => Self::Application,
            other => Self::Other(other.to_owned()),
        }
    }

    /// Render the wire-format token for an `m=` line.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Application => "application",
            Self::Other(s) => s,
        }
    }
}

/// `a=rtpmap:<pt> <name>/<clock>[/<channels>]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtpMap {
    /// Payload type number.
    pub payload_type: u8,
    /// Codec token (`PCMU`, `PCMA`, `opus`, ...). Case-sensitive per
    /// the IANA registry; we preserve the original spelling.
    pub codec: String,
    /// Clock rate in Hz.
    pub clock_rate: u32,
    /// Channel count. `None` is rendered as "no channel parameter";
    /// `Some(1)` is also rendered as absent (RFC 8866 default).
    pub channels: Option<u8>,
}

impl RtpMap {
    /// Compare codecs case-insensitively.
    #[must_use]
    pub fn codec_eq_ignore_ascii_case(&self, name: &str) -> bool {
        self.codec.eq_ignore_ascii_case(name)
    }
}

/// Media direction attribute (session or media level).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum Direction {
    /// `a=sendrecv` (default when no attribute is present).
    #[default]
    SendRecv,
    /// `a=sendonly`
    SendOnly,
    /// `a=recvonly`
    RecvOnly,
    /// `a=inactive`
    Inactive,
}

impl Direction {
    /// Wire-format attribute value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SendRecv => "sendrecv",
            Self::SendOnly => "sendonly",
            Self::RecvOnly => "recvonly",
            Self::Inactive => "inactive",
        }
    }

    /// Direction the peer should use to answer us: invert `send*` /
    /// `recv*`; `sendrecv` and `inactive` map to themselves.
    #[must_use]
    pub fn reverse(self) -> Self {
        match self {
            Self::SendRecv => Self::SendRecv,
            Self::SendOnly => Self::RecvOnly,
            Self::RecvOnly => Self::SendOnly,
            Self::Inactive => Self::Inactive,
        }
    }
}

// ---- Display (serialization) ----

impl fmt::Display for SessionDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SDP uses `\r\n` line endings per RFC 8866 §5.
        writeln_crlf(f, "v=0")?;
        write!(f, "{}", self.origin)?;
        writeln_crlf(f, &format!("s={}", self.session_name))?;
        if let Some(c) = &self.connection {
            write!(f, "{c}")?;
        }
        writeln_crlf(f, "t=0 0")?;
        for m in &self.media {
            write!(f, "{m}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (net, addr_type) = ip_net_tokens(&self.address);
        writeln_crlf(
            f,
            &format!(
                "o={} {} {} {} {} {}",
                self.username, self.session_id, self.session_version, net, addr_type, self.address
            ),
        )
    }
}

impl fmt::Display for ConnectionInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (net, addr_type) = ip_net_tokens(&self.address);
        writeln_crlf(f, &format!("c={net} {addr_type} {}", self.address))
    }
}

impl fmt::Display for MediaDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut m_line = String::new();
        let _ = write!(
            m_line,
            "m={} {} {}",
            self.kind.as_str(),
            self.port,
            self.protocol
        );
        for fmt_pt in &self.formats {
            let _ = write!(m_line, " {fmt_pt}");
        }
        writeln_crlf(f, &m_line)?;

        if let Some(c) = &self.connection {
            write!(f, "{c}")?;
        }
        for r in &self.rtpmap {
            write!(f, "{r}")?;
        }
        for c in &self.crypto {
            writeln_crlf(f, &c.to_sdp_line())?;
        }
        // DTLS-SRTP attrs. Emit order follows §5 examples in
        // draft-ietf-mmusic-sdp-dtls-stuff: fingerprint, setup, then
        // ICE attributes. Skipped entirely on plain RTP/AVP + SDES.
        if let Some(fp) = &self.fingerprint {
            writeln_crlf(f, &format!("a=fingerprint:{} {}", fp.algorithm, fp.value))?;
        }
        if let Some(setup) = self.setup {
            writeln_crlf(f, &format!("a=setup:{}", setup.as_str()))?;
        }
        if let Some(ufrag) = &self.ice_ufrag {
            writeln_crlf(f, &format!("a=ice-ufrag:{ufrag}"))?;
        }
        if let Some(pwd) = &self.ice_pwd {
            writeln_crlf(f, &format!("a=ice-pwd:{}", pwd.0))?;
        }
        if !self.ice_options.is_empty() {
            writeln_crlf(f, &format!("a=ice-options:{}", self.ice_options.join(" ")))?;
        }
        for cand in &self.candidates {
            writeln_crlf(f, &format!("a={cand}"))?;
        }
        if self.end_of_candidates {
            writeln_crlf(f, "a=end-of-candidates")?;
        }
        writeln_crlf(f, &format!("a={}", self.direction.as_str()))?;
        Ok(())
    }
}

impl fmt::Display for IceCandidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "candidate:{} {} {} {} {} {} typ {}",
            self.foundation,
            self.component,
            self.transport,
            self.priority,
            self.address,
            self.port,
            self.candidate_type,
        )?;
        if let Some(ra) = self.related_address {
            write!(f, " raddr {ra}")?;
        }
        if let Some(rp) = self.related_port {
            write!(f, " rport {rp}")?;
        }
        for (k, v) in &self.raw_params {
            write!(f, " {k} {v}")?;
        }
        Ok(())
    }
}

impl fmt::Display for RtpMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let line = match self.channels {
            // RFC 8866: omit `/channels` when it would be the default 1.
            None | Some(1) => {
                format!(
                    "a=rtpmap:{} {}/{}",
                    self.payload_type, self.codec, self.clock_rate
                )
            }
            Some(n) => format!(
                "a=rtpmap:{} {}/{}/{}",
                self.payload_type, self.codec, self.clock_rate, n
            ),
        };
        writeln_crlf(f, &line)
    }
}

/// Network and address type tokens for `o=` / `c=`.
fn ip_net_tokens(ip: &IpAddr) -> (&'static str, &'static str) {
    match ip {
        IpAddr::V4(_) => ("IN", "IP4"),
        IpAddr::V6(_) => ("IN", "IP6"),
    }
}

/// Write one CRLF-terminated line.
fn writeln_crlf(f: &mut fmt::Formatter<'_>, s: &str) -> fmt::Result {
    write!(f, "{s}\r\n")
}
