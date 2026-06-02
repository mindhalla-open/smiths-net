//! `smiths-softphone` — a live-audio SIP client for smiths-net.
//!
//! Captures your microphone, G.711-encodes it into RTP, and plays the
//! peer's RTP back through your speakers. The "server" is whoever runs
//! the engine (`smiths-net --config …`); everyone else runs this and
//! calls the same room. Two callers on one room are bridged by the
//! engine's UAS, so you talk to each other.
//!
//! ```text
//! # validate mic → speaker locally (no network)
//! smiths-softphone loopback
//!
//! # place a call into room "demo" on a local engine
//! smiths-softphone call --engine 127.0.0.1:5060 --room demo
//! ```

// This is a user-facing CLI: status lines go to stdout by design, so
// `print_stdout` is expected here. The one numeric cast (`FRAME_SAMPLES`
// = 160 → `u32` RTP timestamp step) and the `send`/`recv` task-handle
// names are deliberate and bounded.
#![allow(
    clippy::print_stdout,
    clippy::cast_possible_truncation,
    clippy::similar_names
)]

mod audio;
mod codec;
mod effect;
mod rtp;
mod sip;
mod stun;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UdpSocket;
use tokio::process::{Child, Command};
use tokio::time::{MissedTickBehavior, interval};
use tracing::warn;

use audio::{AudioIo, pop_frame, push_samples};
use codec::{FRAME_MS, FRAME_SAMPLES, pcm16_to_pcmu, pcmu_to_pcm16};
use effect::{Voice, VoiceChanger};
use rtp::{PT_PCMU, RtpPacket};
use sip::{SipUac, detect_local_ip};

#[derive(Parser)]
#[command(
    name = "smiths-softphone",
    about = "Live-audio SIP client for smiths-net"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Smoke test: pipe the microphone straight to the speakers, no
    /// network. Proves the capture → resample → playback chain.
    Loopback,
    /// Place a call into `room` on `engine` and run full-duplex audio
    /// until Ctrl-C.
    Call {
        /// Engine SIP address, e.g. `127.0.0.1:5060` or `192.168.1.10:5060`.
        /// Optional when `--host` is set (defaults to the local engine).
        #[arg(long)]
        engine: Option<SocketAddr>,
        /// Room / user-part to dial. Two callers on the same room are
        /// bridged together by the engine.
        #[arg(long, default_value = "demo")]
        room: String,
        /// Local UDP port to bind for RTP. Omit for an OS-assigned
        /// ephemeral port; set a fixed value when you need a known port
        /// to open in a firewall.
        #[arg(long)]
        rtp_port: Option<u16>,
        /// STUN server (`host:port`) to discover this machine's public
        /// address through a NAT, e.g. `stun.l.google.com:19302`. When
        /// set, the discovered public address is advertised in the SDP
        /// so the engine's return RTP can traverse a cone NAT.
        #[arg(long)]
        stun: Option<String>,
        /// Voice effect applied to your outgoing audio. Switch it live
        /// during the call by typing a name (none/deep/high/chipmunk/
        /// robot) and pressing Enter.
        #[arg(long, value_enum, default_value = "none")]
        voice: Voice,
        /// Also start a local engine (server) that others can connect
        /// to, then join it yourself. Spawns `smiths-net`, waits until
        /// it's listening on `0.0.0.0:5060`, and shuts it down on exit.
        #[arg(long)]
        host: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "smiths_softphone=info".into()),
        )
        .init();

    let cli = Cli::parse();

    // cpal streams are not Send — `AudioIo` must live on this thread
    // for the whole session; only the queue handles cross into tasks.
    let audio = AudioIo::start().context("open audio devices")?;

    match cli.cmd {
        Cmd::Loopback => run_loopback(&audio).await,
        Cmd::Call {
            engine,
            room,
            rtp_port,
            stun,
            voice,
            host,
        } => run_call(&audio, engine, &room, rtp_port, stun, voice, host).await,
    }
}

/// Mic → speaker echo. Headphones recommended (no echo cancellation).
async fn run_loopback(audio: &AudioIo) -> Result<()> {
    println!("Loopback: speak — you should hear yourself. Ctrl-C to stop.");
    let capture = audio.capture();
    let playback = audio.playback();

    let pump = tokio::spawn(async move {
        let mut tick = interval(Duration::from_millis(FRAME_MS));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Per-second throughput so we can tell mic-vs-speaker problems
        // apart: samples captured from the mic, and peak |amplitude|.
        let mut ticks: u32 = 0;
        let mut captured: u64 = 0;
        let mut peak: i16 = 0;
        let mut quiet_secs: u32 = 0;
        loop {
            tick.tick().await;
            while let Some(frame) = pop_frame(&capture) {
                captured += frame.len() as u64;
                for &s in &frame {
                    peak = peak.max(s.saturating_abs());
                }
                push_samples(&playback, &frame);
            }
            ticks += 1;
            if ticks >= 50 {
                // ~1 s elapsed.
                if captured == 0 {
                    quiet_secs += 1;
                    warn!(
                        "no microphone samples in the last second \
                         (grant your terminal Microphone access in System \
                         Settings → Privacy & Security → Microphone, then retry)"
                    );
                    if quiet_secs == 1 {
                        println!(
                            "⚠ no mic input detected — check Microphone permission for your terminal."
                        );
                    }
                } else {
                    quiet_secs = 0;
                    println!("mic: {captured} samples/s, peak amplitude {peak}/32767");
                }
                ticks = 0;
                captured = 0;
                peak = 0;
            }
        }
    });

    tokio::signal::ctrl_c().await.context("wait for Ctrl-C")?;
    pump.abort();
    println!("Stopped.");
    Ok(())
}

/// Place the call and run the bidirectional RTP loop.
async fn run_call(
    audio: &AudioIo,
    engine: Option<SocketAddr>,
    room: &str,
    rtp_port: Option<u16>,
    stun_server: Option<String>,
    voice: Voice,
    host: bool,
) -> Result<()> {
    // `--host`: bring up a local engine others can connect to, then
    // join it ourselves. The child is killed when this function returns
    // (`kill_on_drop`). `_engine` must stay in scope for the call.
    let _engine = if host {
        let child = spawn_local_engine().await?;
        println!("Local engine is up on 0.0.0.0:5060.");
        if let Ok(lan) = detect_local_ip("8.8.8.8:80".parse().expect("literal addr")) {
            println!("Others on your network can join with:");
            println!("  smiths-softphone call --engine {lan}:5060 --room {room}");
        }
        Some(child)
    } else {
        None
    };

    let engine = if host {
        SocketAddr::from(([127, 0, 0, 1], 5060))
    } else {
        engine.ok_or_else(|| {
            anyhow::anyhow!("provide --engine <ip:port>, or use --host to run a local engine")
        })?
    };

    let local_ip = detect_local_ip(engine)?;
    // Bind our RTP socket first so we can advertise its port in the
    // SDP offer. `rtp_port` pins it for firewalling; 0 = ephemeral.
    let rtp_sock = Arc::new(
        UdpSocket::bind((local_ip, rtp_port.unwrap_or(0)))
            .await
            .context("bind RTP socket")?,
    );
    let rtp_port = rtp_sock.local_addr().context("RTP local_addr")?.port();

    // Optional STUN: discover our public address through a NAT, using
    // the RTP socket itself so the mapping matches the media flow.
    let advertised = match stun_server {
        Some(server) => {
            let server_addr = tokio::net::lookup_host(&server)
                .await
                .context("resolve STUN server")?
                .next()
                .ok_or_else(|| anyhow::anyhow!("STUN server {server} resolved to no addresses"))?;
            match stun::discover(&rtp_sock, server_addr).await {
                Ok(public) => {
                    println!("STUN: public RTP address is {public} (advertised to engine)");
                    Some(public)
                }
                Err(e) => {
                    warn!(?e, "STUN discovery failed; advertising local address");
                    None
                }
            }
        }
        None => None,
    };

    let mut uac = SipUac::connect(engine, local_ip, room, rtp_port, advertised).await?;
    println!("Calling room {room:?} on {engine} …");
    let peer_rtp = uac.invite().await.context("INVITE failed")?;
    println!("Connected. Media flows to {peer_rtp}. Talk! Ctrl-C to hang up.");

    // Shared voice changer applied to outgoing frames. Switch live by
    // typing a voice name.
    let changer = Arc::new(Mutex::new(VoiceChanger::new(voice)));
    println!("Voice: {voice:?}. Type none/deep/high/chipmunk/robot + Enter to switch.");

    let send = tokio::spawn(send_loop(
        Arc::clone(&rtp_sock),
        peer_rtp,
        audio.capture(),
        Arc::clone(&changer),
    ));
    let recv = tokio::spawn(recv_loop(Arc::clone(&rtp_sock), audio.playback()));
    let switch = tokio::spawn(voice_switch_loop(Arc::clone(&changer)));

    tokio::signal::ctrl_c().await.context("wait for Ctrl-C")?;
    println!("Hanging up …");
    send.abort();
    recv.abort();
    switch.abort();
    if let Err(e) = uac.bye().await {
        warn!(?e, "BYE failed");
    }
    println!("Done.");
    Ok(())
}

/// Locate the `smiths-net` engine binary: next to this executable, in
/// the sibling `release`/`debug` target dirs, else on `PATH`.
fn engine_binary() -> PathBuf {
    let name = if cfg!(windows) {
        "smiths-net.exe"
    } else {
        "smiths-net"
    };
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let mut candidates = vec![dir.join(name)];
        if let Some(target) = dir.parent() {
            candidates.push(target.join("release").join(name));
            candidates.push(target.join("debug").join(name));
        }
        for c in candidates {
            if c.exists() {
                return c;
            }
        }
    }
    PathBuf::from(name) // fall back to PATH
}

/// Write a minimal engine config to a temp file: bind SIP on all
/// interfaces, enable `conf*` conference rooms, and point the plugin
/// dir at a nonexistent path so no example plugins load.
fn write_temp_engine_config() -> Result<PathBuf> {
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let noplugins = dir.join(format!("smiths-host-noplugins-{pid}"));
    let cfg = dir.join(format!("smiths-host-{pid}.toml"));
    let body = format!(
        "[sip]\n\
         bind = [\"0.0.0.0:5060\"]\n\
         conference_prefix = \"conf\"\n\n\
         [plugins]\n\
         dir = \"{}\"\n",
        noplugins.display()
    );
    std::fs::write(&cfg, body).context("write temp engine config")?;
    Ok(cfg)
}

/// Spawn `smiths-net` as a child and wait until it logs that it's
/// listening. The returned `Child` kills the engine when dropped.
async fn spawn_local_engine() -> Result<Child> {
    let bin = engine_binary();
    let cfg = write_temp_engine_config()?;
    println!("Starting local engine ({}) …", bin.display());
    let mut child = Command::new(&bin)
        .arg("--config")
        .arg(&cfg)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| {
            format!(
                "could not start engine `{}` — build it with \
                 `cargo build --release -p smiths-cli --bin smiths-net` or install smiths-net",
                bin.display()
            )
        })?;

    // Drain stdout, signalling readiness when the engine reports it.
    let stdout = child.stdout.take().context("engine stdout pipe")?;
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        let mut tx = Some(tx);
        while let Ok(Some(line)) = lines.next_line().await {
            if line.contains("smiths-net ready")
                && let Some(tx) = tx.take()
            {
                let _ = tx.send(());
            }
            // Keep reading to the end so the pipe never fills.
        }
    });

    if let Ok(Ok(())) = tokio::time::timeout(Duration::from_secs(20), rx).await {
        Ok(child)
    } else {
        let _ = child.kill().await;
        anyhow::bail!(
            "local engine did not become ready within 20s \
             (is UDP 5060 already in use? try running smiths-net manually)"
        )
    }
}

/// Read voice-effect names from stdin and apply them live. Unknown
/// names are reported; EOF ends the loop (the call keeps running).
async fn voice_switch_loop(changer: Arc<Mutex<VoiceChanger>>) {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        match Voice::from_name(&line) {
            Some(v) => {
                changer.lock().expect("voice changer mutex").set(v);
                println!("Voice → {v:?}");
            }
            None => {
                println!("Unknown voice {line:?} (try none/deep/high/chipmunk/robot)");
            }
        }
    }
}

/// Drain mic frames at 20 ms cadence, μ-law-encode, and send as RTP.
/// Sends a silence frame on underrun so the packet/timestamp clock
/// stays continuous and the engine bridge keeps flowing.
async fn send_loop(
    sock: Arc<UdpSocket>,
    peer: SocketAddr,
    capture: audio::Samples,
    changer: Arc<Mutex<VoiceChanger>>,
) {
    let mut tick = interval(Duration::from_millis(FRAME_MS));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let ssrc = rand::random::<u32>();
    let mut seq = rand::random::<u16>();
    let mut ts = rand::random::<u32>();
    let mut first = true;
    loop {
        tick.tick().await;
        let mut frame = pop_frame(&capture).unwrap_or_else(|| vec![0i16; FRAME_SAMPLES]);
        // Apply the live voice effect before encoding. Lock is held
        // only for the synchronous transform — never across an await.
        changer
            .lock()
            .expect("voice changer mutex")
            .process(&mut frame);
        let payload = pcm16_to_pcmu(&frame);
        let pkt = RtpPacket {
            marker: first,
            payload_type: PT_PCMU,
            sequence: seq,
            timestamp: ts,
            ssrc,
            payload,
        };
        if let Err(e) = sock.send_to(&pkt.encode(), peer).await {
            warn!(?e, "RTP send failed");
        }
        first = false;
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(FRAME_SAMPLES as u32);
    }
}

/// Receive RTP, μ-law-decode PCMU payloads, and queue them for the
/// speaker. Non-PCMU / malformed packets are dropped.
async fn recv_loop(sock: Arc<UdpSocket>, playback: audio::Samples) {
    let mut buf = vec![0u8; 2048];
    loop {
        let n = match sock.recv_from(&mut buf).await {
            Ok((n, _)) => n,
            Err(e) => {
                warn!(?e, "RTP recv failed");
                continue;
            }
        };
        let Some(pkt) = RtpPacket::decode(&buf[..n]) else {
            continue;
        };
        if pkt.payload_type != PT_PCMU {
            continue;
        }
        let pcm = pcmu_to_pcm16(&pkt.payload);
        push_samples(&playback, &pcm);
    }
}
