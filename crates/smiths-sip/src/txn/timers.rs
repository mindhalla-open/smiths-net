//! RFC 3261 §17.1.1.1 timer constants.
//!
//! All durations are **wall-clock** (not RTP ticks). `T1` is the
//! estimated round-trip time — 500 ms per the RFC. `T2` caps
//! retransmit backoff for non-INVITE methods. `T4` is the maximum
//! duration a duplicate response might arrive at.
//!
//! Names match the RFC (timer A, B, …) so the FSM code reads 1:1
//! against §17.

use std::time::Duration;

/// Estimated round-trip time. RFC 3261 §17.1.1.1 baseline: 500 ms.
pub const T1: Duration = Duration::from_millis(500);

/// Maximum retransmit interval for non-INVITE requests. 4 s.
pub const T2: Duration = Duration::from_secs(4);

/// Maximum duration a duplicate response might arrive. 5 s on
/// unreliable transport (RFC default).
pub const T4: Duration = Duration::from_secs(5);

/// 64·T1 = 32 s. Client transaction timeout (timers B, F, H).
pub const TIMEOUT_64T1: Duration = Duration::from_secs(32);

/// Computed timer duration helper. The caller passes the **attempt**
/// number (0-based) and the maximum; we return
/// `min(T1 * 2^attempt, max)`. RFC 3261 §17.1.2.2 uses this for the
/// non-INVITE client retransmit schedule (timer E).
#[must_use]
pub fn doubling_backoff(attempt: u32, max: Duration) -> Duration {
    // Cap the shift count at 16 so `1 << attempt` never overflows a
    // `u32`. Any attempt beyond that is already well past `max`.
    let multiplier: u32 = 1u32.checked_shl(attempt.min(16)).unwrap_or(u32::MAX);
    let scaled = T1.saturating_mul(multiplier);
    scaled.min(max)
}

/// One armed timer — the id plus its scheduled duration. The FSM
/// hands this shape to the driver via [`super::TransactionAction`];
/// nothing else uses it directly.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Timer {
    /// Which timer in the RFC table.
    pub id: super::TimerId,
    /// How long until it fires.
    pub after: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_until_cap() {
        // attempt 0 → T1 = 500 ms; attempt 1 → 1 s; attempt 2 → 2 s;
        // attempt 3 → 4 s; attempt 4 → would be 8 s but capped at T2.
        assert_eq!(doubling_backoff(0, T2), T1);
        assert_eq!(doubling_backoff(1, T2), Duration::from_secs(1));
        assert_eq!(doubling_backoff(2, T2), Duration::from_secs(2));
        assert_eq!(doubling_backoff(3, T2), T2);
        assert_eq!(doubling_backoff(4, T2), T2);
        assert_eq!(doubling_backoff(20, T2), T2);
    }

    #[test]
    fn constants_match_rfc() {
        assert_eq!(T1.as_millis(), 500);
        assert_eq!(T2.as_secs(), 4);
        assert_eq!(T4.as_secs(), 5);
        assert_eq!(TIMEOUT_64T1.as_secs(), 32);
    }
}
