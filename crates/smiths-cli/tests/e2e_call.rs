//! Binary-level call tests: spawn `smiths-net`, drive two test UACs
//! through a rendezvous with real RTP, hang up, and verify the
//! shutdown driver BYEs a call that is still up when SIGTERM lands.

#![cfg(unix)]

use std::net::TcpListener;
use std::path::Path;
use std::time::Duration;

use smiths_testkit::FakeUac;
use smiths_testkit::rtp::RtpPacket;
use tempfile::tempdir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::{Instant, timeout};

const RENDEZVOUS: &str = "e2e-room";
const FRAMES: usize = 25;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("reserve port")
        .local_addr()
        .expect("local_addr")
        .port()
}

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

fn write_config(path: &Path, health: u16, sip: u16, drain_secs: u64) {
    let toml = format!(
        "[observability]\n\
         log_format = \"json\"\n\
         log_level = \"info\"\n\
         health_bind = \"127.0.0.1:{health}\"\n\
         \n\
         [sip]\n\
         bind = [\"127.0.0.1:{sip}\"]\n\
         transports = [\"udp\"]\n\
         drain_timeout_secs = {drain_secs}\n"
    );
    std::fs::write(path, toml).expect("write config");
}

struct Engine {
    child: Child,
    sip_port: u16,
    _dir: tempfile::TempDir,
}

async fn spawn_engine(drain_secs: u64) -> Engine {
    let health_port = free_port();
    let sip_port = free_port();
    let dir = tempdir().expect("tempdir");
    let cfg = dir.path().join("config.toml");
    write_config(&cfg, health_port, sip_port, drain_secs);
    let child = Command::new(env!("CARGO_BIN_EXE_smiths-net"))
        .arg("--config")
        .arg(&cfg)
        .env("RUST_LOG", "info")
        .env_remove("SMITHS_DRAIN_SECS")
        .kill_on_drop(true)
        .spawn()
        .expect("spawn binary");
    assert!(
        wait_ready(health_port, Duration::from_secs(10)).await,
        "health endpoint never became ready on :{health_port}"
    );
    Engine {
        child,
        sip_port,
        _dir: dir,
    }
}

fn sigterm(child: &Child) {
    let pid = child.id().expect("child pid");
    let status = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .expect("spawn kill");
    assert!(status.success(), "`kill -TERM {pid}` failed: {status}");
}

#[tokio::test(flavor = "multi_thread")]
async fn two_uacs_rendezvous_rtp_flows_bye_and_clean_exit() {
    let mut engine = spawn_engine(0).await;
    let engine_addr = format!("127.0.0.1:{}", engine.sip_port).parse().unwrap();

    let mut ua_a = FakeUac::bind(engine_addr).await.unwrap();
    let mut ua_b = FakeUac::bind(engine_addr).await.unwrap();
    let (inv_a, inv_b) = tokio::join!(ua_a.invite(RENDEZVOUS), ua_b.invite(RENDEZVOUS));
    inv_a.expect("UA-A INVITE");
    inv_b.expect("UA-B INVITE");
    let rtp_target_a = ua_a.engine_rtp.expect("UA-A media answer");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // RTP A → engine → B.
    let collector = tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        let mut got = 0usize;
        let deadline = Instant::now() + Duration::from_secs(3);
        while got < FRAMES && Instant::now() < deadline {
            match timeout(Duration::from_millis(300), ua_b.rtp.recv_from(&mut buf)).await {
                Ok(Ok((n, _))) if RtpPacket::decode(&buf[..n]).is_some() => got += 1,
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
        (got, ua_b)
    });
    for i in 0..FRAMES {
        let pkt = RtpPacket {
            marker: i == 0,
            payload_type: 0,
            sequence: u16::try_from(i).unwrap(),
            timestamp: u32::try_from(i * 160).unwrap(),
            ssrc: 0xCAFE_F00D,
            payload: vec![0x7F; 160],
        };
        ua_a.rtp.send_to(&pkt.encode(), rtp_target_a).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (received, _ua_b) = collector.await.unwrap();
    assert!(
        received >= FRAMES - 2,
        "UA-B received {received} of {FRAMES} frames"
    );

    // BYE from A ends the call (B2BUA: engine BYEs B).
    ua_a.bye(RENDEZVOUS).await.expect("UA-A BYE");

    sigterm(&engine.child);
    let exit = timeout(Duration::from_secs(10), engine.child.wait())
        .await
        .expect("engine did not exit within 10s")
        .expect("wait child");
    assert!(exit.success(), "engine exited with {exit}");
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_with_active_dialog_sends_bye_within_drain_window() {
    // Drain window of 3 s: the engine must BYE the parked leg and
    // exit well before a hard-cancel would have.
    let mut engine = spawn_engine(3).await;
    let engine_addr = format!("127.0.0.1:{}", engine.sip_port).parse().unwrap();

    let mut ua = FakeUac::bind(engine_addr).await.unwrap();
    ua.invite(RENDEZVOUS).await.expect("INVITE answered");
    let call_id = ua.call_id.clone();

    let started = Instant::now();
    sigterm(&engine.child);

    // The drain step sends an in-dialog BYE to the UA's signaling
    // socket; answer it so the peer side looks well-behaved.
    let mut buf = vec![0u8; 4096];
    let bye = loop {
        let (n, from) = timeout(Duration::from_secs(5), ua.sip.recv_from(&mut buf))
            .await
            .expect("no BYE arrived during drain")
            .unwrap();
        let msg = String::from_utf8_lossy(&buf[..n]).into_owned();
        if msg.starts_with("BYE ") {
            let ok = "SIP/2.0 200 OK\r\nContent-Length: 0\r\n\r\n";
            let _ = ua.sip.send_to(ok.as_bytes(), from).await;
            break msg;
        }
    };
    assert!(
        bye.contains(&format!("Call-ID: {call_id}")),
        "BYE for a different dialog:\n{bye}"
    );
    assert!(bye.contains("CSeq: 1 BYE"), "{bye}");

    let exit = timeout(Duration::from_secs(10), engine.child.wait())
        .await
        .expect("engine did not exit after drain")
        .expect("wait child");
    assert!(exit.success(), "engine exited with {exit}");
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "shutdown took {:?}; hang-up should clear the table before the 3 s drain window",
        started.elapsed()
    );
}
