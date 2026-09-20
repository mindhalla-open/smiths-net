//! Transport-level integration for the stream transports: a raw TCP
//! or TLS client writes one framed SIP request into `TcpTransport` /
//! `TlsTransport`, the transport surfaces it as a `Datagram`, and a
//! reply sent through `Transport::send` reaches the client on the
//! same connection. Also covers the inbound connection cap and the
//! idle timeout both transports enforce.

use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rustls::pki_types::{CertificateDer, ServerName};
use smiths_sip::transport::{Datagram, Transport, TransportKind};
use smiths_sip::{TcpTransport, TlsTransport};
use tempfile::NamedTempFile;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_util::sync::CancellationToken;

const OPTIONS: &[u8] = b"OPTIONS sip:engine@localhost SIP/2.0\r\n\
    Via: SIP/2.0/TCP client;branch=z9hG4bK-stream-1\r\n\
    Content-Length: 0\r\n\r\n";
const OK: &[u8] = b"SIP/2.0 200 OK\r\nContent-Length: 0\r\n\r\n";

/// A pinned trust verifier that accepts one specific cert only.
#[derive(Debug)]
struct PinnedCert(CertificateDer<'static>);

impl rustls::client::danger::ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.0.as_ref() {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("unexpected server cert".into()))
        }
    }
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

struct TestCerts {
    cert_der: CertificateDer<'static>,
    cert_file: NamedTempFile,
    key_file: NamedTempFile,
}

fn generate_certs() -> TestCerts {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let mut cert_file = NamedTempFile::new().unwrap();
    cert_file.write_all(cert.cert.pem().as_bytes()).unwrap();
    let mut key_file = NamedTempFile::new().unwrap();
    key_file
        .write_all(cert.signing_key.serialize_pem().as_bytes())
        .unwrap();
    TestCerts {
        cert_der: cert.cert.der().clone(),
        cert_file,
        key_file,
    }
}

async fn tls_connect(
    addr: SocketAddr,
    certs: &TestCerts,
) -> tokio_rustls::client::TlsStream<TcpStream> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client_cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCert(certs.cert_der.clone())))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_cfg));
    let tcp = TcpStream::connect(addr).await.unwrap();
    connector
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .unwrap()
}

/// Write `OPTIONS`, expect the transport to surface it, reply with
/// `OK` through `transport.send`, and read the reply back on `client`.
async fn round_trip<T, S>(transport: &T, rx: &mut mpsc::Receiver<Datagram>, client: &mut S)
where
    T: Transport,
    S: AsyncRead + AsyncWrite + Unpin,
{
    client.write_all(OPTIONS).await.unwrap();
    let dg = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("request never surfaced")
        .unwrap();
    assert_eq!(&dg.bytes[..], OPTIONS);
    transport
        .send(Bytes::from_static(OK), dg.peer)
        .await
        .expect("reply on the live connection");
    let mut buf = vec![0u8; 256];
    let n = timeout(Duration::from_secs(2), client.read(&mut buf))
        .await
        .expect("reply never arrived")
        .unwrap();
    assert_eq!(&buf[..n], OK);
}

/// `true` when the server closed `client` (read returns 0 or an
/// error) within `within`.
async fn closed_by_server<S: AsyncRead + Unpin>(client: &mut S, within: Duration) -> bool {
    let mut buf = [0u8; 16];
    matches!(
        timeout(within, client.read(&mut buf)).await,
        Ok(Ok(0) | Err(_))
    )
}

async fn wait_for_connections(count: impl Fn() -> usize, want: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while count() != want && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(count(), want);
}

// --------------------------------------------------------------- TCP

#[tokio::test(flavor = "multi_thread")]
async fn tcp_request_and_reply_round_trip_through_transport() {
    let cancel = CancellationToken::new();
    let transport = TcpTransport::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    assert_eq!(transport.kind(), TransportKind::Tcp);
    let addr = transport.local_addr().unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    transport.spawn_reader(tx, cancel.clone());

    let mut client = TcpStream::connect(addr).await.unwrap();
    round_trip(&transport, &mut rx, &mut client).await;
    // Second message on the same connection reuses the pool entry.
    round_trip(&transport, &mut rx, &mut client).await;
    assert_eq!(transport.connections(), 1);
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn tcp_connection_cap_refuses_excess_connections() {
    let cancel = CancellationToken::new();
    let transport = TcpTransport::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap()
        .with_max_connections(1);
    let addr = transport.local_addr().unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    transport.spawn_reader(tx, cancel.clone());

    let mut first = TcpStream::connect(addr).await.unwrap();
    round_trip(&transport, &mut rx, &mut first).await;
    assert_eq!(transport.connections(), 1);

    // Over the cap: accepted by the kernel, closed by the transport.
    let mut second = TcpStream::connect(addr).await.unwrap();
    assert!(
        closed_by_server(&mut second, Duration::from_secs(2)).await,
        "second connection must be closed by the cap"
    );
    assert_eq!(transport.connections(), 1);
    // The first connection keeps working.
    round_trip(&transport, &mut rx, &mut first).await;

    // Once the first goes away the slot frees up.
    drop(first);
    wait_for_connections(|| transport.connections(), 0).await;
    let mut third = TcpStream::connect(addr).await.unwrap();
    round_trip(&transport, &mut rx, &mut third).await;
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn tcp_idle_connection_is_closed_after_timeout() {
    let cancel = CancellationToken::new();
    let transport = TcpTransport::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap()
        .with_idle_timeout(Some(Duration::from_millis(300)));
    let addr = transport.local_addr().unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    transport.spawn_reader(tx, cancel.clone());

    let mut client = TcpStream::connect(addr).await.unwrap();
    round_trip(&transport, &mut rx, &mut client).await;
    // Traffic within the window keeps the connection alive...
    tokio::time::sleep(Duration::from_millis(150)).await;
    round_trip(&transport, &mut rx, &mut client).await;
    // ...silence beyond it closes it and releases the pool entry.
    assert!(
        closed_by_server(&mut client, Duration::from_secs(3)).await,
        "idle connection must be closed"
    );
    wait_for_connections(|| transport.connections(), 0).await;
    cancel.cancel();
}

// --------------------------------------------------------------- TLS

#[tokio::test(flavor = "multi_thread")]
async fn tls_request_and_reply_round_trip_through_transport() {
    let certs = generate_certs();
    let cancel = CancellationToken::new();
    let transport = TlsTransport::bind(
        "127.0.0.1:0".parse().unwrap(),
        certs.cert_file.path(),
        certs.key_file.path(),
    )
    .await
    .unwrap();
    assert_eq!(transport.kind(), TransportKind::Tls);
    let addr = transport.local_addr().unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    transport.spawn_reader(tx, cancel.clone());

    let mut client = tls_connect(addr, &certs).await;
    round_trip(&transport, &mut rx, &mut client).await;
    round_trip(&transport, &mut rx, &mut client).await;
    assert_eq!(transport.connections(), 1);

    // No connection to an arbitrary peer: TLS is inbound-only.
    let err = transport
        .send(Bytes::from_static(OK), "127.0.0.1:1".parse().unwrap())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotConnected);
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn tls_connection_cap_refuses_before_handshake() {
    let certs = generate_certs();
    let cancel = CancellationToken::new();
    let transport = TlsTransport::bind(
        "127.0.0.1:0".parse().unwrap(),
        certs.cert_file.path(),
        certs.key_file.path(),
    )
    .await
    .unwrap()
    .with_max_connections(1);
    let addr = transport.local_addr().unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    transport.spawn_reader(tx, cancel.clone());

    let mut first = tls_connect(addr, &certs).await;
    round_trip(&transport, &mut rx, &mut first).await;

    // The second client never completes a handshake: the raw socket
    // is closed by the cap check.
    let mut raw = TcpStream::connect(addr).await.unwrap();
    assert!(
        closed_by_server(&mut raw, Duration::from_secs(2)).await,
        "over-cap TLS connection must be closed before the handshake"
    );
    assert_eq!(transport.connections(), 1);
    round_trip(&transport, &mut rx, &mut first).await;
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn tls_idle_connection_is_closed_after_timeout() {
    let certs = generate_certs();
    let cancel = CancellationToken::new();
    let transport = TlsTransport::bind(
        "127.0.0.1:0".parse().unwrap(),
        certs.cert_file.path(),
        certs.key_file.path(),
    )
    .await
    .unwrap()
    .with_idle_timeout(Some(Duration::from_millis(300)));
    let addr = transport.local_addr().unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    transport.spawn_reader(tx, cancel.clone());

    let mut client = tls_connect(addr, &certs).await;
    round_trip(&transport, &mut rx, &mut client).await;
    assert!(
        closed_by_server(&mut client, Duration::from_secs(3)).await,
        "idle TLS connection must be closed"
    );
    wait_for_connections(|| transport.connections(), 0).await;
    cancel.cancel();
}
