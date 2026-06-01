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
mod rtp;
mod sip;
mod stun;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::net::UdpSocket;
use tokio::time::{MissedTickBehavior, interval};
use tracing::warn;

use audio::{AudioIo, pop_frame, push_samples};
use codec::{FRAME_MS, FRAME_SAMPLES, pcm16_to_pcmu, pcmu_to_pcm16};
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
        #[arg(long)]
        engine: SocketAddr,
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
        } => run_call(&audio, engine, &room, rtp_port, stun).await,
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
        loop {
            tick.tick().await;
            while let Some(frame) = pop_frame(&capture) {
                push_samples(&playback, &frame);
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
    engine: SocketAddr,
    room: &str,
    rtp_port: Option<u16>,
    stun_server: Option<String>,
) -> Result<()> {
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

    let send = tokio::spawn(send_loop(Arc::clone(&rtp_sock), peer_rtp, audio.capture()));
    let recv = tokio::spawn(recv_loop(Arc::clone(&rtp_sock), audio.playback()));

    tokio::signal::ctrl_c().await.context("wait for Ctrl-C")?;
    println!("Hanging up …");
    send.abort();
    recv.abort();
    if let Err(e) = uac.bye().await {
        warn!(?e, "BYE failed");
    }
    println!("Done.");
    Ok(())
}

/// Drain mic frames at 20 ms cadence, μ-law-encode, and send as RTP.
/// Sends a silence frame on underrun so the packet/timestamp clock
/// stays continuous and the engine bridge keeps flowing.
async fn send_loop(sock: Arc<UdpSocket>, peer: SocketAddr, capture: audio::Samples) {
    let mut tick = interval(Duration::from_millis(FRAME_MS));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let ssrc = rand::random::<u32>();
    let mut seq = rand::random::<u16>();
    let mut ts = rand::random::<u32>();
    let mut first = true;
    loop {
        tick.tick().await;
        let frame = pop_frame(&capture).unwrap_or_else(|| vec![0i16; FRAME_SAMPLES]);
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
