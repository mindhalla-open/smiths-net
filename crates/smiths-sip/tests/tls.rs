//! TLS SIP transport integration: bind `TlsTransport` with an `rcgen`
//! self-signed cert, connect with a `rustls` client, do an
//! OPTIONS → 200 OK round-trip.

use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, ServerName};
use smiths_core::{EventBus, MediaFabric, SdpNegotiator};
use smiths_media::UdpMediaFabric;
use smiths_sdp::Negotiator;
use smiths_sip::TlsTransport;
use smiths_sip::{Transport as _, UasServer};
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_util::sync::CancellationToken;

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
    let cert_pem = cert.cert.pem();
    let key_pem = cert.signing_key.serialize_pem();
    let mut cert_file = NamedTempFile::new().unwrap();
    cert_file.write_all(cert_pem.as_bytes()).unwrap();
    let mut key_file = NamedTempFile::new().unwrap();
    key_file.write_all(key_pem.as_bytes()).unwrap();
    TestCerts {
        cert_der: cert.cert.der().clone(),
        cert_file,
        key_file,
    }
}

async fn spawn_tls_uas(cert: &std::path::Path, key: &std::path::Path) -> SocketAddr {
    let t = TlsTransport::bind("127.0.0.1:0".parse().unwrap(), cert, key)
        .await
        .unwrap();
    let local = t.local_addr().unwrap();
    let transport = Arc::new(t);
    let bus = EventBus::new(16);
    let cancel = CancellationToken::new();
    let (tx, rx) = mpsc::channel(64);
    transport.spawn_reader(tx, cancel.clone());
    let fabric: Arc<dyn MediaFabric> = Arc::new(UdpMediaFabric::new());
    let negotiator: Arc<dyn SdpNegotiator> = Arc::new(Negotiator::with_default_codecs(local.ip()));
    let server = UasServer::new(Arc::clone(&transport), bus, fabric, negotiator).unwrap();
    tokio::spawn(server.run(rx, cancel));
    local
}

#[tokio::test(flavor = "multi_thread")]
async fn tls_options_returns_200_ok() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let certs = generate_certs();
    let uas = spawn_tls_uas(certs.cert_file.path(), certs.key_file.path()).await;

    let client_cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCert(certs.cert_der.clone())))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_cfg));
    let tcp = TcpStream::connect(uas).await.unwrap();
    let domain = ServerName::try_from("localhost").unwrap();
    let mut tls = connector.connect(domain, tcp).await.unwrap();

    let req = format!(
        concat!(
            "OPTIONS sip:smiths@{uas} SIP/2.0\r\n",
            "Via: SIP/2.0/TLS test;branch=z9hG4bK-tls-1\r\n",
            "From: Tester <sip:tester@test>;tag=tls1\r\n",
            "To: <sip:smiths@{uas}>\r\n",
            "Call-ID: tls-opts@test\r\n",
            "CSeq: 1 OPTIONS\r\n",
            "Max-Forwards: 70\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        uas = uas,
    );
    tls.write_all(req.as_bytes()).await.unwrap();

    let mut buf = vec![0u8; 4096];
    let n = timeout(Duration::from_secs(3), tls.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    let resp = std::str::from_utf8(&buf[..n]).unwrap();
    assert!(
        resp.starts_with("SIP/2.0 200 OK\r\n"),
        "expected 200 OK over TLS, got:\n{resp}"
    );
}
