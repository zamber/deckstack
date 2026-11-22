//! Host-side write throttling: a token-bucket rate limiter.
//!
//! The device caps image throughput at ~2.1 MB/s (`upstream/PROTOCOL.md:265`) and LED writes
//! at `/policy/led_rate_per_s` (default 30/s, `docs/contracts/streaming.md` §5); this limiter is
//! the driver-side hook for such caps, so a burst of writes is paced instead of queueing up
//! against a throttled device (ds-vdev's `--throttle-kbps` simulates the device-side cap).

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default LED write rate cap: 30 writes/s (`/policy/led_rate_per_s` default).
pub const DEFAULT_LED_RATE_PER_S: u32 = 30;

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    max: f64,
    refill_per_s: f64,
    last: Instant,
}

/// A token-bucket rate limiter. Cheap to share: all state is behind one [`Mutex`].
#[derive(Debug)]
pub struct RateLimiter {
    bucket: Mutex<Bucket>,
}

impl RateLimiter {
    /// A limiter allowing `per_second` acquisitions, with a burst of the same size.
    pub fn new(per_second: u32) -> Self {
        Self::with_burst(per_second, per_second)
    }

    /// A limiter allowing `per_second` acquisitions with a burst of `burst`.
    pub fn with_burst(per_second: u32, burst: u32) -> Self {
        assert!(per_second > 0 && burst > 0);
        RateLimiter {
            bucket: Mutex::new(Bucket {
                tokens: burst as f64,
                max: burst as f64,
                refill_per_s: per_second as f64,
                last: Instant::now(),
            }),
        }
    }

    /// The configured sustained rate.
    pub fn rate_per_s(&self) -> f64 {
        self.bucket
            .lock()
            .expect("rate limiter poisoned")
            .refill_per_s
    }

    fn refill(&self, b: &mut Bucket) {
        let now = Instant::now();
        let elapsed = now.duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * b.refill_per_s).min(b.max);
        b.last = now;
    }

    /// Take one token if one is available right now.
    pub fn try_acquire(&self) -> bool {
        let mut b = self.bucket.lock().expect("rate limiter poisoned");
        self.refill(&mut b);
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Take one token, sleeping until it is available. Returns how long it slept.
    pub fn acquire(&self) -> Duration {
        let mut b = self.bucket.lock().expect("rate limiter poisoned");
        self.refill(&mut b);
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            return Duration::ZERO;
        }
        let wait = Duration::from_secs_f64((1.0 - b.tokens) / b.refill_per_s);
        b.tokens = 0.0;
        drop(b);
        std::thread::sleep(wait);
        wait
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_then_refill() {
        let r = RateLimiter::with_burst(1000, 2);
        assert!(r.try_acquire());
        assert!(r.try_acquire());
        assert!(!r.try_acquire(), "burst of 2 is exhausted");
        std::thread::sleep(Duration::from_millis(5)); // refills ~5 tokens at 1000/s
        assert!(r.try_acquire());
    }

    #[test]
    fn acquire_paces_the_caller() {
        let r = RateLimiter::with_burst(200, 1); // 5 ms per token, no burst
        assert_eq!(r.acquire(), Duration::ZERO); // burst token
        let slept = r.acquire();
        assert!(slept >= Duration::from_millis(4), "slept {slept:?}");
        assert!(slept < Duration::from_millis(500), "slept {slept:?}");
    }

    #[test]
    fn default_led_rate_is_30_per_s() {
        assert_eq!(RateLimiter::new(DEFAULT_LED_RATE_PER_S).rate_per_s(), 30.0);
    }
}
