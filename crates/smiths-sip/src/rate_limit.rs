//! Per-source-IP token bucket for the UAS ingress.
//!
//! A hostile peer can flood the UAS's UDP socket faster than we can
//! respond. Without backpressure the OS recv buffer saturates (we
//! measured this during the v0.13.1 sipp prove-out) and legitimate
//! traffic starves. This module is anti-flood: each source IP gets
//! its own token bucket; when the bucket is empty the datagram is
//! dropped silently before it reaches `handle_datagram`.
//!
//! **Silent drop, not 503.** Emitting a response on every dropped
//! datagram would burn the same CPU we're trying to protect, and a
//! real attacker doesn't care about a status code.
//!
//! **Per-IP, not per-`(IP, port)`.** An attacker can rotate source
//! ports cheaply; buckets keyed on IP resist that. Legitimate `NAT`ed
//! clients share the bucket for their public IP — that's acceptable
//! because the bucket size (configurable `burst`) covers realistic
//! burstiness from a carrier NAT.
//!
//! The limiter is disabled by default (`per_sec == 0`) so dev
//! deployments don't trip on their own test traffic.

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use dashmap::DashMap;
use smiths_core::SipRateLimit;

/// Cheaply-clonable shared rate limiter. An inner `Arc<DashMap>`
/// keyed by source IP holds the per-source bucket state; the
/// thresholds (`rate_per_sec`, `burst`) live in atomics so a
/// config read-through adapter can live-swap them via
/// [`Self::reconfigure`] without rebuilding the limiter
/// (slice 5.8-b).
#[derive(Clone)]
pub struct SipRateLimiter {
    inner: Arc<Inner>,
}

struct Inner {
    /// Refill rate (tokens per second). `0` = limiter disabled
    /// (every `allow()` returns true). Atomic so
    /// [`SipRateLimiter::reconfigure`] can swap it live.
    rate_per_sec: AtomicU32,
    /// Bucket depth (max tokens). Atomic for the same reason.
    burst: AtomicU32,
    buckets: DashMap<IpAddr, Bucket>,
}

#[derive(Copy, Clone)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl SipRateLimiter {
    /// Build a limiter from config. `per_sec == 0` returns a disabled
    /// limiter whose [`Self::allow`] is always `true` — every caller
    /// can wire the limiter unconditionally without branching on
    /// whether rate limiting is configured.
    #[must_use]
    pub fn new(cfg: SipRateLimit) -> Self {
        // `burst == 0` falls back to `per_sec` so a bare `per_sec`
        // config stanza behaves like "rate == burst == per_sec" —
        // matches how the MCP rate limiter treats the field.
        let burst = if cfg.burst == 0 {
            cfg.per_sec
        } else {
            cfg.burst
        };
        Self {
            inner: Arc::new(Inner {
                rate_per_sec: AtomicU32::new(cfg.per_sec),
                burst: AtomicU32::new(burst),
                buckets: DashMap::new(),
            }),
        }
    }

    /// A disabled limiter (always allows). Useful in tests / embedded
    /// callers that don't wire a config.
    #[must_use]
    pub fn disabled() -> Self {
        Self::new(SipRateLimit::default())
    }

    /// Atomically swap the rate thresholds (slice 5.8-b
    /// read-through). Per-source buckets keep their existing
    /// token counts — tokens earned under the old rate stay
    /// valid, and the next `allow()` call refills using the
    /// new rate + new cap. No bucket reset, no traffic jolt.
    ///
    /// `per_sec == 0` disables the limiter immediately; a
    /// later `reconfigure` to non-zero re-enables it.
    pub fn reconfigure(&self, cfg: SipRateLimit) {
        let burst = if cfg.burst == 0 {
            cfg.per_sec
        } else {
            cfg.burst
        };
        self.inner
            .rate_per_sec
            .store(cfg.per_sec, Ordering::Release);
        self.inner.burst.store(burst, Ordering::Release);
    }

    /// `true` when `source` has a token to spend; `false` when the
    /// bucket is empty (caller should drop the datagram).
    ///
    /// When `rate_per_sec == 0` the limiter is off and this returns
    /// `true` immediately without touching the map.
    #[must_use]
    pub fn allow(&self, source: IpAddr) -> bool {
        let rate_u32 = self.inner.rate_per_sec.load(Ordering::Acquire);
        if rate_u32 == 0 {
            return true;
        }
        let rate = f64::from(rate_u32);
        let burst = f64::from(self.inner.burst.load(Ordering::Acquire));
        let now = Instant::now();
        let mut entry = self.inner.buckets.entry(source).or_insert(Bucket {
            tokens: burst,
            last: now,
        });
        // Refill by elapsed time × rate, clamped at burst.
        let elapsed = now.saturating_duration_since(entry.last).as_secs_f64();
        entry.tokens = (entry.tokens + elapsed * rate).min(burst);
        entry.last = now;
        if entry.tokens >= 1.0 {
            entry.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Current refill rate (tokens/sec). Read-only snapshot of
    /// the atomic; `0` = limiter disabled.
    #[must_use]
    pub fn rate_per_sec(&self) -> u32 {
        self.inner.rate_per_sec.load(Ordering::Acquire)
    }

    /// Current bucket depth. Read-only snapshot.
    #[must_use]
    pub fn burst(&self) -> u32 {
        self.inner.burst.load(Ordering::Acquire)
    }

    /// Current bucket count — for tests and diagnostics.
    #[must_use]
    pub fn tracked_sources(&self) -> usize {
        self.inner.buckets.len()
    }
}

impl std::fmt::Debug for SipRateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SipRateLimiter")
            .field("rate_per_sec", &self.rate_per_sec())
            .field("burst", &self.burst())
            .field("tracked_sources", &self.inner.buckets.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn disabled_always_allows() {
        let lim = SipRateLimiter::disabled();
        for _ in 0..10_000 {
            assert!(lim.allow(ip(10, 0, 0, 1)));
        }
        assert_eq!(lim.tracked_sources(), 0, "disabled path must not touch map");
    }

    #[test]
    fn burst_then_denies_until_refill() {
        // 10 tokens per second, burst 5. First 5 allowed; 6th denied.
        let lim = SipRateLimiter::new(SipRateLimit {
            per_sec: 10,
            burst: 5,
        });
        let src = ip(10, 0, 0, 1);
        for i in 0..5 {
            assert!(lim.allow(src), "burst token {i} must be allowed");
        }
        assert!(!lim.allow(src), "6th attempt must be denied");
    }

    #[test]
    fn different_sources_have_independent_buckets() {
        let lim = SipRateLimiter::new(SipRateLimit {
            per_sec: 1,
            burst: 1,
        });
        assert!(lim.allow(ip(10, 0, 0, 1)));
        // Same IP exhausted, different IP unaffected.
        assert!(!lim.allow(ip(10, 0, 0, 1)));
        assert!(lim.allow(ip(10, 0, 0, 2)));
    }

    #[test]
    fn reconfigure_raises_thresholds_live() {
        // Cap at burst 1, exhaust it, then raise burst + rate via
        // reconfigure. The new rate kicks in for the next refill —
        // 10 ms at 1000 tok/s is ~10 tokens, well above the one token
        // the next `allow` needs. Under the old 1-tok/s rate 10 ms
        // would yield only 0.01 tokens, so the test would still fail
        // without the reconfigure.
        let lim = SipRateLimiter::new(SipRateLimit {
            per_sec: 1,
            burst: 1,
        });
        let src = ip(10, 0, 0, 1);
        assert!(lim.allow(src));
        assert!(!lim.allow(src), "bucket exhausted under old cap");
        lim.reconfigure(SipRateLimit {
            per_sec: 1_000,
            burst: 1_000,
        });
        assert_eq!(lim.rate_per_sec(), 1_000);
        assert_eq!(lim.burst(), 1_000);
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(lim.allow(src), "new rate should refill the bucket");
    }

    #[test]
    fn reconfigure_to_zero_disables_limiter() {
        // Enabled limiter → reconfigure(per_sec=0) flips it to the
        // always-allow path. No per-source bucket lookup.
        let lim = SipRateLimiter::new(SipRateLimit {
            per_sec: 1,
            burst: 1,
        });
        let src = ip(10, 0, 0, 1);
        assert!(lim.allow(src));
        assert!(!lim.allow(src));
        lim.reconfigure(SipRateLimit::default());
        assert_eq!(lim.rate_per_sec(), 0);
        for _ in 0..1_000 {
            assert!(lim.allow(src), "disabled limiter must allow");
        }
    }

    #[test]
    fn reconfigure_bare_per_sec_sets_burst_to_match() {
        // Mirrors the `new()` fallback: a config with `burst == 0`
        // should treat `per_sec` as the bucket depth too. This is the
        // shape read-through config edits take when the operator only
        // writes `per_sec` under the `[sip.rate_limit]` stanza.
        let lim = SipRateLimiter::new(SipRateLimit {
            per_sec: 10,
            burst: 0,
        });
        assert_eq!(lim.burst(), 10);
        lim.reconfigure(SipRateLimit {
            per_sec: 25,
            burst: 0,
        });
        assert_eq!(lim.rate_per_sec(), 25);
        assert_eq!(lim.burst(), 25);
    }

    #[test]
    fn bucket_refills_over_time() {
        // 50 tok/s = 20 ms per token. The gap between the two
        // back-to-back `allow` calls must be under one token's worth
        // of time, otherwise the bucket silently refills and the
        // "denied" assertion flakes. 20 ms is comfortably larger
        // than any realistic inter-call scheduling delay, and the
        // 40 ms sleep is still well above one-token of refill, so
        // the "allowed again" side of the test stays deterministic.
        let lim = SipRateLimiter::new(SipRateLimit {
            per_sec: 50,
            burst: 1,
        });
        let src = ip(10, 0, 0, 1);
        assert!(lim.allow(src));
        assert!(!lim.allow(src));
        std::thread::sleep(std::time::Duration::from_millis(40));
        assert!(lim.allow(src), "bucket should have refilled");
    }
}
