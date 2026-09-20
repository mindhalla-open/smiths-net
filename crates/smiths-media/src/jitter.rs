//! Adaptive playout jitter buffer for received RTP audio frames.
//!
//! Every path that decodes audio and re-paces it on its own clock —
//! the transcoded session, a conference participant, the softphone's
//! speaker — receives one 20 ms frame per RTP packet from a network
//! that delivers packets reordered, duplicated, late, or not at all,
//! from a sender whose media clock never ticks in perfect lockstep
//! with the local one. Playing packets in arrival order plays
//! reorders out of order, plays duplicates twice, and closes loss gaps
//! by pulling later audio forward. All three are audible.
//!
//! This buffer decouples *arrival* from *playout*:
//! - [`JitterBuffer::insert`] places each decoded frame at its absolute
//!   sequence index, dropping duplicates and packets that arrive after
//!   their playout slot has already passed.
//! - [`JitterBuffer::tick`], called once per frame interval, releases
//!   the next in-order frame — or a concealment frame when the
//!   expected one is missing (packet-loss concealment: the last good
//!   frame, faded toward silence over a run of losses).
//! - The playout depth (`target`) is primed before audio starts and
//!   adapts: sustained starvation widens the cushion and re-buffers so
//!   audio resumes on a clean in-order run; a long calm stretch narrows
//!   it back toward the floor to give the earned latency back; a
//!   buffer that keeps growing (the sender's clock runs fast, or the
//!   local clock stalled) is caught up by skipping the oldest frames
//!   so latency stays bounded instead of turning into a permanent
//!   delay.
//!
//! It is deliberately I/O-free — no sockets, no codec, no wall clock —
//! so it unit-tests deterministically. The caller drives
//! [`JitterBuffer::tick`] from its own frame clock; the buffer never
//! reads the clock itself.
//!
//! It keys purely on the RTP **sequence number**, assuming one frame
//! per packet (true for G.711 and 20 ms Opus). Timestamp-carried
//! silence gaps are not reconstructed.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

/// Tunables for a [`JitterBuffer`]. All depths are in frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitterConfig {
    /// Cushion primed before the first frame is released.
    pub initial_target: usize,
    /// Floor the adaptive target never narrows below.
    pub min_target: usize,
    /// Ceiling the adaptive target never widens past.
    pub max_target: usize,
    /// Hard cap on how far ahead of the play head a frame may sit.
    /// Anything further is a bogus sequence jump, not reordering —
    /// dropped so a garbage packet can't grow the map without bound.
    pub max_buffer: usize,
    /// When the earliest buffered frame sits at least this many slots
    /// past the one we're waiting for, stop concealing one slot at a
    /// time and resync the play head onto it.
    pub resync_gap: usize,
    /// After this many consecutive clean (in-order, no-concealment)
    /// frames, narrow `target` by one.
    pub lower_after_clean: u32,
    /// When the buffer holds more than `target + catchup_slack`
    /// frames, the oldest are dropped down to `target` so the play
    /// head catches up with the sender.
    pub catchup_slack: usize,
}

impl Default for JitterConfig {
    /// 60 ms initial cushion between a 40 ms floor and a 240 ms
    /// ceiling (past which added mouth-to-ear latency hurts a
    /// conversation more than the concealment it buys); 2 s hard cap;
    /// resync after 240 ms of dead air; narrow after 5 s of clean
    /// audio; catch up when 80 ms over target.
    fn default() -> Self {
        Self {
            initial_target: 3,
            min_target: 2,
            max_target: 12,
            max_buffer: 100,
            resync_gap: 12,
            lower_after_clean: 250,
            catchup_slack: 4,
        }
    }
}

/// Concealment attenuation per successive concealed frame. A burst of
/// loss decays geometrically toward silence instead of buzzing on a
/// held frame.
const CONCEAL_FADE: f32 = 0.6;

/// Running counters, surfaced for a one-line teardown log and asserted
/// against in tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JitterStats {
    /// Frames offered to `insert` (includes those later dropped).
    pub inserted: u64,
    /// Frames released to the consumer in order.
    pub played: u64,
    /// Concealment frames emitted for a genuine mid-stream loss.
    pub concealed_loss: u64,
    /// Concealment frames emitted while the buffer was starved.
    pub concealed_starve: u64,
    /// Frames dropped as duplicates of an already-buffered slot.
    pub duplicates: u64,
    /// Frames dropped because their playout slot had already passed.
    pub late: u64,
    /// Frames dropped as implausibly far ahead of the play head.
    pub overflow: u64,
    /// Times the play head jumped forward over a large gap.
    pub resyncs: u64,
    /// Times a sustained stall forced a re-prime with a wider cushion.
    pub rebuffers: u64,
    /// Frames skipped to pull a too-deep buffer back to target.
    pub catchup: u64,
}

/// What [`JitterBuffer::insert`] did with the offered frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Insert {
    /// Buffered at its sequence slot.
    Buffered,
    /// Dropped: a frame for that sequence number was already buffered.
    Duplicate,
    /// Dropped: its playout slot has already passed.
    Late,
    /// Dropped: implausibly far ahead of the play head.
    Overflow,
}

/// What [`JitterBuffer::tick_into`] wrote into the caller's frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Playout {
    /// Still priming the initial cushion; nothing was written.
    Priming,
    /// A real frame, in order.
    Played,
    /// A concealment frame (loss or starvation).
    Concealed,
}

/// One buffered frame: the decoded PCM plus its RTP sequence number
/// (retained so the play head can re-anchor exactly on a resync).
struct Stored {
    seq: u16,
    pcm: Vec<i16>,
}

/// Sequence-keyed playout buffer. See the module docs.
pub struct JitterBuffer {
    cfg: JitterConfig,
    /// Pending frames keyed by absolute play index (monotonic, wrap-free
    /// internally). Only ever holds indices `>= expected`.
    frames: BTreeMap<i64, Stored>,
    /// Absolute index of the next frame to play; `None` until the first
    /// packet anchors the sequence space.
    expected: Option<i64>,
    /// Correspondence used to map an incoming `u16` sequence to an
    /// absolute index. Invariant: `anchor_index == expected` and
    /// `anchor_seq` is the sequence number that belongs at `expected`.
    anchor_index: i64,
    anchor_seq: u16,
    /// Whether the initial `target` cushion has filled and playout has
    /// begun. Cleared on a sustained stall to force a re-prime.
    primed: bool,
    /// Adaptive playout depth, in frames.
    target: usize,
    /// Samples in a concealment frame when no prior frame exists.
    frame_samples: usize,
    /// Last frame actually played, faded and re-emitted during loss.
    last_frame: Vec<i16>,
    /// Consecutive concealment frames emitted (drives the fade).
    conceal_run: u32,
    /// Consecutive starved ticks since the last real frame.
    starve_run: usize,
    /// Consecutive clean in-order plays (drives the narrow-down).
    clean_run: u32,
    stats: JitterStats,
}

impl JitterBuffer {
    /// Buffer with the default [`JitterConfig`]; `frame_samples` sizes
    /// the silence emitted before any frame has played.
    #[must_use]
    pub fn new(frame_samples: usize) -> Self {
        Self::with_config(frame_samples, JitterConfig::default())
    }

    /// Buffer with explicit tunables.
    #[must_use]
    pub fn with_config(frame_samples: usize, cfg: JitterConfig) -> Self {
        Self {
            cfg,
            frames: BTreeMap::new(),
            expected: None,
            anchor_index: 0,
            anchor_seq: 0,
            primed: false,
            target: cfg.initial_target,
            frame_samples,
            last_frame: Vec::new(),
            conceal_run: 0,
            starve_run: 0,
            clean_run: 0,
            stats: JitterStats::default(),
        }
    }

    /// Snapshot of the running counters.
    #[must_use]
    pub fn stats(&self) -> JitterStats {
        self.stats
    }

    /// Frames currently buffered ahead of the play head.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// Current adaptive playout depth, in frames.
    #[must_use]
    pub fn target(&self) -> usize {
        self.target
    }

    /// `true` once the initial cushion has filled and playout runs.
    #[must_use]
    pub fn is_primed(&self) -> bool {
        self.primed
    }

    /// Map an incoming sequence number to an absolute play index. The
    /// distance from the play head is taken as a signed 16-bit delta so
    /// `u16` wraparound resolves to the nearest interpretation — safe
    /// because real reordering and loss are tiny next to 2^15 frames.
    fn index_of(&self, seq: u16) -> i64 {
        #[allow(clippy::cast_possible_wrap)] // signed delta is the intent
        let delta = seq.wrapping_sub(self.anchor_seq) as i16;
        self.anchor_index + i64::from(delta)
    }

    /// Offer a decoded frame at RTP sequence `seq` to the buffer.
    pub fn insert(&mut self, seq: u16, pcm: Vec<i16>) -> Insert {
        self.stats.inserted += 1;

        // The first packet ever — or the first after a stall drained the
        // buffer while re-priming — anchors (or re-anchors) the sequence
        // space onto itself.
        if self.expected.is_none() || (!self.primed && self.frames.is_empty()) {
            self.frames.clear();
            self.anchor_index = 0;
            self.anchor_seq = seq;
            self.expected = Some(0);
            self.frames.insert(0, Stored { seq, pcm });
            return Insert::Buffered;
        }

        let expected = self.expected.unwrap_or(0);
        let idx = self.index_of(seq);
        if idx < expected {
            // Its playout slot already passed — arrived too late.
            self.stats.late += 1;
            return Insert::Late;
        }
        if idx - expected >= i64::try_from(self.cfg.max_buffer).unwrap_or(i64::MAX) {
            // Implausibly far ahead: treat as garbage, don't grow the map.
            self.stats.overflow += 1;
            return Insert::Overflow;
        }
        match self.frames.entry(idx) {
            Entry::Occupied(_) => {
                self.stats.duplicates += 1;
                Insert::Duplicate
            }
            Entry::Vacant(slot) => {
                slot.insert(Stored { seq, pcm });
                Insert::Buffered
            }
        }
    }

    /// Advance the play head by one frame interval. Returns the frame to
    /// hand to the consumer, or `None` while still priming the initial
    /// cushion (the caller should emit nothing, i.e. silence).
    pub fn tick(&mut self) -> Option<Vec<i16>> {
        match self.advance()? {
            Next::Frame(pcm) => Some(pcm),
            Next::Conceal => Some(self.conceal_frame()),
        }
    }

    /// Like [`Self::tick`] but writes into `out` (silence-filled when
    /// priming or when `out` is longer than the frame) instead of
    /// handing back an owned frame. For callers that mix or encode in
    /// place and don't want a `Vec` per tick.
    pub fn tick_into(&mut self, out: &mut [i16]) -> Playout {
        match self.advance() {
            None => {
                out.fill(0);
                Playout::Priming
            }
            Some(Next::Frame(pcm)) => {
                copy_frame(&pcm, out);
                Playout::Played
            }
            Some(Next::Conceal) => {
                self.conceal_into(out);
                Playout::Concealed
            }
        }
    }

    /// Shared play-head logic behind [`Self::tick`] / [`Self::tick_into`].
    fn advance(&mut self) -> Option<Next> {
        let expected = self.expected?; // no stream yet → nothing to play
        if !self.primed {
            if self.frames.len() < self.target {
                return None; // keep buffering the initial cushion
            }
            self.primed = true;
        }

        let expected = self.catch_up(expected);

        // The frame we want is here: release it in order.
        if let Some(frame) = self.frames.remove(&expected) {
            let out = self.deliver(expected, frame);
            self.note_clean();
            return Some(Next::Frame(out));
        }

        // Expected frame absent, but later frames are buffered.
        if let Some(lowest) = self.frames.keys().next().copied() {
            self.clean_run = 0;
            if lowest - expected >= i64::try_from(self.cfg.resync_gap).unwrap_or(i64::MAX) {
                // Too much dead air to conceal one slot at a time — skip
                // to the earliest frame we actually have.
                if let Some(frame) = self.frames.remove(&lowest) {
                    self.stats.resyncs += 1;
                    return Some(Next::Frame(self.deliver(lowest, frame)));
                }
            }
            // Small gap: the expected frame is genuinely lost. Conceal it
            // and step the play head over its slot.
            self.stats.concealed_loss += 1;
            self.expected = Some(expected + 1);
            self.anchor_index = expected + 1;
            self.anchor_seq = self.anchor_seq.wrapping_add(1);
            return Some(Next::Conceal);
        }

        // Buffer fully drained: starvation.
        self.clean_run = 0;
        self.stats.concealed_starve += 1;
        self.starve_run += 1;
        if self.starve_run >= self.target {
            // Sustained stall: widen the cushion and re-prime so audio
            // resumes on a clean, in-order run rather than chronic
            // single-frame underruns. The play head stays put; the next
            // arriving packet re-anchors it (see `insert`).
            self.target = (self.target + 1).min(self.cfg.max_target);
            self.primed = false;
            self.starve_run = 0;
            self.stats.rebuffers += 1;
        }
        Some(Next::Conceal)
    }

    /// Bound latency: when the buffer has run deeper than
    /// `target + catchup_slack` frames the sender is ahead of us
    /// (its clock runs fast, or our clock stalled). Drop the oldest
    /// frames down to `target` and move the play head onto the
    /// earliest survivor. Returns the (possibly moved) play index.
    fn catch_up(&mut self, expected: i64) -> i64 {
        if self.frames.len() <= self.target + self.cfg.catchup_slack {
            return expected;
        }
        while self.frames.len() > self.target {
            if self.frames.pop_first().is_none() {
                break;
            }
            self.stats.catchup += 1;
        }
        match self.frames.first_key_value() {
            Some((&idx, stored)) => {
                self.expected = Some(idx);
                self.anchor_index = idx;
                self.anchor_seq = stored.seq;
                idx
            }
            None => expected,
        }
    }

    /// Release `frame` (played at absolute index `pos`) and move the play
    /// head to the following slot.
    fn deliver(&mut self, pos: i64, frame: Stored) -> Vec<i16> {
        let Stored { seq, pcm } = frame;
        self.last_frame.clone_from(&pcm);
        self.conceal_run = 0;
        self.starve_run = 0;
        self.stats.played += 1;
        self.expected = Some(pos + 1);
        self.anchor_index = pos + 1;
        self.anchor_seq = seq.wrapping_add(1);
        pcm
    }

    /// Gain for the next concealment frame: the last good frame
    /// attenuated by a fade that deepens with each consecutive
    /// concealment, decaying to silence over a loss burst. `None`
    /// means emit pure silence (nothing has played yet, or the fade
    /// has reached the floor).
    fn conceal_gain(&mut self) -> Option<f32> {
        self.conceal_run += 1;
        if self.last_frame.is_empty() {
            return None;
        }
        let gain = CONCEAL_FADE.powi(i32::try_from(self.conceal_run).unwrap_or(i32::MAX));
        (gain >= 0.05).then_some(gain)
    }

    fn conceal_frame(&mut self) -> Vec<i16> {
        match self.conceal_gain() {
            None if self.last_frame.is_empty() => vec![0i16; self.frame_samples],
            None => vec![0i16; self.last_frame.len()],
            Some(gain) => self.last_frame.iter().map(|s| scale(*s, gain)).collect(),
        }
    }

    fn conceal_into(&mut self, out: &mut [i16]) {
        match self.conceal_gain() {
            None => out.fill(0),
            Some(gain) => {
                let n = out.len().min(self.last_frame.len());
                for (o, s) in out[..n].iter_mut().zip(&self.last_frame) {
                    *o = scale(*s, gain);
                }
                out[n..].fill(0);
            }
        }
    }

    /// Record a clean in-order play; narrow the cushion after a long
    /// calm stretch to recover latency.
    fn note_clean(&mut self) {
        self.clean_run += 1;
        if self.clean_run >= self.cfg.lower_after_clean {
            self.clean_run = 0;
            if self.target > self.cfg.min_target {
                self.target -= 1;
            }
        }
    }
}

/// Outcome of one play-head advance before it is materialized.
enum Next {
    Frame(Vec<i16>),
    Conceal,
}

fn copy_frame(pcm: &[i16], out: &mut [i16]) {
    let n = out.len().min(pcm.len());
    out[..n].copy_from_slice(&pcm[..n]);
    out[n..].fill(0);
}

#[allow(clippy::cast_possible_truncation)] // |gain| ≤ 1 keeps the product in i16 range
fn scale(sample: i16, gain: f32) -> i16 {
    (f32::from(sample) * gain) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame of `n` samples all equal to `v` — an easy-to-assert tag.
    fn frame(v: i16, n: usize) -> Vec<i16> {
        vec![v; n]
    }

    const INITIAL_TARGET: usize = 3;
    const LOWER_AFTER_CLEAN: u32 = 250;

    #[test]
    fn primes_before_releasing_audio() {
        let mut jb = JitterBuffer::new(4);
        assert_eq!(jb.target, INITIAL_TARGET);
        assert!(jb.tick().is_none(), "no stream yet");
        jb.insert(10, frame(1, 4));
        assert!(jb.tick().is_none(), "1 < target");
        jb.insert(11, frame(2, 4));
        assert!(jb.tick().is_none(), "2 < target");
        jb.insert(12, frame(3, 4));
        assert_eq!(jb.tick(), Some(frame(1, 4)), "primed, first frame out");
        assert_eq!(jb.tick(), Some(frame(2, 4)));
        assert_eq!(jb.tick(), Some(frame(3, 4)));
    }

    #[test]
    fn reorders_by_sequence() {
        let mut jb = JitterBuffer::new(4);
        jb.insert(10, frame(1, 4));
        jb.insert(12, frame(3, 4));
        jb.insert(11, frame(2, 4)); // arrived out of order
        assert_eq!(jb.tick(), Some(frame(1, 4)));
        assert_eq!(jb.tick(), Some(frame(2, 4)));
        assert_eq!(jb.tick(), Some(frame(3, 4)));
        assert_eq!(jb.stats.played, 3);
    }

    #[test]
    fn drops_duplicates_keeping_first() {
        let mut jb = JitterBuffer::new(4);
        assert_eq!(jb.insert(10, frame(1, 4)), Insert::Buffered);
        jb.insert(11, frame(2, 4));
        assert_eq!(jb.insert(11, frame(9, 4)), Insert::Duplicate);
        jb.insert(12, frame(3, 4));
        assert_eq!(jb.tick(), Some(frame(1, 4)));
        assert_eq!(jb.tick(), Some(frame(2, 4)), "kept first copy, not the dup");
        assert_eq!(jb.tick(), Some(frame(3, 4)));
        assert_eq!(jb.stats.duplicates, 1);
    }

    #[test]
    fn drops_late_arrivals() {
        let mut jb = JitterBuffer::new(4);
        jb.insert(10, frame(1, 4));
        jb.insert(11, frame(2, 4));
        jb.insert(12, frame(3, 4));
        assert_eq!(jb.tick(), Some(frame(1, 4))); // play head now past seq 10
        assert_eq!(jb.insert(9, frame(9, 4)), Insert::Late);
        assert_eq!(jb.stats.late, 1);
        assert_eq!(jb.tick(), Some(frame(2, 4)), "late frame did not intrude");
    }

    #[test]
    fn conceals_a_single_loss_and_recovers() {
        let mut jb = JitterBuffer::new(4);
        jb.insert(10, frame(1, 4));
        jb.insert(11, frame(2, 4));
        jb.insert(13, frame(4, 4)); // seq 12 lost; primes at 3 frames
        assert_eq!(jb.tick(), Some(frame(1, 4)));
        assert_eq!(jb.tick(), Some(frame(2, 4)));
        // seq 12 missing, seq 13 waiting one slot ahead → conceal.
        // last good frame [2,2,2,2] faded by 0.6 → [1,1,1,1].
        assert_eq!(jb.tick(), Some(frame(1, 4)));
        assert_eq!(jb.stats.concealed_loss, 1);
        assert_eq!(
            jb.tick(),
            Some(frame(4, 4)),
            "resumes in order after the gap"
        );
    }

    #[test]
    fn handles_sequence_wraparound() {
        let mut jb = JitterBuffer::new(4);
        jb.insert(65_534, frame(1, 4));
        jb.insert(65_535, frame(2, 4));
        jb.insert(0, frame(3, 4)); // wraps past u16::MAX
        assert_eq!(jb.tick(), Some(frame(1, 4)));
        assert_eq!(jb.tick(), Some(frame(2, 4)));
        assert_eq!(jb.tick(), Some(frame(3, 4)));
        assert_eq!(jb.stats.played, 3);
    }

    #[test]
    fn resyncs_over_a_large_gap() {
        let mut jb = JitterBuffer::new(4);
        jb.insert(10, frame(1, 4));
        jb.insert(11, frame(2, 4));
        jb.insert(12, frame(3, 4));
        jb.tick();
        jb.tick();
        jb.tick(); // drained; play head expects seq 13
        jb.insert(33, frame(5, 4)); // 20 slots ahead — beyond resync_gap
        assert_eq!(jb.tick(), Some(frame(5, 4)), "jumps to the frame we have");
        assert_eq!(jb.stats.resyncs, 1);
    }

    #[test]
    fn rebuffers_and_widens_after_sustained_starvation() {
        let mut jb = JitterBuffer::new(4);
        jb.insert(10, frame(1, 4));
        jb.insert(11, frame(2, 4));
        jb.insert(12, frame(3, 4));
        jb.tick();
        jb.tick();
        jb.tick(); // drained
        assert!(jb.frames.is_empty());
        jb.tick(); // starve 1
        jb.tick(); // starve 2
        assert_eq!(jb.stats.rebuffers, 0);
        jb.tick(); // starve 3 == target → rebuffer
        assert_eq!(jb.stats.rebuffers, 1);
        assert_eq!(jb.target, 4, "cushion widened");
        assert!(!jb.primed, "back to priming");

        // Stream resumes on a fresh, distant sequence — must re-anchor.
        jb.insert(500, frame(7, 4));
        assert_eq!(jb.expected, Some(0));
        jb.insert(501, frame(8, 4));
        jb.insert(502, frame(9, 4));
        assert!(jb.tick().is_none(), "3 < widened target of 4");
        jb.insert(503, frame(1, 4));
        assert_eq!(jb.tick(), Some(frame(7, 4)), "re-primed and playing");
    }

    #[test]
    fn narrows_cushion_after_a_long_clean_run() {
        let mut jb = JitterBuffer::new(4);
        jb.insert(0, frame(1, 4));
        jb.insert(1, frame(1, 4));
        jb.insert(2, frame(1, 4));
        assert!(jb.tick().is_some()); // prime + first play
        let mut next = 3u16;
        for _ in 0..=LOWER_AFTER_CLEAN {
            jb.insert(next, frame(1, 4));
            next = next.wrapping_add(1);
            assert!(jb.tick().is_some(), "buffer stays fed, always plays");
        }
        assert_eq!(jb.target, INITIAL_TARGET - 1, "cushion narrowed by one");
    }

    #[test]
    fn catches_up_when_the_buffer_runs_deep() {
        // Prime with 3, play one, then let 12 more pile up without a
        // tick (the sender's clock ran ahead of ours).
        let mut jb = JitterBuffer::new(4);
        for seq in 10..=12u16 {
            jb.insert(seq, frame(seq.cast_signed(), 4));
        }
        assert_eq!(jb.tick(), Some(frame(10, 4)));
        for seq in 13..=24u16 {
            jb.insert(seq, frame(seq.cast_signed(), 4));
        }
        assert_eq!(jb.depth(), 14, "11..=24 buffered");
        // 14 > target(3) + slack(4): drop 11 oldest, play the earliest
        // survivor (seq 22) and continue in order from there.
        assert_eq!(jb.tick(), Some(frame(22, 4)));
        assert_eq!(jb.stats.catchup, 11);
        assert_eq!(jb.tick(), Some(frame(23, 4)));
        assert_eq!(jb.tick(), Some(frame(24, 4)));
        // Sequence space is still anchored correctly after the jump.
        jb.insert(25, frame(25, 4));
        assert_eq!(jb.tick(), Some(frame(25, 4)));
        assert_eq!(jb.stats.late, 0);
    }

    #[test]
    fn depth_within_slack_is_not_caught_up() {
        let mut jb = JitterBuffer::new(4);
        for seq in 0..7u16 {
            jb.insert(seq, frame(1, 4)); // exactly target + slack
        }
        assert!(jb.tick().is_some());
        assert_eq!(jb.stats.catchup, 0);
    }

    #[test]
    fn tick_into_fills_silence_while_priming_and_copies_frames() {
        let mut jb = JitterBuffer::new(4);
        let mut out = [7i16; 4];
        assert_eq!(jb.tick_into(&mut out), Playout::Priming);
        assert_eq!(out, [0; 4]);
        jb.insert(1, frame(5, 4));
        jb.insert(2, frame(6, 4));
        jb.insert(4, frame(8, 4));
        assert_eq!(jb.tick_into(&mut out), Playout::Played);
        assert_eq!(out, [5; 4]);
        assert_eq!(jb.tick_into(&mut out), Playout::Played);
        assert_eq!(out, [6; 4]);
        assert_eq!(jb.tick_into(&mut out), Playout::Concealed);
        assert_eq!(out, [3; 4], "last frame faded by 0.6");
        assert_eq!(jb.tick_into(&mut out), Playout::Played);
        assert_eq!(out, [8; 4]);
    }

    #[test]
    fn overflow_is_reported_and_dropped() {
        let mut jb = JitterBuffer::new(4);
        jb.insert(0, frame(1, 4));
        assert_eq!(jb.insert(500, frame(1, 4)), Insert::Overflow);
        assert_eq!(jb.stats.overflow, 1);
        assert_eq!(jb.depth(), 1);
    }

    #[test]
    fn custom_config_sets_initial_target() {
        let cfg = JitterConfig {
            initial_target: 1,
            min_target: 1,
            ..JitterConfig::default()
        };
        let mut jb = JitterBuffer::with_config(4, cfg);
        jb.insert(3, frame(9, 4));
        assert_eq!(
            jb.tick(),
            Some(frame(9, 4)),
            "one frame primes a depth-1 buffer"
        );
    }
}
