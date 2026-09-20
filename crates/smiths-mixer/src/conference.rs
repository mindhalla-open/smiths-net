//! Conference state + mixer tick.
//!
//! A [`Conference`] holds N participants. Each participant is
//! addressed by a stable [`ParticipantId`] and owns:
//!
//! - an **ingress jitter buffer**: callers push PCM16 frames in with
//!   an RTP sequence number ([`Conference::push_frame_seq`]) or let
//!   the conference number them ([`Conference::push_frame`]); the
//!   buffer reorders, de-duplicates, conceals losses and — when a
//!   pusher's clock runs ahead of the mixer's — skips frames so the
//!   backlog never turns into growing latency;
//! - an **egress** channel (`tx_out`, `rx_out`): the mixer tick task
//!   pushes each participant's mixed output frame; the caller drains.
//!
//! The tick task runs at `frame_interval` cadence (typically
//! 20 ms) and is the single clock every participant's playout is
//! paced on. Each tick:
//!
//! 1. Pull exactly one frame from every participant's jitter buffer
//!    (silence while a buffer is still priming, concealment when the
//!    expected frame is missing).
//! 2. Sum them once and hand every participant the sum minus their
//!    own input, run through their AGC ([`Mixer`]).
//! 3. Observe each input with the [`Vad`]; update
//!    [`Self::dominant_speaker`].
//! 4. Push mixed outputs to every participant's egress queue.
//!
//! Per-tick scratch buffers live for the life of the tick task, so a
//! tick allocates only the egress frames it hands to the channels.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use smiths_media::jitter::{Insert, JitterBuffer, JitterConfig, JitterStats, Playout};
use thiserror::Error;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::agc::{Agc, AgcConfig};
use crate::metrics::{ConferenceLabel, IngressDropReason, MixerMetrics};
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
    /// Ingress jitter-buffer policy applied to every participant.
    pub jitter: JitterConfig,
}

/// Jitter-buffer policy the conference uses unless configured
/// otherwise: a 40 ms initial cushion that can narrow to one frame.
/// The mixer tick already adds a frame of latency, so the cushion is
/// shallower than a standalone playout buffer's.
#[must_use]
pub fn default_jitter_config() -> JitterConfig {
    JitterConfig {
        initial_target: 2,
        min_target: 1,
        ..JitterConfig::default()
    }
}

impl Default for ConferenceConfig {
    fn default() -> Self {
        Self {
            mixer: MixerConfig::default(),
            agc: AgcConfig::default(),
            frame_interval: Duration::from_millis(20),
            vad_threshold: VadScore::DEFAULT_SPEECH,
            jitter: default_jitter_config(),
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
    /// Ingress buffer refused the frame because it sits implausibly
    /// far ahead of the play head (more than `jitter.max_buffer`
    /// frames). Callers get this instead of blocking so slow mixers
    /// don't back-pressure the RTP receive loop.
    #[error("participant {0} ingress buffer full")]
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
    jitter: JitterBuffer,
    /// Sequence number [`Conference::push_frame`] stamps on the next
    /// frame from a caller that doesn't supply one.
    next_seq: u16,
    tx_out: mpsc::Sender<ParticipantFrame>,
}

/// N-participant audio mixer with per-participant channels.
pub struct Conference {
    id: ConferenceId,
    cfg: ConferenceConfig,
    state: Arc<Mutex<ConferenceState>>,
    cancel: CancellationToken,
    tick_handle: Mutex<Option<JoinHandle<()>>>,
    /// Prometheus metrics handle. `None` on tests that don't
    /// supply one; `Some` in every production deployment so
    /// operators see live mixer health.
    metrics: Option<Arc<MixerMetrics>>,
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
        Self::spawn_with_metrics(id, cfg, None)
    }

    /// Same as [`Self::spawn`] with an optional
    /// [`MixerMetrics`] handle — the engine wires one at boot;
    /// tests pass `None` to skip the Prometheus path.
    #[must_use]
    pub fn spawn_with_metrics(
        id: ConferenceId,
        cfg: ConferenceConfig,
        metrics: Option<Arc<MixerMetrics>>,
    ) -> Arc<Self> {
        let cancel = CancellationToken::new();
        let state = Arc::new(Mutex::new(ConferenceState {
            participants: BTreeMap::new(),
            next_participant: 0,
            ticks: 0,
            dominant: None,
        }));

        if let Some(m) = &metrics {
            m.conferences_active.inc();
        }
        let label = ConferenceLabel {
            conference: id.0.to_string(),
        };

        let tick_handle = tokio::spawn(run_tick(
            Arc::clone(&state),
            Mixer::new(cfg.mixer),
            cfg.frame_interval,
            cfg.vad_threshold,
            cancel.clone(),
            metrics.clone(),
            label,
        ));

        Arc::new(Self {
            id,
            cfg,
            state,
            cancel,
            tick_handle: Mutex::new(Some(tick_handle)),
            metrics,
        })
    }

    /// Conference id.
    #[must_use]
    pub fn id(&self) -> ConferenceId {
        self.id
    }

    /// Configured frame size (samples per tick).
    #[must_use]
    pub fn samples_per_frame(&self) -> usize {
        self.cfg.mixer.samples_per_frame
    }

    /// Configured tick cadence.
    #[must_use]
    pub fn frame_interval(&self) -> Duration {
        self.cfg.frame_interval
    }

    /// Add a participant. Returns the new `ParticipantId` + the
    /// egress `Receiver` the caller should drain to hear the mix.
    /// Ingress frames go through [`Self::push_frame`] /
    /// [`Self::push_frame_seq`].
    pub async fn join(&self) -> (ParticipantId, mpsc::Receiver<ParticipantFrame>) {
        let (tx_out, rx_out) = mpsc::channel(32);
        let mut state = self.state.lock().await;
        let id = ParticipantId(state.next_participant);
        state.next_participant += 1;
        state.participants.insert(
            id,
            Participant {
                agc: Agc::new(self.cfg.agc),
                vad: Box::new(EnergyVad::default()),
                jitter: JitterBuffer::with_config(
                    self.cfg.mixer.samples_per_frame,
                    self.cfg.jitter,
                ),
                next_seq: 0,
                tx_out,
            },
        );
        if let Some(m) = &self.metrics {
            m.participants_active.inc();
        }
        debug!(conf = %self.id, %id, "participant joined");
        (id, rx_out)
    }

    /// Push one PCM16 frame from a participant, numbering it as the
    /// next in that participant's own sequence. For callers that
    /// don't have RTP sequence numbers (audio injected by a plugin,
    /// tests). Returns [`ConferenceError::UnknownParticipant`] when
    /// `participant` has already left.
    ///
    /// # Errors
    /// [`ConferenceError`] — see variant docs.
    pub async fn push_frame(
        &self,
        participant: ParticipantId,
        samples: Vec<i16>,
    ) -> Result<(), ConferenceError> {
        self.push_inner(participant, None, samples).await
    }

    /// Push one PCM16 frame decoded from RTP packet `seq`. The jitter
    /// buffer uses the sequence number to reorder, de-duplicate and
    /// detect loss.
    ///
    /// # Errors
    /// [`ConferenceError`] — see variant docs.
    pub async fn push_frame_seq(
        &self,
        participant: ParticipantId,
        seq: u16,
        samples: Vec<i16>,
    ) -> Result<(), ConferenceError> {
        self.push_inner(participant, Some(seq), samples).await
    }

    async fn push_inner(
        &self,
        participant: ParticipantId,
        seq: Option<u16>,
        samples: Vec<i16>,
    ) -> Result<(), ConferenceError> {
        if samples.len() != self.cfg.mixer.samples_per_frame {
            self.count_drop("frame_size");
            return Err(ConferenceError::FrameSizeMismatch {
                expected: self.cfg.mixer.samples_per_frame,
                got: samples.len(),
            });
        }
        let mut state = self.state.lock().await;
        let p = state
            .participants
            .get_mut(&participant)
            .ok_or(ConferenceError::UnknownParticipant(participant))?;
        let seq = seq.unwrap_or_else(|| {
            let s = p.next_seq;
            p.next_seq = p.next_seq.wrapping_add(1);
            s
        });
        match p.jitter.insert(seq, samples) {
            Insert::Buffered => Ok(()),
            Insert::Duplicate => {
                self.count_drop("duplicate");
                Ok(())
            }
            Insert::Late => {
                self.count_drop("late");
                Ok(())
            }
            Insert::Overflow => {
                self.count_drop("overflow");
                Err(ConferenceError::IngressFull(participant))
            }
        }
    }

    fn count_drop(&self, reason: &'static str) {
        if let Some(m) = &self.metrics {
            m.ingress_dropped
                .get_or_create(&IngressDropReason {
                    reason: reason.into(),
                })
                .inc();
        }
    }

    /// Remove a participant; closes their egress channel and drops
    /// their ingress buffer.
    ///
    /// # Errors
    /// [`ConferenceError::UnknownParticipant`] when `participant` is
    /// not (or no longer) in the room.
    pub async fn leave(&self, participant: ParticipantId) -> Result<(), ConferenceError> {
        let mut state = self.state.lock().await;
        state
            .participants
            .remove(&participant)
            .ok_or(ConferenceError::UnknownParticipant(participant))?;
        if let Some(m) = &self.metrics {
            m.participants_active.dec();
        }
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

    /// Ingress jitter-buffer counters for one participant, or `None`
    /// if they aren't in the room.
    pub async fn participant_stats(&self, participant: ParticipantId) -> Option<JitterStats> {
        let state = self.state.lock().await;
        state
            .participants
            .get(&participant)
            .map(|p| p.jitter.stats())
    }

    /// Cancel the tick task and await its shutdown. Idempotent.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        let mut lock = self.tick_handle.lock().await;
        let was_live = lock.is_some();
        if let Some(h) = lock.take() {
            let _ = h.await;
        }
        if was_live && let Some(m) = &self.metrics {
            m.conferences_active.dec();
            // Best-effort: decrement participants_active by the
            // count still on the roster. Leaves not yet seen
            // stay in the gauge until their own leave — that
            // path zeros them out.
            let state = self.state.lock().await;
            let remaining = i64::try_from(state.participants.len()).unwrap_or(0);
            m.participants_active.dec_by(remaining);
        }
    }
}

/// Buffers the tick task reuses across frames.
#[derive(Default)]
struct TickScratch {
    /// All participants' inputs for this tick, `frame_len` each.
    inputs: Vec<i16>,
    /// One participant's mixed output.
    out: Vec<i16>,
    /// Participant order matching `inputs`.
    ids: Vec<ParticipantId>,
    /// VAD scores in the same order.
    scores: Vec<VadScore>,
}

async fn run_tick(
    state: Arc<Mutex<ConferenceState>>,
    mut mixer: Mixer,
    interval: Duration,
    vad_threshold: f32,
    cancel: CancellationToken,
    metrics: Option<Arc<MixerMetrics>>,
    label: ConferenceLabel,
) {
    let mut scratch = TickScratch::default();
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
                tick_once(&state, &mut mixer, &mut scratch, vad_threshold, metrics.as_deref(), &label).await;
            }
        }
    }
}

async fn tick_once(
    state: &Mutex<ConferenceState>,
    mixer: &mut Mixer,
    scratch: &mut TickScratch,
    vad_threshold: f32,
    metrics: Option<&MixerMetrics>,
    label: &ConferenceLabel,
) {
    let mut s = state.lock().await;
    s.ticks = s.ticks.wrapping_add(1);
    if let Some(m) = metrics {
        m.ticks.get_or_create(label).inc();
    }
    if s.participants.is_empty() {
        return;
    }

    let n = s.participants.len();
    let frame_len = mixer.samples_per_frame();
    let TickScratch {
        inputs,
        out,
        ids,
        scores,
    } = scratch;
    inputs.clear();
    inputs.resize(n * frame_len, 0);
    ids.clear();
    scores.clear();

    // Phase 1: one frame per participant out of its jitter buffer
    // (silence while priming, concealment on loss), summed once.
    mixer.begin_frame();
    let mut concealed = 0u64;
    for ((id, p), input) in s
        .participants
        .iter_mut()
        .zip(inputs.chunks_exact_mut(frame_len))
    {
        if p.jitter.tick_into(input) == Playout::Concealed {
            concealed += 1;
        }
        mixer.add_input(input);
        ids.push(*id);
    }
    if concealed > 0
        && let Some(m) = metrics
    {
        m.concealed.get_or_create(label).inc_by(concealed);
    }

    // Phase 2: each participant hears the sum minus themselves, run
    // through their own AGC in place; the VAD observes their INPUT
    // ("is this participant speaking", not "is someone speaking at
    // this participant").
    for ((id, p), input) in s
        .participants
        .iter_mut()
        .zip(inputs.chunks_exact(frame_len))
    {
        out.clear();
        out.resize(frame_len, 0);
        mixer.leave_one_out(input, out);
        p.agc.apply_inplace(out);
        scores.push(p.vad.observe(input));
        // Best-effort: drop the frame if the egress queue is full.
        // That's an operational "slow consumer" signal for metrics,
        // not a correctness issue. The clone is the one allocation
        // per participant per tick — the channel owns the frame.
        let _ = p.tx_out.try_send(ParticipantFrame {
            participant: *id,
            samples: out.clone(),
        });
    }

    let prior_dominant = s.dominant;
    s.dominant = dominant_speaker(scores, vad_threshold).map(|i| ids[i]);
    if prior_dominant != s.dominant
        && let Some(m) = metrics
    {
        m.dominant_switches.get_or_create(label).inc();
    }
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
            // One frame primes the buffer so a single push comes out on
            // the next tick.
            jitter: JitterConfig {
                initial_target: 1,
                min_target: 1,
                ..JitterConfig::default()
            },
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
    async fn default_cushion_holds_the_first_frame_until_primed() {
        // Default policy primes at two frames: after one push the
        // other participant hears silence, after the second push the
        // first frame comes through.
        let cfg = ConferenceConfig {
            jitter: default_jitter_config(),
            ..tiny_cfg()
        };
        let conf = Conference::spawn(ConferenceId(1), cfg);
        let (a_id, _a_out) = conf.join().await;
        let (_b_id, mut b_out) = conf.join().await;
        conf.push_frame(a_id, vec![7; 4]).await.unwrap();
        let first = tokio::time::timeout(Duration::from_millis(100), b_out.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.samples, vec![0; 4], "still priming: silence");
        conf.push_frame(a_id, vec![8; 4]).await.unwrap();
        let heard = tokio::time::timeout(Duration::from_millis(200), async {
            loop {
                let f = b_out.recv().await.unwrap();
                if f.samples != vec![0; 4] {
                    break f;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(heard.samples, vec![7; 4], "first frame plays once primed");
        conf.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn push_frame_seq_reorders_and_drops_duplicates() {
        let cfg = ConferenceConfig {
            // Deep enough cushion to hold the reordered frames.
            jitter: JitterConfig {
                initial_target: 3,
                min_target: 1,
                ..JitterConfig::default()
            },
            ..tiny_cfg()
        };
        let conf = Conference::spawn(ConferenceId(1), cfg);
        let (a_id, _a_out) = conf.join().await;
        let (_b_id, mut b_out) = conf.join().await;
        conf.push_frame_seq(a_id, 10, vec![1; 4]).await.unwrap();
        conf.push_frame_seq(a_id, 12, vec![3; 4]).await.unwrap();
        conf.push_frame_seq(a_id, 11, vec![2; 4]).await.unwrap();
        conf.push_frame_seq(a_id, 11, vec![9; 4]).await.unwrap(); // duplicate
        let mut heard = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
        while heard.len() < 3 && tokio::time::Instant::now() < deadline {
            if let Ok(Some(f)) = tokio::time::timeout(Duration::from_millis(50), b_out.recv()).await
                && f.samples != vec![0; 4]
            {
                heard.push(f.samples[0]);
            }
        }
        assert_eq!(
            heard,
            vec![1, 2, 3],
            "played in sequence order, dup dropped"
        );
        let js = conf.participant_stats(a_id).await.unwrap();
        assert_eq!(js.duplicates, 1);
        assert_eq!(js.played, 3);
        conf.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pusher_running_ahead_is_caught_up_not_queued() {
        let conf = Conference::spawn(ConferenceId(1), tiny_cfg());
        let (a_id, _a_out) = conf.join().await;
        let (_b_id, _b_out) = conf.join().await;
        // 40 frames in one burst against a 5 ms tick: far more than
        // target + slack. The buffer must skip ahead rather than
        // serve them all 5 ms apart (200 ms of latency).
        for i in 0..40i16 {
            conf.push_frame(a_id, vec![i; 4]).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
        let js = conf.participant_stats(a_id).await.unwrap();
        assert_eq!(js.inserted, 40);
        assert_eq!(js.overflow, 0, "a burst is not an overflow");
        assert!(js.catchup > 0, "catch-up must have skipped frames: {js:?}");
        assert!(js.played > 0, "and still played the survivors: {js:?}");
        assert!(js.played + js.catchup <= 40);
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
