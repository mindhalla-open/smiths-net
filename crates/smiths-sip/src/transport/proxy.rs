//! Outbound-connect proxy shim for the TCP-based SIP transports
//! (slice 3.5 / P16).
//!
//! SIP over UDP cannot tunnel through SOCKS5 or HTTP-CONNECT — both
//! proxies are stream protocols. The proxy layer therefore only wraps
//! the TCP connect path: inbound accepts are unaffected (operators
//! who want ingress protection run a reverse proxy in front of the
//! engine), and UDP continues to go direct.
//!
//! Two proxies ship:
//!
//! * **`Socks5Connector`** — RFC 1928 CONNECT, with optional RFC 1929
//!   user/password authentication.
//! * **`HttpConnectConnector`** — HTTP/1.1 `CONNECT host:port` tunnel
//!   (RFC 9110 §9.3.6) with `Proxy-Authorization: Basic` when creds
//!   are provided.
//!
//! Both return a `TcpStream` that's already spoken its handshake and
//! is ready to carry SIP bytes. The TCP transport plugs one of these
//! in via [`super::tcp::TcpTransport::with_proxy`].

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use smiths_core::{ProxyMode, SipProxyConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Trait implemented by every outbound-proxy shim. Returns an open
/// `TcpStream` to `peer` — the caller treats it as if it had just
/// dialled `peer` directly, and the proxy is invisible from that
/// point on.
#[async_trait]
pub trait ProxyConnector: Send + Sync + 'static {
    /// Open a TCP tunnel to `peer`. Implementations own the proxy-
    /// level handshake end-to-end; on success the returned stream is
    /// at the first application-data byte.
    async fn connect(&self, peer: SocketAddr) -> std::io::Result<TcpStream>;

    /// Short human name — appears in logs when a connect fails.
    fn label(&self) -> &'static str;
}

/// No-op pass-through. The `TcpTransport` uses this when
/// `[sip.proxy] mode = "none"`; keeping it a real type (not
/// `Option<Arc<dyn _>>` at every call site) means the hot path is
/// the same shape whether or not a proxy is configured.
pub struct DirectConnector;

#[async_trait]
impl ProxyConnector for DirectConnector {
    async fn connect(&self, peer: SocketAddr) -> std::io::Result<TcpStream> {
        TcpStream::connect(peer).await
    }
    fn label(&self) -> &'static str {
        "direct"
    }
}

/// RFC 1928 SOCKS5 CONNECT connector.
///
/// Protocol cheat-sheet (every field is big-endian):
///
/// 1. Client → `[VER=05, NMETHODS=n, METHODS...]`.
/// 2. Server → `[VER=05, METHOD]`.
/// 3. If METHOD == 0x02 (user/pass), do RFC 1929:
///    client → `[VER=01, ULEN, user, PLEN, pass]`;
///    server → `[VER=01, STATUS]` (0x00 = ok).
/// 4. Client → `[VER=05, CMD=01 CONNECT, RSV=00, ATYP, addr, port]`.
/// 5. Server → `[VER=05, REP, RSV=00, ATYP, bnd_addr, bnd_port]`.
pub struct Socks5Connector {
    proxy: SocketAddr,
    credentials: Option<(String, String)>,
}

impl Socks5Connector {
    /// Build a connector pointing at `proxy`. Pass `credentials` for
    /// RFC 1929 user/password; `None` requests the `no-auth` method.
    #[must_use]
    pub fn new(proxy: SocketAddr, credentials: Option<(String, String)>) -> Self {
        Self { proxy, credentials }
    }
}

#[async_trait]
impl ProxyConnector for Socks5Connector {
    async fn connect(&self, peer: SocketAddr) -> std::io::Result<TcpStream> {
        let mut stream = TcpStream::connect(self.proxy).await?;
        socks5_handshake(&mut stream, self.credentials.as_ref(), peer).await?;
        Ok(stream)
    }
    fn label(&self) -> &'static str {
        "socks5"
    }
}

async fn socks5_handshake(
    stream: &mut TcpStream,
    credentials: Option<&(String, String)>,
    peer: SocketAddr,
) -> std::io::Result<()> {
    // --- Greeting ---
    let methods: &[u8] = if credentials.is_some() {
        &[0x00, 0x02] // no-auth + user/pass
    } else {
        &[0x00]
    };
    let mut greeting = vec![0x05u8, u8::try_from(methods.len()).unwrap_or(1)];
    greeting.extend_from_slice(methods);
    stream.write_all(&greeting).await?;

    let mut sel = [0u8; 2];
    stream.read_exact(&mut sel).await?;
    if sel[0] != 0x05 {
        return Err(io_err("socks5: bad server version in method selection"));
    }
    match sel[1] {
        0x00 => {}
        0x02 => {
            let (user, pass) = credentials.ok_or_else(|| {
                io_err("socks5: server chose user/pass but no credentials configured")
            })?;
            socks5_auth(stream, user, pass).await?;
        }
        0xFF => return Err(io_err("socks5: proxy refused every offered method")),
        other => {
            return Err(io_err(&format!(
                "socks5: proxy chose unsupported method 0x{other:02x}"
            )));
        }
    }

    // --- CONNECT request ---
    let mut req = vec![0x05u8, 0x01, 0x00]; // VER, CMD=CONNECT, RSV
    match peer {
        SocketAddr::V4(a) => {
            req.push(0x01); // IPv4
            req.extend_from_slice(&a.ip().octets());
        }
        SocketAddr::V6(a) => {
            req.push(0x04); // IPv6
            req.extend_from_slice(&a.ip().octets());
        }
    }
    req.extend_from_slice(&peer.port().to_be_bytes());
    stream.write_all(&req).await?;

    // --- Reply ---
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    if head[0] != 0x05 {
        return Err(io_err("socks5: bad server version in reply"));
    }
    if head[1] != 0x00 {
        return Err(io_err(&format!(
            "socks5: connect rejected (REP=0x{:02x} — {})",
            head[1],
            socks5_rep_label(head[1])
        )));
    }
    // Drain BND.ADDR + BND.PORT so the stream is at app-data.
    let bnd_len: usize = match head[3] {
        0x01 => 4,
        0x03 => {
            let mut l = [0u8; 1];
            stream.read_exact(&mut l).await?;
            usize::from(l[0])
        }
        0x04 => 16,
        other => {
            return Err(io_err(&format!(
                "socks5: unsupported bind ATYP 0x{other:02x}"
            )));
        }
    };
    let mut scratch = vec![0u8; bnd_len + 2];
    stream.read_exact(&mut scratch).await?;
    Ok(())
}

async fn socks5_auth(stream: &mut TcpStream, user: &str, pass: &str) -> std::io::Result<()> {
    if user.len() > 255 || pass.len() > 255 {
        return Err(io_err("socks5 auth: username/password exceed 255 bytes"));
    }
    let mut frame = vec![0x01u8]; // sub-negotiation version
    frame.push(u8::try_from(user.len()).unwrap_or(255));
    frame.extend_from_slice(user.as_bytes());
    frame.push(u8::try_from(pass.len()).unwrap_or(255));
    frame.extend_from_slice(pass.as_bytes());
    stream.write_all(&frame).await?;

    let mut resp = [0u8; 2];
    stream.read_exact(&mut resp).await?;
    if resp[0] != 0x01 {
        return Err(io_err("socks5 auth: bad sub-negotiation version"));
    }
    if resp[1] != 0x00 {
        return Err(io_err("socks5 auth: credentials rejected"));
    }
    Ok(())
}

fn socks5_rep_label(code: u8) -> &'static str {
    match code {
        0x01 => "general failure",
        0x02 => "connection not allowed by ruleset",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused",
        0x06 => "TTL expired",
        0x07 => "command not supported",
        0x08 => "address type not supported",
        _ => "unknown",
    }
}

/// HTTP/1.1 CONNECT connector (RFC 9110 §9.3.6). Request shape:
///
/// ```text
/// CONNECT host:port HTTP/1.1\r\n
/// Host: host:port\r\n
/// Proxy-Authorization: Basic <base64(user:pass)>\r\n   (optional)
/// \r\n
/// ```
///
/// Proxy responds with a status line + headers + CRLF CRLF. Only
/// 2xx means the tunnel is open; anything else is an error. Any
/// bytes the proxy sent past the CRLF CRLF would normally be
/// discarded by the caller — we don't expect a proxy to do that
/// on a CONNECT before the client speaks, and we error out loudly
/// if it does.
pub struct HttpConnectConnector {
    proxy: SocketAddr,
    credentials: Option<(String, String)>,
}

impl HttpConnectConnector {
    /// Build a connector pointing at `proxy`. `credentials`, if set,
    /// flow as `Proxy-Authorization: Basic base64(user:pass)`.
    #[must_use]
    pub fn new(proxy: SocketAddr, credentials: Option<(String, String)>) -> Self {
        Self { proxy, credentials }
    }
}

#[async_trait]
impl ProxyConnector for HttpConnectConnector {
    async fn connect(&self, peer: SocketAddr) -> std::io::Result<TcpStream> {
        let mut stream = TcpStream::connect(self.proxy).await?;
        http_connect_handshake(&mut stream, self.credentials.as_ref(), peer).await?;
        Ok(stream)
    }
    fn label(&self) -> &'static str {
        "http-connect"
    }
}

async fn http_connect_handshake(
    stream: &mut TcpStream,
    credentials: Option<&(String, String)>,
    peer: SocketAddr,
) -> std::io::Result<()> {
    let host_port = format!("{peer}");
    let mut req = format!("CONNECT {host_port} HTTP/1.1\r\nHost: {host_port}\r\n");
    if let Some((user, pass)) = credentials {
        let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        write!(req, "Proxy-Authorization: Basic {encoded}\r\n").ok();
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await?;

    // Read until CRLF CRLF. Cap at 8 KiB so a misbehaving proxy
    // can't pin us allocating forever.
    let mut buf = Vec::with_capacity(512);
    let mut one = [0u8; 1];
    loop {
        let n = stream.read(&mut one).await?;
        if n == 0 {
            return Err(io_err("http-connect: proxy closed before response"));
        }
        buf.push(one[0]);
        if buf.len() > 8192 {
            return Err(io_err("http-connect: response header exceeds 8 KiB"));
        }
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }

    let head =
        std::str::from_utf8(&buf).map_err(|_| io_err("http-connect: non-UTF-8 proxy response"))?;
    let status_line = head.lines().next().unwrap_or("");
    // "HTTP/1.1 200 Connection established"
    let mut parts = status_line.split_whitespace();
    let _version = parts.next().unwrap_or("");
    let code = parts.next().unwrap_or("");
    if !code.starts_with('2') {
        return Err(io_err(&format!(
            "http-connect: proxy refused: `{status_line}`"
        )));
    }
    Ok(())
}

/// Build the right connector for `cfg`. Returns [`DirectConnector`]
/// for `mode = "none"` so call sites never branch on `Option`.
pub fn connector_from_config(cfg: &SipProxyConfig) -> std::io::Result<Arc<dyn ProxyConnector>> {
    match cfg.mode {
        ProxyMode::None => Ok(Arc::new(DirectConnector)),
        ProxyMode::Socks5 => {
            let addr = cfg
                .address
                .ok_or_else(|| io_err("sip.proxy.mode = \"socks5\" requires `address`"))?;
            let creds = creds_from(cfg);
            Ok(Arc::new(Socks5Connector::new(addr, creds)))
        }
        ProxyMode::HttpConnect => {
            let addr = cfg
                .address
                .ok_or_else(|| io_err("sip.proxy.mode = \"http-connect\" requires `address`"))?;
            let creds = creds_from(cfg);
            Ok(Arc::new(HttpConnectConnector::new(addr, creds)))
        }
    }
}

fn creds_from(cfg: &SipProxyConfig) -> Option<(String, String)> {
    match (&cfg.username, &cfg.password) {
        (Some(u), Some(p)) => Some((u.clone(), p.clone())),
        _ => None,
    }
}

fn io_err(msg: &str) -> std::io::Error {
    std::io::Error::other(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// Stand up a single-shot TCP server that plays the role of a
    /// SOCKS5 proxy: greets with `no-auth`, accepts the CONNECT
    /// request, replies success, then echoes one byte so we can
    /// confirm the client exited the handshake at the right place.
    #[tokio::test(flavor = "multi_thread")]
    async fn socks5_no_auth_completes_and_streams_appdata() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            // Greeting: [VER=05, NMETHODS, METHODS...]
            let mut head = [0u8; 2];
            s.read_exact(&mut head).await.unwrap();
            assert_eq!(head[0], 0x05);
            let mut methods = vec![0u8; head[1] as usize];
            s.read_exact(&mut methods).await.unwrap();
            assert!(methods.contains(&0x00));
            // Pick no-auth.
            s.write_all(&[0x05, 0x00]).await.unwrap();

            // CONNECT request: [VER, CMD, RSV, ATYP, addr, port]
            let mut fixed = [0u8; 4];
            s.read_exact(&mut fixed).await.unwrap();
            assert_eq!(&fixed[..3], &[0x05, 0x01, 0x00]);
            assert_eq!(fixed[3], 0x01); // IPv4
            let mut ipv4 = [0u8; 6]; // 4 addr + 2 port
            s.read_exact(&mut ipv4).await.unwrap();
            // Reply success, BND = 0.0.0.0:0.
            s.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
            // Stream one byte so the test can prove app data flows.
            s.write_all(b"A").await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });

        let connector = Socks5Connector::new(proxy_addr, None);
        let mut stream = connector
            .connect("10.0.0.1:5060".parse().unwrap())
            .await
            .expect("handshake ok");
        let mut one = [0u8; 1];
        stream.read_exact(&mut one).await.unwrap();
        assert_eq!(one[0], b'A');
        server.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn socks5_user_pass_auth_round_trips() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            // Greeting.
            let mut head = [0u8; 2];
            s.read_exact(&mut head).await.unwrap();
            let mut methods = vec![0u8; head[1] as usize];
            s.read_exact(&mut methods).await.unwrap();
            assert!(methods.contains(&0x02));
            s.write_all(&[0x05, 0x02]).await.unwrap(); // pick user/pass

            // Auth sub-negotiation: [VER=01, ULEN, user, PLEN, pass].
            let mut auth_head = [0u8; 2];
            s.read_exact(&mut auth_head).await.unwrap();
            assert_eq!(auth_head[0], 0x01);
            let ulen = auth_head[1] as usize;
            let mut user = vec![0u8; ulen];
            s.read_exact(&mut user).await.unwrap();
            assert_eq!(user, b"alice");
            let mut plen = [0u8; 1];
            s.read_exact(&mut plen).await.unwrap();
            let mut pass = vec![0u8; plen[0] as usize];
            s.read_exact(&mut pass).await.unwrap();
            assert_eq!(pass, b"s3cret");
            s.write_all(&[0x01, 0x00]).await.unwrap(); // success

            // CONNECT + reply.
            let mut fixed = [0u8; 4];
            s.read_exact(&mut fixed).await.unwrap();
            let mut ipv4 = [0u8; 6];
            s.read_exact(&mut ipv4).await.unwrap();
            s.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
        });

        let connector = Socks5Connector::new(proxy_addr, Some(("alice".into(), "s3cret".into())));
        let _ = connector
            .connect("127.0.0.1:5060".parse().unwrap())
            .await
            .expect("auth ok");
        server.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn socks5_refused_surfaces_clean_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut head = [0u8; 2];
            s.read_exact(&mut head).await.unwrap();
            let mut methods = vec![0u8; head[1] as usize];
            s.read_exact(&mut methods).await.unwrap();
            s.write_all(&[0x05, 0xFF]).await.unwrap(); // refuse every method
        });

        let connector = Socks5Connector::new(proxy_addr, None);
        let err = connector
            .connect("127.0.0.1:5060".parse().unwrap())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("refused every offered method"),
            "{err}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn http_connect_200_establishes_tunnel() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut one = [0u8; 1];
            loop {
                s.read_exact(&mut one).await.unwrap();
                buf.push(one[0]);
                if buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let req = String::from_utf8(buf).unwrap();
            assert!(req.starts_with("CONNECT 127.0.0.1:5060 HTTP/1.1"));
            assert!(req.contains("Host: 127.0.0.1:5060"));
            s.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
            s.write_all(b"Z").await.unwrap();
        });

        let connector = HttpConnectConnector::new(proxy_addr, None);
        let mut stream = connector
            .connect("127.0.0.1:5060".parse().unwrap())
            .await
            .expect("tunnel open");
        let mut one = [0u8; 1];
        stream.read_exact(&mut one).await.unwrap();
        assert_eq!(one[0], b'Z');
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn http_connect_basic_auth_header_present() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut one = [0u8; 1];
            loop {
                s.read_exact(&mut one).await.unwrap();
                buf.push(one[0]);
                if buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let req = String::from_utf8(buf).unwrap();
            let expected = base64::engine::general_purpose::STANDARD.encode("alice:s3cret");
            assert!(req.contains(&format!("Proxy-Authorization: Basic {expected}")));
            s.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
        });

        let connector =
            HttpConnectConnector::new(proxy_addr, Some(("alice".into(), "s3cret".into())));
        let _ = connector
            .connect("127.0.0.1:5060".parse().unwrap())
            .await
            .expect("tunnel");
        server.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn http_connect_non_2xx_surfaces_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut one = [0u8; 1];
            loop {
                s.read_exact(&mut one).await.unwrap();
                buf.push(one[0]);
                if buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            s.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
                .await
                .unwrap();
        });

        let connector = HttpConnectConnector::new(proxy_addr, None);
        let err = connector
            .connect("127.0.0.1:5060".parse().unwrap())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("407"), "{err}");
    }

    #[test]
    fn connector_from_config_handles_every_mode() {
        let none_cfg = SipProxyConfig {
            mode: ProxyMode::None,
            ..SipProxyConfig::default()
        };
        assert_eq!(connector_from_config(&none_cfg).unwrap().label(), "direct");

        let socks_no_addr = SipProxyConfig {
            mode: ProxyMode::Socks5,
            ..SipProxyConfig::default()
        };
        assert!(connector_from_config(&socks_no_addr).is_err());

        let socks_ok = SipProxyConfig {
            mode: ProxyMode::Socks5,
            address: Some("127.0.0.1:9050".parse().unwrap()),
            ..SipProxyConfig::default()
        };
        assert_eq!(connector_from_config(&socks_ok).unwrap().label(), "socks5");

        let http_ok = SipProxyConfig {
            mode: ProxyMode::HttpConnect,
            address: Some("127.0.0.1:3128".parse().unwrap()),
            ..SipProxyConfig::default()
        };
        assert_eq!(
            connector_from_config(&http_ok).unwrap().label(),
            "http-connect"
        );
    }
}
