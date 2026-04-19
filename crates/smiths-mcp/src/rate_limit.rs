//! Shared token-bucket rate limiter for tool invocations.
//!
//! One bucket per tool name, lazily created. Refill is a pure
//! monotonic-clock calculation — no background task. Disabled by
//! default; operators opt in via `mcp.rate_limit` config.

use std::sync::Mutex;
use std::time::Instant;

use dashmap::DashMap;
use smiths_core::RateLimitConfig;

/// Bucket state. One-thread-per-bucket contention is low enough that
/// `std::sync::Mutex` is the right trade-off over a lock-free CAS loop.
struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

/// Token-bucket rate limiter keyed by tool name.
pub struct RateLimiter {
    /// `per_sec` and `burst` resolved at construction — avoids a
    /// re-read on every `try_acquire`.
    refill_per_sec: f64,
    capacity: f64,
    buckets: DashMap<String, Mutex<Bucket>>,
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
            buckets: DashMap::new(),
        }
    }

    /// `true` when the limiter is disabled (`per_sec == 0`).
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        self.refill_per_sec <= 0.0
    }

    /// Try to consume one token for `tool`. Returns `Ok(())` when the
    /// call may proceed, `Err(RateLimitReject)` when the bucket is dry.
    /// Disabled limiter always admits.
    pub fn try_acquire(&self, tool: &str) -> Result<(), RateLimitReject> {
        if self.is_disabled() {
            return Ok(());
        }
        let bucket = self
            .buckets
            .entry(tool.to_owned())
            .or_insert_with(|| {
                Mutex::new(Bucket {
                    tokens: self.capacity,
                    last_refill: Instant::now(),
                })
            })
            .downgrade();
        let mut b = bucket.lock().expect("rate limit bucket poisoned");
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
                // Safe: `refill_per_sec` was constructed from `u32`.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                per_sec: self.refill_per_sec as u32,
            })
        }
    }
}

/// Structured rejection so the adapter can translate into its own
/// error code (MCP: `ToolError::Forbidden`; audit: `rate_limited`).
#[derive(Debug, Clone)]
pub struct RateLimitReject {
    pub tool: String,
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
}
