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
    /// Session-level `a=group:` lines (RFC 5888), e.g.
    /// `a=group:BUNDLE 0 1` on a browser offer.
    pub groups: Vec<Group>,
    /// `true` when the session carries `a=ice-lite` (RFC 8839 §5.3).
    pub ice_lite: bool,
    /// Media-level blocks. At least one is required in practice.
    pub media: Vec<MediaDescription>,
}

impl SessionDescription {
    /// `a=group:BUNDLE` mids, in offer order. Empty when the session
    /// has no BUNDLE group.
    #[must_use]
    pub fn bundle_mids(&self) -> Vec<&str> {
        self.groups
            .iter()
            .filter(|g| g.semantics.eq_ignore_ascii_case("BUNDLE"))
            .flat_map(|g| g.mids.iter().map(String::as_str))
            .collect()
    }
}

/// `a=group:<semantics> <mid> [<mid>...]` (RFC 5888).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    /// Grouping semantics token — `BUNDLE`, `LS`, `FID`,...
    pub semantics: String,
    /// Identification tags (`a=mid:` values) of the grouped media.
    pub mids: Vec<String>,
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
    /// Protocol string (e.g. `RTP/AVP`, `RTP/SAVP`, `UDP/TLS/RTP/SAVP`,
    /// `udptl`).
    pub protocol: String,
    /// Ordered list of `m=` format tokens exactly as they appeared on
    /// the wire. For RTP profiles these are payload-type numbers
    /// (`"0"`, `"111"`); for T.38 the token is `"t38"`. Use
    /// [`Self::payload_types`] for the numeric view.
    pub formats: Vec<String>,
    /// Parsed `a=rtpmap` attributes keyed by payload type.
    pub rtpmap: Vec<RtpMap>,
    /// Parsed `a=fmtp:` lines (RFC 8866 §6.15), in wire order.
    pub fmtp: Vec<Fmtp>,
    /// `a=ptime:` — preferred packetization time in milliseconds.
    pub ptime: Option<u32>,
    /// `a=maxptime:` — largest packetization time the peer accepts.
    pub maxptime: Option<u32>,
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
    /// A session-level fingerprint is inherited by every media block
    /// that doesn't carry its own.
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
    /// `a=mid:` identification tag (RFC 5888 §4). Browsers put one on
    /// every m-line; the answer must echo it verbatim.
    pub mid: Option<String>,
    /// `a=extmap:` RTP header-extension mappings (RFC 8285 §5).
    pub extmap: Vec<ExtMap>,
    /// `true` when the block carries `a=rtcp-mux` (RFC 5761): RTCP is
    /// multiplexed on the RTP port.
    pub rtcp_mux: bool,
    /// `a=rtcp:` attribute (RFC 3605) — explicit RTCP port (and
    /// optional address) when the peer doesn't use RTP port + 1.
    pub rtcp: Option<RtcpAttr>,
    /// `a=rtcp-fb:` RTCP feedback capabilities (RFC 4585 §4.2).
    pub rtcp_fb: Vec<RtcpFeedback>,
    /// T.38 fax attributes (`a=T38FaxVersion:` and friends) on an
    /// `m=image... udptl t38` block. `None` when no `a=T38…` line
    /// was present.
    pub t38: Option<T38Params>,
}

impl MediaDescription {
    /// A media block with the given `m=` line and no attributes:
    /// `sendrecv`, no formats, no rtpmap. Callers fill in the fields
    /// they need.
    #[must_use]
    pub fn new(kind: MediaKind, port: u16, protocol: impl Into<String>) -> Self {
        Self {
            kind,
            port,
            protocol: protocol.into(),
            formats: Vec::new(),
            rtpmap: Vec::new(),
            fmtp: Vec::new(),
            ptime: None,
            maxptime: None,
            crypto: Vec::new(),
            direction: Direction::SendRecv,
            connection: None,
            fingerprint: None,
            setup: None,
            ice_ufrag: None,
            ice_pwd: None,
            ice_options: Vec::new(),
            candidates: Vec::new(),
            end_of_candidates: false,
            mid: None,
            extmap: Vec::new(),
            rtcp_mux: false,
            rtcp: None,
            rtcp_fb: Vec::new(),
            t38: None,
        }
    }

    /// Numeric RTP payload types from the `m=` format list, in wire
    /// order. Non-numeric tokens (`t38`) are skipped.
    pub fn payload_types(&self) -> impl Iterator<Item = u8> + '_ {
        self.formats.iter().filter_map(|f| f.parse::<u8>().ok())
    }

    /// `true` when the `m=` format list contains `token`
    /// (case-insensitive — `t38` and `T38` are the same format).
    #[must_use]
    pub fn has_format(&self, token: &str) -> bool {
        self.formats.iter().any(|f| f.eq_ignore_ascii_case(token))
    }

    /// `a=rtpmap` entry for payload type `pt`, if any.
    #[must_use]
    pub fn rtpmap_for(&self, pt: u8) -> Option<&RtpMap> {
        self.rtpmap.iter().find(|r| r.payload_type == pt)
    }

    /// `a=fmtp` entry for payload type `pt`, if any.
    #[must_use]
    pub fn fmtp_for(&self, pt: u8) -> Option<&Fmtp> {
        let token = pt.to_string();
        self.fmtp.iter().find(|f| f.format == token)
    }

    /// RTCP port the peer receives on for this block: the `a=rtcp:`
    /// port when present, the RTP port itself under `a=rtcp-mux`,
    /// otherwise RTP port + 1 (RFC 3550 §11). `None` for a rejected
    /// (port 0) stream.
    #[must_use]
    pub fn rtcp_port(&self) -> Option<u16> {
        if self.port == 0 {
            return None;
        }
        if let Some(r) = &self.rtcp {
            return Some(r.port);
        }
        if self.rtcp_mux {
            return Some(self.port);
        }
        self.port.checked_add(1)
    }
}

/// `a=fmtp:<format> <parameters>` (RFC 8866 §6.15). The parameter
/// string is kept verbatim — its grammar is codec-specific.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fmtp {
    /// Format token the parameters apply to (usually a payload type).
    pub format: String,
    /// Everything after the first space, verbatim (e.g.
    /// `minptime=10;useinbandfec=1` or `0-16`).
    pub params: String,
}

/// `a=extmap:<id>[/<direction>] <uri> [<attributes>]` (RFC 8285 §5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtMap {
    /// Local identifier (1–14 for one-byte headers, up to 255 for
    /// two-byte headers).
    pub id: u16,
    /// Optional direction qualifier on the id (`1/sendonly`).
    pub direction: Option<Direction>,
    /// Extension URI.
    pub uri: String,
    /// Optional extension-specific attributes after the URI.
    pub attributes: Option<String>,
}

/// `a=rtcp:<port> [<nettype> <addrtype> <address>]` (RFC 3605).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtcpAttr {
    /// RTCP port.
    pub port: u16,
    /// Optional RTCP address when it differs from the `c=` line.
    pub address: Option<IpAddr>,
}

/// `a=rtcp-fb:<format> <value>` (RFC 4585 §4.2), e.g.
/// `a=rtcp-fb:96 nack pli` or `a=rtcp-fb:* ccm fir`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtcpFeedback {
    /// Payload type the capability applies to, or `*` for all.
    pub format: String,
    /// Feedback type and parameters, verbatim (`nack pli`,
    /// `transport-cc`, `goog-remb`).
    pub value: String,
}

impl RtcpFeedback {
    /// `true` when this line applies to payload type `pt` — either an
    /// exact match or the `*` wildcard.
    #[must_use]
    pub fn applies_to(&self, pt: u8) -> bool {
        self.format == "*" || self.format.parse::<u8>().ok() == Some(pt)
    }
}

/// T.38 session parameters from `a=T38…` attribute lines
/// (ITU-T T.38 Annex D, RFC 3362).
///
/// Every field is optional — T.38 specifies defaults for each
/// attribute, and the engine has no business inventing values the
/// peer didn't offer. A gateway that *needs* a specific knob is the
/// one that should surface an error when it's missing. Attribute
/// names the model doesn't type (`T38FaxMaxIFP`, `T38VendorInfo`,
/// …) are preserved verbatim in [`Self::extra`] so a round-trip
/// never drops information.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct T38Params {
    /// `a=T38FaxVersion:<n>` — protocol version. Typical values are
    /// 0, 1, 2, 3.
    pub version: Option<u8>,
    /// `a=T38MaxBitRate:<bps>` — cap on the modem rate the terminal
    /// will attempt. Often 14400.
    pub max_bit_rate: Option<u32>,
    /// `a=T38FaxFillBitRemoval` — fill bits are stripped before
    /// transport. A bare attribute or `:1`/`:true` means `true`.
    pub fill_bit_removal: bool,
    /// `a=T38FaxTranscodingMMR` — MMR transcoding capability.
    pub transcoding_mmr: bool,
    /// `a=T38FaxTranscodingJBIG` — JBIG transcoding capability.
    pub transcoding_jbig: bool,
    /// `a=T38FaxRateManagement:<token>` — usually `transferredTCF`
    /// (preferred) or `localTCF`.
    pub rate_management: Option<String>,
    /// `a=T38FaxMaxBuffer:<bytes>` — terminal buffer advertisement.
    pub max_buffer: Option<u32>,
    /// `a=T38FaxMaxDatagram:<bytes>` — largest UDPTL datagram the
    /// terminal accepts.
    pub max_datagram: Option<u32>,
    /// `a=T38FaxUdpEC:<token>` — error-correction profile:
    /// `t38UDPRedundancy` or `t38UDPFEC`.
    pub udp_ec: Option<String>,
    /// Other `a=T38…` attributes as `(name, value)`; a bare flag
    /// attribute has `None` for its value.
    pub extra: Vec<(String, Option<String>)>,
}

impl T38Params {
    /// Prefix shared by every T.38 attribute name.
    pub const ATTR_PREFIX: &'static str = "T38";

    /// Apply one `a=` attribute (without the leading `a=`) to the
    /// parameter set. Returns `false` — leaving `self` untouched —
    /// when the attribute isn't a `T38…` one.
    pub fn apply_attribute(&mut self, attr: &str) -> bool {
        if !attr.starts_with(Self::ATTR_PREFIX) {
            return false;
        }
        let (name, value) = match attr.split_once(':') {
            Some((n, v)) => (n.trim(), Some(v.trim())),
            None => (attr.trim(), None),
        };
        match name {
            "T38FaxVersion" => self.version = value.and_then(|v| v.parse().ok()),
            "T38MaxBitRate" => self.max_bit_rate = value.and_then(|v| v.parse().ok()),
            "T38FaxFillBitRemoval" => self.fill_bit_removal = flag_value(value),
            "T38FaxTranscodingMMR" => self.transcoding_mmr = flag_value(value),
            "T38FaxTranscodingJBIG" => self.transcoding_jbig = flag_value(value),
            "T38FaxRateManagement" => self.rate_management = value.map(str::to_owned),
            "T38FaxMaxBuffer" => self.max_buffer = value.and_then(|v| v.parse().ok()),
            "T38FaxMaxDatagram" => self.max_datagram = value.and_then(|v| v.parse().ok()),
            "T38FaxUdpEC" => self.udp_ec = value.map(str::to_owned),
            other => self
                .extra
                .push((other.to_owned(), value.map(str::to_owned))),
        }
        true
    }

    /// Parse a set of attribute lines (each without the leading
    /// `a=`). Non-T.38 lines are ignored.
    #[must_use]
    pub fn parse_attrs<S: AsRef<str>>(attrs: &[S]) -> Self {
        let mut out = Self::default();
        for line in attrs {
            out.apply_attribute(line.as_ref());
        }
        out
    }

    /// Render the populated fields as wire-format attribute lines
    /// without the leading `a=`, in the order T.38 Annex D lists
    /// them.
    #[must_use]
    pub fn render_attrs(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(v) = self.version {
            out.push(format!("T38FaxVersion:{v}"));
        }
        if let Some(r) = self.max_bit_rate {
            out.push(format!("T38MaxBitRate:{r}"));
        }
        if self.fill_bit_removal {
            out.push("T38FaxFillBitRemoval".to_owned());
        }
        if self.transcoding_mmr {
            out.push("T38FaxTranscodingMMR".to_owned());
        }
        if self.transcoding_jbig {
            out.push("T38FaxTranscodingJBIG".to_owned());
        }
        if let Some(r) = &self.rate_management {
            out.push(format!("T38FaxRateManagement:{r}"));
        }
        if let Some(b) = self.max_buffer {
            out.push(format!("T38FaxMaxBuffer:{b}"));
        }
        if let Some(d) = self.max_datagram {
            out.push(format!("T38FaxMaxDatagram:{d}"));
        }
        if let Some(e) = &self.udp_ec {
            out.push(format!("T38FaxUdpEC:{e}"));
        }
        for (name, value) in &self.extra {
            match value {
                Some(v) => out.push(format!("{name}:{v}")),
                None => out.push(name.clone()),
            }
        }
        out
    }

    /// Defaults for a locally-initiated T.38 offer: version 0,
    /// 14 400 bps, `transferredTCF`, 72-byte buffer, 316-byte
    /// datagrams, UDP redundancy — what typical soft-PBXes and ATAs
    /// emit.
    #[must_use]
    pub fn sensible_offer() -> Self {
        Self {
            version: Some(0),
            max_bit_rate: Some(14_400),
            fill_bit_removal: false,
            transcoding_mmr: false,
            transcoding_jbig: false,
            rate_management: Some("transferredTCF".into()),
            max_buffer: Some(72),
            max_datagram: Some(316),
            udp_ec: Some("t38UDPRedundancy".into()),
            extra: Vec::new(),
        }
    }
}

/// Boolean T.38 attributes appear bare (`a=T38FaxFillBitRemoval`) or
/// with an explicit `0`/`1` / `false`/`true` value.
fn flag_value(value: Option<&str>) -> bool {
    match value {
        None => true,
        Some(v) => !(v == "0" || v.eq_ignore_ascii_case("false")),
    }
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
    /// `image` — T.38 FAX-over-IP uses `m=image... udptl t38`.
    Image,
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
            "image" => Self::Image,
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
            Self::Image => "image",
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

    /// `true` for the RFC 4733 DTMF event payload (`telephone-event`).
    #[must_use]
    pub fn is_telephone_event(&self) -> bool {
        self.codec_eq_ignore_ascii_case("telephone-event")
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
    /// Parse a direction token; `None` for anything else.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sendrecv" => Some(Self::SendRecv),
            "sendonly" => Some(Self::SendOnly),
            "recvonly" => Some(Self::RecvOnly),
            "inactive" => Some(Self::Inactive),
            _ => None,
        }
    }

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
        for g in &self.groups {
            writeln_crlf(f, &format!("a=group:{} {}", g.semantics, g.mids.join(" ")))?;
        }
        if self.ice_lite {
            writeln_crlf(f, "a=ice-lite")?;
        }
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
        for fmt_tok in &self.formats {
            let _ = write!(m_line, " {fmt_tok}");
        }
        writeln_crlf(f, &m_line)?;

        if let Some(c) = &self.connection {
            write!(f, "{c}")?;
        }
        for r in &self.rtpmap {
            write!(f, "{r}")?;
        }
        for p in &self.fmtp {
            writeln_crlf(f, &format!("a=fmtp:{} {}", p.format, p.params))?;
        }
        if let Some(pt) = self.ptime {
            writeln_crlf(f, &format!("a=ptime:{pt}"))?;
        }
        if let Some(mpt) = self.maxptime {
            writeln_crlf(f, &format!("a=maxptime:{mpt}"))?;
        }
        for c in &self.crypto {
            writeln_crlf(f, &c.to_sdp_line())?;
        }
        // DTLS-SRTP attrs: fingerprint, setup, then ICE attributes.
        // Skipped entirely on plain RTP/AVP + SDES.
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
        if let Some(mid) = &self.mid {
            writeln_crlf(f, &format!("a=mid:{mid}"))?;
        }
        for e in &self.extmap {
            writeln_crlf(f, &format!("a={e}"))?;
        }
        if self.rtcp_mux {
            writeln_crlf(f, "a=rtcp-mux")?;
        }
        if let Some(r) = &self.rtcp {
            match r.address {
                Some(addr) => {
                    let (net, addr_type) = ip_net_tokens(&addr);
                    writeln_crlf(f, &format!("a=rtcp:{} {net} {addr_type} {addr}", r.port))?;
                }
                None => writeln_crlf(f, &format!("a=rtcp:{}", r.port))?,
            }
        }
        for fb in &self.rtcp_fb {
            writeln_crlf(f, &format!("a=rtcp-fb:{} {}", fb.format, fb.value))?;
        }
        if let Some(t38) = &self.t38 {
            for line in t38.render_attrs() {
                writeln_crlf(f, &format!("a={line}"))?;
            }
        }
        writeln_crlf(f, &format!("a={}", self.direction.as_str()))?;
        Ok(())
    }
}

impl fmt::Display for ExtMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "extmap:{}", self.id)?;
        if let Some(d) = self.direction {
            write!(f, "/{}", d.as_str())?;
        }
        write!(f, " {}", self.uri)?;
        if let Some(a) = &self.attributes {
            write!(f, " {a}")?;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t38_flag_attributes_parse_bare_and_valued() {
        let p = T38Params::parse_attrs(&[
            "T38FaxFillBitRemoval",
            "T38FaxTranscodingMMR:0",
            "T38FaxTranscodingJBIG:1",
            "T38FaxMaxIFP:40",
        ]);
        assert!(p.fill_bit_removal);
        assert!(!p.transcoding_mmr);
        assert!(p.transcoding_jbig);
        assert_eq!(
            p.extra,
            vec![("T38FaxMaxIFP".to_owned(), Some("40".to_owned()))]
        );
        assert_eq!(
            p.render_attrs(),
            vec![
                "T38FaxFillBitRemoval",
                "T38FaxTranscodingJBIG",
                "T38FaxMaxIFP:40"
            ]
        );
    }

    #[test]
    fn t38_apply_attribute_ignores_non_t38_lines() {
        let mut p = T38Params::default();
        assert!(!p.apply_attribute("rtpmap:0 PCMU/8000"));
        assert_eq!(p, T38Params::default());
    }

    #[test]
    fn rtcp_port_follows_rfc_3550_and_mux_rules() {
        let mut m = MediaDescription::new(MediaKind::Audio, 4000, "RTP/AVP");
        assert_eq!(m.rtcp_port(), Some(4001));
        m.rtcp_mux = true;
        assert_eq!(m.rtcp_port(), Some(4000));
        m.rtcp = Some(RtcpAttr {
            port: 4444,
            address: None,
        });
        assert_eq!(m.rtcp_port(), Some(4444));
        m.port = 0;
        assert_eq!(m.rtcp_port(), None);
    }

    #[test]
    fn payload_types_skip_non_numeric_formats() {
        let mut m = MediaDescription::new(MediaKind::Image, 6250, "udptl");
        m.formats = vec!["t38".into(), "0".into(), "101".into()];
        assert_eq!(m.payload_types().collect::<Vec<u8>>(), vec![0, 101]);
        assert!(m.has_format("T38"));
        assert!(!m.has_format("t30"));
    }
}
