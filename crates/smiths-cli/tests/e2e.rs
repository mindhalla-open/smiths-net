//! Full-binary end-to-end smoke test.
//!
//! Spawns the release/debug `smiths-net` binary with a temp TOML config
//! on ephemeral ports, waits for `/health`, exercises the SIP UAS over
//! UDP, then sends `SIGTERM` and asserts a clean exit.
//!
//! Unix-only: uses `kill -TERM`. Windows needs a different signal path
//! (`GenerateConsoleCtrlEvent`) that we don't wire yet.

#![cfg(unix)]

use std::net::TcpListener;
use std::time::Duration;

use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::process::Command;
use tokio::time::{Instant, timeout};

const OPTIONS_REQ: &str = concat!(
    "OPTIONS sip:probe@127.0.0.1 SIP/2.0\r\n",
    "Via: SIP/2.0/UDP 127.0.0.1:0;branch=z9hG4bK-e2e-1;rport\r\n",
    "From: Probe <sip:probe@127.0.0.1>;tag=e2e\r\n",
    "To: Target <sip:alice@127.0.0.1>\r\n",
    "Call-ID: e2e-cid-1@127.0.0.1\r\n",
    "CSeq: 1 OPTIONS\r\n",
    "Max-Forwards: 70\r\n",
    "Content-Length: 0\r\n\r\n",
);

const SUBSCRIBE_REQ: &str = concat!(
    "SUBSCRIBE sip:probe@127.0.0.1 SIP/2.0\r\n",
    "Via: SIP/2.0/UDP 127.0.0.1:0;branch=z9hG4bK-e2e-2\r\n",
    "From: Probe <sip:probe@127.0.0.1>;tag=e2e\r\n",
    "To: Target <sip:alice@127.0.0.1>\r\n",
    "Call-ID: e2e-cid-2@127.0.0.1\r\n",
    "CSeq: 2 SUBSCRIBE\r\n",
    "Max-Forwards: 70\r\n",
    "Content-Length: 0\r\n\r\n",
);

/// Reserve a local port by briefly binding and dropping the listener.
/// There's a tiny race between the drop and the binary's bind — good
/// enough for a test suite.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("reserve port")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// One HTTP/1.1 round-trip against `/health`. `true` on `200 OK`.
async fn health_ok(port: u16) -> bool {
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)).await else {
        return false;
    };
    if s.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .is_err()
    {
        return false;
    }
    let mut buf = Vec::with_capacity(256);
    let _ = timeout(Duration::from_millis(500), s.read_to_end(&mut buf)).await;
    buf.starts_with(b"HTTP/1.1 200")
}

async fn wait_ready(port: u16, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if health_ok(port).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

async fn sip_roundtrip(sip_port: u16, req: &str) -> String {
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(req.as_bytes(), ("127.0.0.1", sip_port))
        .await
        .unwrap();
    let mut buf = vec![0u8; 4096];
    let (n, _) = timeout(Duration::from_secs(2), client.recv_from(&mut buf))
        .await
        .expect("timed out waiting for SIP reply")
        .unwrap();
    String::from_utf8(buf[..n].to_vec()).unwrap()
}

fn write_config(path: &std::path::Path, health: u16, sip: u16) {
    let toml = format!(
        "[observability]\n\
         log_format = \"json\"\n\
         log_level = \"info\"\n\
         health_bind = \"127.0.0.1:{health}\"\n\
         \n\
         [sip]\n\
         bind = [\"127.0.0.1:{sip}\"]\n\
         transports = [\"udp\"]\n\
         drain_timeout_secs = 2\n"
    );
    std::fs::write(path, toml).expect("write config");
}

#[tokio::test(flavor = "multi_thread")]
async fn binary_boots_serves_sip_and_shuts_down_cleanly() {
    let bin = env!("CARGO_BIN_EXE_smiths-net");

    let health_port = free_port();
    let sip_port = free_port();

    let dir = tempdir().expect("tempdir");
    let cfg = dir.path().join("config.toml");
    write_config(&cfg, health_port, sip_port);

    let mut child = Command::new(bin)
        .arg("--config")
        .arg(&cfg)
        .env("RUST_LOG", "info")
        .kill_on_drop(true)
        .spawn()
        .expect("spawn binary");
    let pid = child.id().expect("child pid");

    // 1. Health endpoint becomes ready within 5 s.
    assert!(
        wait_ready(health_port, Duration::from_secs(5)).await,
        "health endpoint never became ready on :{health_port}"
    );

    // 2. OPTIONS → 200 OK with correlation headers preserved and a
    //    server-generated To tag.
    let resp = sip_roundtrip(sip_port, OPTIONS_REQ).await;
    assert!(resp.starts_with("SIP/2.0 200 OK\r\n"), "got:\n{resp}");
    assert!(resp.contains("Call-ID: e2e-cid-1@127.0.0.1\r\n"));
    assert!(resp.contains("CSeq: 1 OPTIONS\r\n"));
    assert!(
        resp.contains(";tag=smiths-"),
        "To header missing server tag:\n{resp}"
    );

    // 3. Unknown method → 405.
    let resp = sip_roundtrip(sip_port, SUBSCRIBE_REQ).await;
    assert!(
        resp.starts_with("SIP/2.0 405 Method Not Allowed\r\n"),
        "got:\n{resp}"
    );

    // 4. Graceful shutdown via SIGTERM. Use /bin/kill so we don't pull
    //    in a signal crate for a two-line test.
    let kill = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .expect("spawn kill");
    assert!(kill.success(), "`kill -TERM {pid}` failed: {kill}");

    let exit = timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("engine did not exit within 5s")
        .expect("wait child");
    assert!(exit.success(), "engine exited with {exit}");
}
