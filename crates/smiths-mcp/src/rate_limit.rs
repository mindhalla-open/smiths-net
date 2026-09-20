//! Shared token-bucket rate limiter for tool invocations.
//!
//! Buckets are keyed by `(caller, tool)`: adapters that know who is
//! calling (an HTTP peer address, an MCP session id) get a private
//! bucket per caller, everything else shares one global bucket per
//! tool. Refill is a pure monotonic-clock calculation — no background
//! task. The bucket map is bounded: idle (fully refilled) buckets are
//! swept when the map is full, and if it is still full the caller
//! falls back to the tool's global bucket so an attacker who can mint
//! caller identities cannot grow memory without limit.
//!
//! Disabled by default; operators opt in via `mcp.rate_limit` config.

use std::sync::Mutex;
use std::time::Instant;

use dashmap::DashMap;
use smiths_core::RateLimitConfig;

/// Default cap on the number of live `(caller, tool)` buckets.
pub const DEFAULT_MAX_BUCKETS: usize = 4096;

/// Bucket state. One-thread-per-bucket contention is low enough that
/// `std::sync::Mutex` is the right trade-off over a lock-free CAS loop.
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

/// Map key: `caller = None` is the tool's global bucket.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct BucketKey {
    caller: Option<String>,
    tool: String,
}

/// Token-bucket rate limiter keyed by `(caller, tool)`.
pub struct RateLimiter {
    /// `per_sec` and `burst` resolved at construction — avoids a
    /// re-read on every `try_acquire`.
    refill_per_sec: f64,
    capacity: f64,
    max_buckets: usize,
    buckets: DashMap<BucketKey, Mutex<Bucket>>,
}

impl RateLimiter {
    /// Build a limiter from the operator-supplied config.
    #[must_use]
    pub fn new(cfg: &RateLimitConfig) -> Self {
        let refill_per_sec = f64::from(cfg.per_sec);
        // burst=0 falls back to per_sec — a single-knob config still works.
        let capacity = if cfg.burst == 0 {
            refill_per_sec
        } else {
            f64::from(cfg.burst)
        };
        Self {
            refill_per_sec,
            capacity,
            max_buckets: DEFAULT_MAX_BUCKETS,
            buckets: DashMap::new(),
        }
    }

    /// Override the bucket-map bound (default [`DEFAULT_MAX_BUCKETS`]).
    #[must_use]
    pub fn with_max_buckets(mut self, max_buckets: usize) -> Self {
        self.max_buckets = max_buckets.max(1);
        self
    }

    /// `true` when the limiter is disabled (`per_sec == 0`).
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        self.refill_per_sec <= 0.0
    }

    /// Number of live buckets (for tests and diagnostics).
    #[must_use]
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Try to consume one token from `tool`'s global bucket.
    pub fn try_acquire(&self, tool: &str) -> Result<(), RateLimitReject> {
        self.try_acquire_for(None, tool)
    }

    /// Try to consume one token for `tool` on behalf of `caller`.
    /// Returns `Ok()` when the call may proceed, `Err` when the
    /// bucket is dry. A disabled limiter always admits.
    pub fn try_acquire_for(&self, caller: Option<&str>, tool: &str) -> Result<(), RateLimitReject> {
        if self.is_disabled() {
            return Ok(());
        }
        let key = self.resolve_key(caller, tool);
        let bucket = self
            .buckets
            .entry(key)
            .or_insert_with(|| {
                Mutex::new(Bucket {
                    tokens: self.capacity,
                    last_refill: Instant::now(),
                })
            })
            .downgrade();
        // Mutex poison means another thread panicked while holding
        // this bucket. Rate limiting is a best-effort signal, so a
        // poisoned bucket admits the call rather than cascading the
        // panic through every request path.
        let Ok(mut b) = bucket.lock() else {
            return Ok(());
        };
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(b.last_refill).as_secs_f64();
        b.tokens = (b.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        b.last_refill = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            Err(RateLimitReject {
                tool: tool.to_owned(),
                caller: caller.map(str::to_owned),
                // Safe: `refill_per_sec` was constructed from `u32`.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                per_sec: self.refill_per_sec as u32,
            })
        }
    }

    /// Pick the bucket key, enforcing the map bound. An existing key
    /// is always used as-is; a new per-caller key is only admitted
    /// when there is room (after sweeping idle buckets), otherwise
    /// the caller shares the tool's global bucket.
    fn resolve_key(&self, caller: Option<&str>, tool: &str) -> BucketKey {
        let key = BucketKey {
            caller: caller.map(str::to_owned),
            tool: tool.to_owned(),
        };
        if key.caller.is_none() || self.buckets.contains_key(&key) {
            return key;
        }
        if self.buckets.len() >= self.max_buckets {
            self.sweep_idle();
        }
        if self.buckets.len() >= self.max_buckets {
            return BucketKey {
                caller: None,
                tool: key.tool,
            };
        }
        key
    }

    /// Drop every per-caller bucket that has been idle long enough to
    /// be full again — removing it loses nothing.
    fn sweep_idle(&self) {
        let full_after = self.capacity / self.refill_per_sec;
        let now = Instant::now();
        self.buckets.retain(|key, bucket| {
            if key.caller.is_none() {
                return true;
            }
            match bucket.get_mut() {
                Ok(b) => now.saturating_duration_since(b.last_refill).as_secs_f64() < full_after,
                Err(_) => false,
            }
        });
    }
}

/// Structured rejection so the adapter can translate into its own
/// error code (MCP: `ToolError::Forbidden`; audit: `rate_limited`).
#[derive(Debug, Clone)]
pub struct RateLimitReject {
    /// Tool whose bucket was dry.
    pub tool: String,
    /// Caller identity the bucket was keyed by, if any.
    pub caller: Option<String>,
    /// Sustained refill rate the caller exceeded.
    pub per_sec: u32,
}

impl std::fmt::Display for RateLimitReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "rate limited for tool `{}` ({} req/s)",
            self.tool, self.per_sec
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn disabled_limiter_always_admits() {
        let lim = RateLimiter::new(&RateLimitConfig::default());
        for _ in 0..1000 {
            lim.try_acquire("x").unwrap();
        }
        assert_eq!(lim.bucket_count(), 0);
    }

    #[test]
    fn burst_then_reject() {
        let lim = RateLimiter::new(&RateLimitConfig {
            per_sec: 1,
            burst: 3,
        });
        // Three back-to-back fit in the burst.
        lim.try_acquire("tool").unwrap();
        lim.try_acquire("tool").unwrap();
        lim.try_acquire("tool").unwrap();
        // Fourth exceeds capacity immediately.
        let err = lim.try_acquire("tool").unwrap_err();
        assert_eq!(err.tool, "tool");
        assert_eq!(err.per_sec, 1);
        assert!(err.caller.is_none());
    }

    #[test]
    fn refills_over_time() {
        let lim = RateLimiter::new(&RateLimitConfig {
            per_sec: 100,
            burst: 1,
        });
        lim.try_acquire("z").unwrap();
        assert!(lim.try_acquire("z").is_err());
        sleep(Duration::from_millis(20));
        // 100/s * 20ms = 2 tokens refilled, clamped to burst=1 — still ok.
        lim.try_acquire("z").unwrap();
    }

    #[test]
    fn per_tool_isolation() {
        let lim = RateLimiter::new(&RateLimitConfig {
            per_sec: 1,
            burst: 1,
        });
        lim.try_acquire("a").unwrap();
        // `b` has its own bucket — independent of `a` being empty.
        lim.try_acquire("b").unwrap();
        assert!(lim.try_acquire("a").is_err());
        assert!(lim.try_acquire("b").is_err());
    }

    #[test]
    fn per_caller_isolation() {
        let lim = RateLimiter::new(&RateLimitConfig {
            per_sec: 1,
            burst: 1,
        });
        lim.try_acquire_for(Some("10.0.0.1"), "health").unwrap();
        // A different caller has its own bucket.
        lim.try_acquire_for(Some("10.0.0.2"), "health").unwrap();
        // The global bucket is separate from both.
        lim.try_acquire_for(None, "health").unwrap();
        let err = lim.try_acquire_for(Some("10.0.0.1"), "health").unwrap_err();
        assert_eq!(err.caller.as_deref(), Some("10.0.0.1"));
        assert!(lim.try_acquire_for(Some("10.0.0.2"), "health").is_err());
        assert_eq!(lim.bucket_count(), 3);
    }

    #[test]
    fn bucket_map_is_bounded_and_falls_back_to_global() {
        let lim = RateLimiter::new(&RateLimitConfig {
            per_sec: 1,
            burst: 2,
        })
        .with_max_buckets(2);
        lim.try_acquire_for(Some("c1"), "t").unwrap();
        lim.try_acquire_for(Some("c2"), "t").unwrap();
        assert_eq!(lim.bucket_count(), 2);
        // Map full; nothing idle enough to sweep → c3 shares the
        // tool's global bucket instead of getting its own. That
        // shared bucket is itself one entry, so the map goes to 3
        // and stops there however many callers arrive.
        lim.try_acquire_for(Some("c3"), "t").unwrap();
        assert_eq!(lim.bucket_count(), 3);
        lim.try_acquire_for(Some("c4"), "t").unwrap();
        // c3 + c4 drained the shared global bucket (burst 2).
        assert!(lim.try_acquire_for(Some("c5"), "t").is_err());
        // Attackers minting ids cannot grow the map past the bound.
        for i in 0..100 {
            let _ = lim.try_acquire_for(Some(&format!("x{i}")), "t");
        }
        assert!(lim.bucket_count() <= 3);
    }

    #[test]
    fn sweep_reclaims_idle_buckets() {
        let lim = RateLimiter::new(&RateLimitConfig {
            per_sec: 1000,
            burst: 1,
        })
        .with_max_buckets(1);
        lim.try_acquire_for(Some("c1"), "t").unwrap();
        assert_eq!(lim.bucket_count(), 1);
        // 1 token at 1000/s is full again after 1 ms.
        sleep(Duration::from_millis(5));
        lim.try_acquire_for(Some("c2"), "t").unwrap();
        // c1 was swept to make room, so c2 got its own bucket.
        assert_eq!(lim.bucket_count(), 1);
        assert!(lim.try_acquire_for(Some("c2"), "t").is_err());
    }

    #[test]
    fn poisoned_bucket_admits_instead_of_panicking() {
        let lim = RateLimiter::new(&RateLimitConfig {
            per_sec: 1,
            burst: 1,
        });
        lim.try_acquire("p").unwrap();
        assert!(lim.try_acquire("p").is_err());
        // Poison the bucket by panicking while holding its lock.
        let key = BucketKey {
            caller: None,
            tool: "p".into(),
        };
        let entry = lim.buckets.get(&key).unwrap();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = entry.lock().unwrap();
            panic!("poison");
        }));
        drop(entry);
        // Documented behaviour: poison → allow.
        lim.try_acquire("p").unwrap();
    }
}
