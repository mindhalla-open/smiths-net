//! Conference state + mixer tick.
//!
//! A [`Conference`] holds N participants. Each participant is
//! addressed by a stable [`ParticipantId`]; the conference owns
//! two MPSC channels per participant:
//!
//! - **ingress** (`tx_in`, `rx_in`): caller pushes PCM16 frames in;
//!   the mixer tick task drains the queue once per tick.
//! - **egress** (`tx_out`, `rx_out`): the mixer tick task pushes
//!   each participant's mixed output frame; the caller drains.
//!
//! The tick task runs at `frame_interval` cadence (typically
//! 20 ms). Each tick:
//!
//! 1. Pull at most one frame from every participant's ingress
//!    queue. Missing participants (no frame since last tick) count
//!    as silence for this tick.
//! 2. Mix with [`Mixer::mix`].
//! 3. Observe each frame with the [`Vad`]; update
//!    [`Self::dominant_speaker`].
//! 4. Push mixed outputs to every participant's egress queue.
//!
//! The channel-based transport deliberately sits between the
//! mixer and the network: tests drive frames in and out over the
//! channels (no UDP sockets), and the future UAS-side bridge
//! integration adds an RTP depayloader → `tx_in` at one end and
//! `rx_out` → RTP payloader at the other. This mirrors the
//! "primitives land here, wiring lands with the FSM refactor"
//! pattern from slices 5.3 and 5.4.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::agc::{Agc, AgcConfig};
use crate::mixer::{Mixer, MixerConfig};
use crate::vad::{EnergyVad, Vad, VadScore, dominant_speaker};

/// Opaque stable identifier for one conference.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ConferenceId(pub u64);

impl std::fmt::Display for ConferenceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "conf-{}", self.0)
    }
}

/// Opaque stable identifier for one participant within a
/// conference. Two participants across different conferences MAY
/// share the same id; pair with [`ConferenceId`] to disambiguate
/// across the process.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ParticipantId(pub u64);

impl std::fmt::Display for ParticipantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "p-{}", self.0)
    }
}

/// One PCM16 frame destined for or emitted by a participant.
#[derive(Clone, Debug)]
pub struct ParticipantFrame {
    /// Which participant this frame belongs to.
    pub participant: ParticipantId,
    /// PCM16 samples at the conference's configured sample rate.
    /// Length must equal `mixer_config.samples_per_frame`.
    pub samples: Vec<i16>,
}

/// Conference runtime config.
#[derive(Clone, Copy, Debug)]
pub struct ConferenceConfig {
    /// Mixer config (frame size).
    pub mixer: MixerConfig,
    /// AGC config applied per participant.
    pub agc: AgcConfig,
    /// Tick interval — must match the caller's RTP cadence. 20 ms
    /// is the RFC 3551 default for PCMU.
    pub frame_interval: Duration,
    /// VAD threshold for [`dominant_speaker`] selection.
    pub vad_threshold: f32,
}

impl Default for ConferenceConfig {
    fn default() -> Self {
        Self {
            mixer: MixerConfig::default(),
            agc: AgcConfig::default(),
            frame_interval: Duration::from_millis(20),
            vad_threshold: VadScore::DEFAULT_SPEECH,
        }
    }
}

/// Errors surfaced by [`Conference`] operations.
#[derive(Debug, Error)]
pub enum ConferenceError {
    /// Reference to a participant this conference doesn't know
    /// (never joined, or already left).
    #[error("unknown participant: {0}")]
    UnknownParticipant(ParticipantId),
    /// Caller pushed a frame whose length doesn't match the
    /// configured `samples_per_frame`.
    #[error("frame size mismatch: expected {expected}, got {got}")]
    FrameSizeMismatch {
        /// Configured frame size.
        expected: usize,
        /// Frame length the caller submitted.
        got: usize,
    },
    /// Ingress channel full — the tick task is running behind.
    /// Callers get this instead of blocking so slow mixers don't
    /// back-pressure the RTP receive loop.
    #[error("participant {0} ingress channel full")]
    IngressFull(ParticipantId),
}

/// Snapshot counters for one conference.
#[derive(Clone, Debug, Default)]
pub struct ConferenceStats {
    /// Number of live participants.
    pub participants: usize,
    /// Cumulative ticks since the conference started.
    pub ticks: u64,
    /// Last-tick dominant speaker, or `None` if nobody is speaking.
    pub dominant: Option<ParticipantId>,
}

struct Participant {
    agc: Agc,
    vad: Box<dyn Vad + Send>,
    rx_in: mpsc::Receiver<Vec<i16>>,
    tx_in: mpsc::Sender<Vec<i16>>,
    tx_out: mpsc::Sender<ParticipantFrame>,
}

/// N-participant audio mixer with per-participant channels.
pub struct Conference {
    id: ConferenceId,
    cfg: ConferenceConfig,
    state: Arc<Mutex<ConferenceState>>,
    cancel: CancellationToken,
    tick_handle: Mutex<Option<JoinHandle<()>>>,
}

struct ConferenceState {
    participants: BTreeMap<ParticipantId, Participant>,
    next_participant: u64,
    ticks: u64,
    dominant: Option<ParticipantId>,
}

impl Conference {
    /// Build and start a conference. The tick task runs until
    /// [`Self::shutdown`].
    #[must_use]
    pub fn spawn(id: ConferenceId, cfg: ConferenceConfig) -> Arc<Self> {
        let cancel = CancellationToken::new();
        let state = Arc::new(Mutex::new(ConferenceState {
            participants: BTreeMap::new(),
            next_participant: 0,
            ticks: 0,
            dominant: None,
        }));
        let mixer = Mixer::new(cfg.mixer);

        let tick_handle = tokio::spawn(run_tick(
            Arc::clone(&state),
            mixer,
            cfg.frame_interval,
            cfg.vad_threshold,
            cancel.clone(),
        ));

        Arc::new(Self {
            id,
            cfg,
            state,
            cancel,
            tick_handle: Mutex::new(Some(tick_handle)),
        })
    }

    /// Conference id.
    #[must_use]
    pub fn id(&self) -> ConferenceId {
        self.id
    }

    /// Configured frame size (samples per 20 ms tick).
    #[must_use]
    pub fn samples_per_frame(&self) -> usize {
        self.cfg.mixer.samples_per_frame
    }

    /// Add a participant. Returns the new `ParticipantId` + the
    /// egress `Receiver` the caller should drain to hear the mix.
    /// Ingress frames go through [`Self::push_frame`].
    pub async fn join(&self) -> (ParticipantId, mpsc::Receiver<ParticipantFrame>) {
        let (tx_in, rx_in) = mpsc::channel(32);
        let (tx_out, rx_out) = mpsc::channel(32);
        let mut state = self.state.lock().await;
        let id = ParticipantId(state.next_participant);
        state.next_participant += 1;
        state.participants.insert(
            id,
            Participant {
                agc: Agc::new(self.cfg.agc),
                vad: Box::new(EnergyVad::default()),
                rx_in,
                tx_in,
                tx_out,
            },
        );
        debug!(conf = %self.id, %id, "participant joined");
        (id, rx_out)
    }

    /// Push one PCM16 frame from a participant into the ingress
    /// queue. Returns [`ConferenceError::UnknownParticipant`] when
    /// `participant` has already left.
    ///
    /// # Errors
    /// [`ConferenceError`] — see variant docs.
    pub async fn push_frame(
        &self,
        participant: ParticipantId,
        samples: Vec<i16>,
    ) -> Result<(), ConferenceError> {
        if samples.len() != self.cfg.mixer.samples_per_frame {
            return Err(ConferenceError::FrameSizeMismatch {
                expected: self.cfg.mixer.samples_per_frame,
                got: samples.len(),
            });
        }
        let state = self.state.lock().await;
        let p = state
            .participants
            .get(&participant)
            .ok_or(ConferenceError::UnknownParticipant(participant))?;
        p.tx_in.try_send(samples).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => ConferenceError::IngressFull(participant),
            mpsc::error::TrySendError::Closed(_) => {
                ConferenceError::UnknownParticipant(participant)
            }
        })
    }

    /// Remove a participant; closes both their channels.
    pub async fn leave(&self, participant: ParticipantId) -> Result<(), ConferenceError> {
        let mut state = self.state.lock().await;
        state
            .participants
            .remove(&participant)
            .ok_or(ConferenceError::UnknownParticipant(participant))?;
        debug!(conf = %self.id, %participant, "participant left");
        Ok(())
    }

    /// Snapshot stats (live count, tick count, dominant speaker).
    pub async fn stats(&self) -> ConferenceStats {
        let state = self.state.lock().await;
        ConferenceStats {
            participants: state.participants.len(),
            ticks: state.ticks,
            dominant: state.dominant,
        }
    }

    /// Cancel the tick task and await its shutdown. Idempotent.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        let mut lock = self.tick_handle.lock().await;
        if let Some(h) = lock.take() {
            let _ = h.await;
        }
    }
}

async fn run_tick(
    state: Arc<Mutex<ConferenceState>>,
    mixer: Mixer,
    interval: Duration,
    vad_threshold: f32,
    cancel: CancellationToken,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                debug!("conference tick cancelled");
                return;
            }
            _ = ticker.tick() => {
                tick_once(&state, &mixer, vad_threshold).await;
            }
        }
    }
}

async fn tick_once(state: &Mutex<ConferenceState>, mixer: &Mixer, vad_threshold: f32) {
    let mut s = state.lock().await;
    if s.participants.is_empty() {
        s.ticks = s.ticks.wrapping_add(1);
        return;
    }

    // Pull one frame per participant (or silence on underflow).
    let n = s.participants.len();
    let frame_len = mixer.samples_per_frame();
    let silence: Vec<i16> = vec![0; frame_len];
    let mut inputs: Vec<Vec<i16>> = Vec::with_capacity(n);
    let ids: Vec<ParticipantId> = s.participants.keys().copied().collect();
    for id in &ids {
        let Some(p) = s.participants.get_mut(id) else {
            inputs.push(silence.clone());
            continue;
        };
        let frame = p.rx_in.try_recv().unwrap_or_else(|_| silence.clone());
        inputs.push(frame);
    }

    // Mix.
    let mut outputs: Vec<Vec<i16>> = (0..n).map(|_| vec![0_i16; frame_len]).collect();
    let mut agcs: Vec<Agc> = ids
        .iter()
        .filter_map(|id| s.participants.get(id).map(|p| p.agc.clone()))
        .collect();
    // If a participant raced out between `ids` and here, `agcs` is
    // shorter than `ids` / `inputs` — pad with defaults so
    // `Mixer::mix`'s length invariants hold.
    while agcs.len() < ids.len() {
        agcs.push(Agc::new(AgcConfig::default()));
    }
    {
        let input_refs: Vec<&[i16]> = inputs.iter().map(Vec::as_slice).collect();
        let mut out_refs: Vec<&mut [i16]> = outputs.iter_mut().map(Vec::as_mut_slice).collect();
        mixer.mix(&input_refs, &mut agcs, &mut out_refs);
    }
    // Write back AGC state.
    for (id, agc) in ids.iter().zip(agcs) {
        if let Some(p) = s.participants.get_mut(id) {
            p.agc = agc;
        }
    }

    // VAD: observe each participant's INPUT (not output — we want
    // "is this participant speaking", not "is someone speaking at
    // this participant").
    let mut scores: Vec<VadScore> = Vec::with_capacity(n);
    for (id, input) in ids.iter().zip(inputs.iter()) {
        let Some(p) = s.participants.get_mut(id) else {
            scores.push(VadScore::default());
            continue;
        };
        scores.push(p.vad.observe(input));
    }
    s.dominant = dominant_speaker(&scores, vad_threshold).map(|i| ids[i]);

    // Publish outputs.
    for (id, out) in ids.iter().zip(outputs) {
        if let Some(p) = s.participants.get_mut(id) {
            // Best-effort: drop the frame if the egress queue is
            // full. That's an operational "slow consumer" signal
            // for metrics, not a correctness issue.
            let _ = p.tx_out.try_send(ParticipantFrame {
                participant: *id,
                samples: out,
            });
        }
    }

    s.ticks = s.ticks.wrapping_add(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_cfg() -> ConferenceConfig {
        ConferenceConfig {
            mixer: MixerConfig {
                samples_per_frame: 4,
            },
            agc: AgcConfig {
                target_rms: u32::MAX,
                attack: 1.0,
                release: 1.0,
                max_gain: 1.0,
            },
            frame_interval: Duration::from_millis(5),
            vad_threshold: VadScore::DEFAULT_SPEECH,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_participants_hear_each_other() {
        let conf = Conference::spawn(ConferenceId(1), tiny_cfg());
        let (a_id, mut a_out) = conf.join().await;
        let (b_id, mut b_out) = conf.join().await;

        conf.push_frame(a_id, vec![100, 200, 300, 400])
            .await
            .unwrap();
        conf.push_frame(b_id, vec![1, 2, 3, 4]).await.unwrap();

        let a_frame = tokio::time::timeout(Duration::from_millis(100), a_out.recv())
            .await
            .unwrap()
            .unwrap();
        let b_frame = tokio::time::timeout(Duration::from_millis(100), b_out.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(a_frame.samples, vec![1, 2, 3, 4]);
        assert_eq!(b_frame.samples, vec![100, 200, 300, 400]);

        conf.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn leave_unknown_errors() {
        let conf = Conference::spawn(ConferenceId(1), tiny_cfg());
        let err = conf.leave(ParticipantId(999)).await.unwrap_err();
        assert!(matches!(err, ConferenceError::UnknownParticipant(_)));
        conf.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn frame_size_mismatch_errors() {
        let conf = Conference::spawn(ConferenceId(1), tiny_cfg());
        let (a_id, _a_out) = conf.join().await;
        let err = conf.push_frame(a_id, vec![0, 0]).await.unwrap_err(); // 2 ≠ 4
        assert!(matches!(
            err,
            ConferenceError::FrameSizeMismatch {
                expected: 4,
                got: 2
            }
        ));
        conf.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stats_track_participant_count_and_ticks() {
        let cfg = ConferenceConfig {
            frame_interval: Duration::from_millis(10),
            ..tiny_cfg()
        };
        let conf = Conference::spawn(ConferenceId(2), cfg);
        let (_a, _rxa) = conf.join().await;
        let (_b, _rxb) = conf.join().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stats = conf.stats().await;
        assert_eq!(stats.participants, 2);
        assert!(stats.ticks > 0, "tick loop did not run");
        conf.shutdown().await;
    }
}
