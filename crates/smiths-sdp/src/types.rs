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
    /// Protocol string (e.g. `RTP/AVP`, `RTP/SAVP`).
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
        writeln_crlf(f, &format!("a={}", self.direction.as_str()))?;
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
