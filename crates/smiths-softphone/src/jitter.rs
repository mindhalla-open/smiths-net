//! Adaptive playout jitter buffer for received RTP audio frames.
//!
//! The softphone receives one 20 ms G.711 frame per RTP packet. UDP
//! delivers those packets reordered, duplicated, late, or not at all,
//! and a sender's media clock never ticks in perfect lockstep with the
//! local speaker. Feeding packets straight to the speaker in arrival
//! order (as the earlier `recv_loop` did — it decoded the sequence
//! number and then discarded it) plays reorders out of order, plays
//! duplicates twice, and closes loss gaps by pulling later audio
//! forward. All three are audible on a real network.
//!
//! This buffer decouples *arrival* from *playout*:
//! - [`JitterBuffer::insert`] places each decoded frame at its absolute
//!   sequence index, dropping duplicates and packets that arrive after
//!   their playout slot has already passed.
//! - [`JitterBuffer::tick`], called once per 20 ms frame interval,
//!   releases the next in-order frame — or a concealment frame when the
//!   expected one is missing (packet-loss concealment: the last good
//!   frame, faded toward silence over a run of losses).
//! - The playout depth (`target`) is primed before audio starts and
//!   adapts: sustained starvation widens the cushion and re-buffers so
//!   audio resumes on a clean in-order run; a long calm stretch narrows
//!   it back toward the floor to give the earned latency back.
//!
//! It is deliberately I/O-free — no sockets, no codec, no wall clock —
//! so it unit-tests deterministically and can move into `smiths-media`
//! for reuse by other playout/transcode endpoints. The caller drives
//! [`JitterBuffer::tick`] from its own frame clock; the buffer never
//! reads the clock itself.
//!
//! It keys purely on the RTP **sequence number**, assuming one 20 ms
//! frame per packet (true for this softphone's G.711 stream and the
//! engine bridge that relays it). Timestamp-carried silence gaps are
//! not reconstructed — an MVP-appropriate simplification.

// Fixed-point audio math: f32 gain applied to i16 PCM, and i64/u16
// sequence-index arithmetic. All casts are range-bounded by
// construction; the pedantic cast lints are noise here (mirrors
// `audio.rs`).
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

/// Playout depth bounds, in 20 ms frames. 2 frames = 40 ms of cushion
/// at the floor; 12 = 240 ms at the ceiling, past which added mouth-to-
/// ear latency hurts a conversation more than the concealment it buys.
const MIN_TARGET: usize = 2;
const MAX_TARGET: usize = 12;
/// Initial cushion (60 ms) primed before the first frame is released.
const INITIAL_TARGET: usize = 3;

/// Hard cap on how far ahead of the play head a frame may sit (2 s).
/// Anything further is a bogus/huge sequence jump, not reordering —
/// dropped so a garbage packet can't grow the map without bound.
const MAX_BUFFER: i64 = 100;

/// When the earliest buffered frame sits at least this many slots past
/// the one we're waiting for, stop concealing one slot at a time and
/// resync the play head onto it. Bounds catch-up time after a real
/// discontinuity (matches `MAX_TARGET`).
const RESYNC_GAP: i64 = 12;

/// After this many consecutive clean (in-order, no-concealment) frames,
/// narrow `target` by one to recover latency earned during a calm
/// stretch. 250 frames = 5 s.
const LOWER_AFTER_CLEAN: u32 = 250;

/// Concealment attenuation per successive concealed frame. A burst of
/// loss decays geometrically toward silence instead of buzzing on a
/// held frame.
const CONCEAL_FADE: f32 = 0.6;

/// Running counters, surfaced for a one-line teardown log and asserted
/// against in tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JitterStats {
    /// Frames offered to `insert` (includes those later dropped).
    pub inserted: u64,
    /// Frames released to the speaker in order.
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
}

/// One buffered frame: the decoded PCM plus its RTP sequence number
/// (retained so the play head can re-anchor exactly on a resync).
struct Stored {
    seq: u16,
    pcm: Vec<i16>,
}

pub(crate) struct JitterBuffer {
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
    pub(crate) fn new(frame_samples: usize) -> Self {
        Self {
            frames: BTreeMap::new(),
            expected: None,
            anchor_index: 0,
            anchor_seq: 0,
            primed: false,
            target: INITIAL_TARGET,
            frame_samples,
            last_frame: Vec::new(),
            conceal_run: 0,
            starve_run: 0,
            clean_run: 0,
            stats: JitterStats::default(),
        }
    }

    /// Snapshot of the running counters.
    pub(crate) fn stats(&self) -> JitterStats {
        self.stats
    }

    /// Map an incoming sequence number to an absolute play index. The
    /// distance from the play head is taken as a signed 16-bit delta so
    /// `u16` wraparound resolves to the nearest interpretation — safe
    /// because real reordering and loss are tiny next to 2^15 frames.
    fn index_of(&self, seq: u16) -> i64 {
        let delta = seq.wrapping_sub(self.anchor_seq) as i16;
        self.anchor_index + i64::from(delta)
    }

    /// Offer a decoded frame at RTP sequence `seq` to the buffer.
    pub(crate) fn insert(&mut self, seq: u16, pcm: Vec<i16>) {
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
            return;
        }

        let expected = self.expected.expect("checked Some above");
        let idx = self.index_of(seq);
        if idx < expected {
            // Its playout slot already passed — arrived too late.
            self.stats.late += 1;
            return;
        }
        if idx - expected >= MAX_BUFFER {
            // Implausibly far ahead: treat as garbage, don't grow the map.
            self.stats.overflow += 1;
            return;
        }
        match self.frames.entry(idx) {
            Entry::Occupied(_) => self.stats.duplicates += 1,
            Entry::Vacant(slot) => {
                slot.insert(Stored { seq, pcm });
            }
        }
    }

    /// Advance the play head by one frame interval. Returns the frame to
    /// hand to the speaker, or `None` while still priming the initial
    /// cushion (the caller should emit nothing, i.e. silence).
    pub(crate) fn tick(&mut self) -> Option<Vec<i16>> {
        let expected = self.expected?; // no stream yet → nothing to play
        if !self.primed {
            if self.frames.len() < self.target {
                return None; // keep buffering the initial cushion
            }
            self.primed = true;
        }

        // The frame we want is here: release it in order.
        if let Some(frame) = self.frames.remove(&expected) {
            let out = self.deliver(expected, frame);
            self.note_clean();
            return Some(out);
        }

        // Expected frame absent, but later frames are buffered.
        if let Some(lowest) = self.frames.keys().next().copied() {
            self.clean_run = 0;
            if lowest - expected >= RESYNC_GAP {
                // Too much dead air to conceal one slot at a time — skip
                // to the earliest frame we actually have.
                let frame = self.frames.remove(&lowest).expect("key just observed");
                self.stats.resyncs += 1;
                return Some(self.deliver(lowest, frame));
            }
            // Small gap: the expected frame is genuinely lost. Conceal it
            // and step the play head over its slot.
            self.stats.concealed_loss += 1;
            self.expected = Some(expected + 1);
            self.anchor_index = expected + 1;
            self.anchor_seq = self.anchor_seq.wrapping_add(1);
            return Some(self.conceal());
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
            self.target = (self.target + 1).min(MAX_TARGET);
            self.primed = false;
            self.starve_run = 0;
            self.stats.rebuffers += 1;
        }
        Some(self.conceal())
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

    /// Produce a concealment frame: the last good frame attenuated by a
    /// fade that deepens with each consecutive concealment, decaying to
    /// silence over a loss burst. Pure silence before any frame has
    /// played.
    fn conceal(&mut self) -> Vec<i16> {
        self.conceal_run += 1;
        if self.last_frame.is_empty() {
            return vec![0i16; self.frame_samples];
        }
        let gain = CONCEAL_FADE.powi(self.conceal_run as i32);
        if gain < 0.05 {
            return vec![0i16; self.last_frame.len()];
        }
        self.last_frame
            .iter()
            .map(|s| (f32::from(*s) * gain) as i16)
            .collect()
    }

    /// Record a clean in-order play; narrow the cushion after a long
    /// calm stretch to recover latency.
    fn note_clean(&mut self) {
        self.clean_run += 1;
        if self.clean_run >= LOWER_AFTER_CLEAN {
            self.clean_run = 0;
            if self.target > MIN_TARGET {
                self.target -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame of `n` samples all equal to `v` — an easy-to-assert tag.
    fn frame(v: i16, n: usize) -> Vec<i16> {
        vec![v; n]
    }

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
        jb.insert(10, frame(1, 4));
        jb.insert(11, frame(2, 4));
        jb.insert(11, frame(9, 4)); // duplicate seq — must be ignored
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
        jb.insert(9, frame(9, 4)); // slot already gone
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
        jb.insert(33, frame(5, 4)); // 20 slots ahead — beyond RESYNC_GAP
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
}
