//! Throttle and backpressure knobs of the virtual device (DS-035).
//!
//! Everything the virtual device does to *slow itself down* lives here, so the semantics
//! are pinned in one place and unit-testable without a socket:
//!
//! - [`Throttle`] — the `--throttle-kbps` write cap (acks are delayed by the time the real
//!   ~2.1 MB/s device would need, `upstream/PROTOCOL.md:265`) and `--drop-after N`
//!   (hot-unplug: the connection that has accepted more than N writes is closed without an
//!   ack for the write that exceeds the budget);
//! - [`CreditWindow`] / [`CreditState`] — the credit-exhaustion mode: the device grants a
//!   bucket of write credits, each accepted output report consumes one, and when the bucket
//!   is empty the ack is held back until the next refill (writes *wait* — backpressure —
//!   instead of being dropped or buffered without bound). The bucket refills on a fixed
//!   schedule derived from a seed, so a run is deterministic: the same seed and the same
//!   write timeline produce the same hold durations.
//!
//! The hub (`hub.rs`) applies these knobs per request frame; the control socket exposes
//! them through the existing `throttle` command (`ds-vdevctl … throttle <kbps|off>
//! [drop-after|off] [credits=<burst>/<refill>/<period_ms>[@<seed>]|off]`) and the Rust API
//! ([`crate::Hub::set_credits`]). Semantics are documented in
//! `docs/components/virtual-device.md`.

use std::time::Duration;

/// 1 kbit/s in bytes/s (1 kbit/s = 1000 bit/s = 125 B/s).
pub fn kbps_to_bytes_per_s(kbps: u32) -> f64 {
    f64::from(kbps) * 125.0
}

/// Artificial device-side slowdown knobs (bench/conformance; DS-033/DS-035).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Throttle {
    /// Simulated device write cap in kbit/s (`--throttle-kbps`); `None` = unlimited.
    pub kbps: Option<u32>,
    /// Simulate hot-unplug after this many accepted writes on one connection
    /// (`--drop-after`); `None` = never.
    pub drop_after: Option<u32>,
}

impl Throttle {
    /// No throttling: deterministic by default.
    pub fn none() -> Self {
        Throttle {
            kbps: None,
            drop_after: None,
        }
    }

    /// How long the device takes to accept `bytes` bytes at the configured cap
    /// (~2.1 MB/s is the real cap, `upstream/PROTOCOL.md:265`). `kbps = 0` means unlimited.
    pub fn delay_for(&self, bytes: usize) -> Duration {
        match self.kbps {
            Some(kbps) if kbps > 0 => {
                Duration::from_secs_f64(bytes as f64 / kbps_to_bytes_per_s(kbps))
            }
            _ => Duration::ZERO,
        }
    }

    /// Whether a connection that has already accepted `accepted` writes must be dropped
    /// (`--drop-after`): the write with `accepted > n` is the one that gets no ack and
    /// closes the connection (the already-accepted writes keep their acks).
    pub fn drop_exceeded(&self, accepted: u64) -> bool {
        match self.drop_after {
            Some(n) => accepted > u64::from(n),
            None => false,
        }
    }
}

/// A deterministic credit window: the device-side receive budget in credit-exhaustion mode.
///
/// The bucket starts full (`burst` credits); every accepted **output** write consumes one.
/// When the bucket is empty, the ack for the next write is held until the next refill
/// (granting `refill` credits, capped at `burst`), so the sender waits instead of piling up
/// unbounded work. Refills happen every `refill_period`; with a non-zero `seed` each
/// refill window gets a deterministic jitter of up to `refill_period / 8`, so a seeded run
/// is still exactly reproducible (same seed + same write timeline ⇒ same hold durations).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreditWindow {
    /// Credits in a full bucket (the initial grant).
    pub burst: u32,
    /// Credits granted per refill.
    pub refill: u32,
    /// Base time between refills.
    pub refill_period: Duration,
    /// Seed for the deterministic per-window jitter (`0` = fixed schedule, no jitter).
    pub seed: u64,
}

impl CreditWindow {
    /// A fixed window with no jitter: refills are exactly `refill_period` apart.
    pub fn fixed(burst: u32, refill: u32, refill_period: Duration) -> Self {
        CreditWindow {
            burst,
            refill,
            refill_period,
            seed: 0,
        }
    }

    /// A seeded window: same seed ⇒ identical refill schedule for the same timeline.
    pub fn seeded(burst: u32, refill: u32, refill_period: Duration, seed: u64) -> Self {
        CreditWindow {
            burst,
            refill,
            refill_period,
            seed,
        }
    }

    /// Parse the control-socket spelling `credits=<burst>/<refill>/<period_ms>[@<seed>]`
    /// (e.g. `credits=8/8/250` or `credits=8/8/250@42`).
    pub fn parse(spec: &str) -> Result<CreditWindow, String> {
        let spec = spec.strip_prefix("credits=").unwrap_or(spec);
        let (main, seed) = match spec.split_once('@') {
            Some((m, s)) => (
                m,
                Some(
                    s.parse::<u64>()
                        .map_err(|_| format!("bad credit seed '{s}'"))?,
                ),
            ),
            None => (spec, None),
        };
        let mut parts = main.split('/');
        let burst = parts
            .next()
            .ok_or("credits need <burst>/<refill>/<period_ms>")?
            .parse::<u32>()
            .map_err(|_| format!("bad credit burst '{main}'"))?;
        let refill = parts
            .next()
            .ok_or("credits need <burst>/<refill>/<period_ms>")?
            .parse::<u32>()
            .map_err(|_| format!("bad credit refill '{main}'"))?;
        let period_ms = parts
            .next()
            .ok_or("credits need <burst>/<refill>/<period_ms>")?
            .parse::<u64>()
            .map_err(|_| format!("bad credit period_ms '{main}'"))?;
        if parts.next().is_some() {
            return Err(format!("bad credits spec '{spec}'"));
        }
        if burst == 0 || refill == 0 || period_ms == 0 {
            return Err("credit burst/refill/period_ms must be > 0".to_string());
        }
        Ok(match seed {
            Some(seed) => {
                CreditWindow::seeded(burst, refill, Duration::from_millis(period_ms), seed)
            }
            None => CreditWindow::fixed(burst, refill, Duration::from_millis(period_ms)),
        })
    }

    /// The effective refill period of window `n`: the base period plus a deterministic
    /// jitter derived from the seed (no jitter when `seed == 0` or for window 0).
    fn period_for(&self, n: u64) -> Duration {
        if self.seed == 0 || n == 0 {
            return self.refill_period;
        }
        let mut s = self.seed ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let span = self.refill_period.as_nanos() / 8 + 1;
        let jitter_ns = (s as u128 % span) as u64;
        self.refill_period + Duration::from_nanos(jitter_ns)
    }
}

/// The mutable state of one [`CreditWindow`], driven by a logical clock (a `Duration` since
/// the hub started) so the whole schedule is a pure function of the write timeline.
#[derive(Debug, Clone)]
pub struct CreditState {
    window: CreditWindow,
    /// Credits available, possibly negative: a held write reserves one credit from the
    /// refill that ends its hold (token bucket with debt, so holds never chain).
    remaining: i64,
    next_refill: Duration,
    windows: u64,
}

impl CreditState {
    /// A fresh bucket at `now`, full (`burst` credits), first refill after one period.
    pub fn new(window: CreditWindow, now: Duration) -> Self {
        CreditState {
            window,
            remaining: i64::from(window.burst),
            next_refill: now + window.period_for(0),
            windows: 0,
        }
    }

    /// The window this state belongs to.
    pub fn window(&self) -> CreditWindow {
        self.window
    }

    /// Credits currently in the bucket (0 when the bucket is empty or over-reserved).
    pub fn remaining(&self) -> u32 {
        self.remaining.max(0) as u32
    }

    /// Grant credits for every refill window that has elapsed by `now` (capped at `burst`).
    /// The refill schedule itself only ever advances with the clock, never with a write.
    fn advance(&mut self, now: Duration) {
        while now >= self.next_refill {
            self.remaining =
                (self.remaining + i64::from(self.window.refill)).min(i64::from(self.window.burst));
            self.windows += 1;
            self.next_refill += self.window.period_for(self.windows);
        }
    }

    /// Register one accepted output write at `now`. Returns how long the device holds the
    /// ack back: [`Duration::ZERO`] when a credit was in the bucket, otherwise the wait
    /// until the next refill (always shorter than one refill period); the write reserves
    /// one credit from that refill.
    pub fn take(&mut self, now: Duration) -> Duration {
        self.advance(now);
        if self.remaining > 0 {
            self.remaining -= 1;
            return Duration::ZERO;
        }
        let hold = self.next_refill.saturating_sub(now);
        self.remaining -= 1;
        hold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kbps_shaping_math_is_exact() {
        let t = Throttle {
            kbps: Some(64),
            drop_after: None,
        };
        // 64 kbit/s = 8000 B/s: 320 B takes 40 ms, 8 B takes 1 ms
        assert_eq!(t.delay_for(320), Duration::from_millis(40));
        assert_eq!(t.delay_for(8), Duration::from_millis(1));
        assert_eq!(t.delay_for(0), Duration::ZERO);
        // the ~2.1 MB/s real cap is ~16800 kbit/s
        let cap = Throttle {
            kbps: Some(16_800),
            drop_after: None,
        };
        assert!(cap.delay_for(2_100_000) >= Duration::from_secs(1));
        // 0 and None mean unlimited
        assert_eq!(
            Throttle {
                kbps: Some(0),
                drop_after: None
            }
            .delay_for(1_000_000),
            Duration::ZERO
        );
        assert_eq!(Throttle::none().delay_for(1_000_000), Duration::ZERO);
    }

    #[test]
    fn throttle_kbps_matches_wall_clock_bytes_per_second() {
        // 800 kbit/s = 100 B/ms: 40 writes of 320 B must shape to 40 * 3.2 ms = 128 ms
        let t = Throttle {
            kbps: Some(800),
            drop_after: None,
        };
        let writes = 40;
        let bytes = 320;
        let total: Duration = (0..writes).map(|_| t.delay_for(bytes)).sum();
        assert_eq!(total, Duration::from_millis(128));
        // wall clock: sleeping the shaped delays really takes that long
        let start = std::time::Instant::now();
        for _ in 0..writes {
            std::thread::sleep(t.delay_for(bytes));
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(120),
            "shaped {elapsed:?} < 120 ms for {total:?} of delays"
        );
        assert!(
            elapsed <= Duration::from_millis(2_000),
            "shaped {elapsed:?} absurdly long for {total:?}"
        );
    }

    #[test]
    fn drop_after_counts_accepted_writes() {
        let t = Throttle {
            kbps: None,
            drop_after: Some(2),
        };
        assert!(!t.drop_exceeded(0));
        assert!(!t.drop_exceeded(1));
        assert!(!t.drop_exceeded(2));
        assert!(
            t.drop_exceeded(3),
            "the 3rd write on a budget of 2 is the unplug"
        );
        assert!(!Throttle::none().drop_exceeded(u64::MAX));
    }

    #[test]
    fn credit_window_hold_schedule_is_deterministic() {
        let w = CreditWindow::fixed(2, 2, Duration::from_millis(100));
        let mut c = CreditState::new(w, Duration::ZERO);
        assert_eq!(c.remaining(), 2);
        // two writes fit the burst
        assert_eq!(c.take(Duration::ZERO), Duration::ZERO);
        assert_eq!(c.take(Duration::from_millis(10)), Duration::ZERO);
        assert_eq!(c.remaining(), 0);
        // the third is held until the first refill at t=100 ms
        assert_eq!(c.take(Duration::from_millis(20)), Duration::from_millis(80));
        // charged against that refill: one credit left right after it
        assert_eq!(c.take(Duration::from_millis(100)), Duration::ZERO);
        assert_eq!(c.remaining(), 0);
        // empty again -> held until t=200 ms
        assert_eq!(
            c.take(Duration::from_millis(110)),
            Duration::from_millis(90)
        );
        // and a long idle gap refills the whole bucket (capped at burst)
        assert_eq!(c.take(Duration::from_millis(10_000)), Duration::ZERO);
        assert_eq!(c.remaining(), 1);
        assert_eq!(c.take(Duration::from_millis(10_000)), Duration::ZERO);
        assert_eq!(c.remaining(), 0);
    }

    #[test]
    fn credit_seed_reproduces_the_same_jittered_schedule() {
        let w = CreditWindow::seeded(1, 1, Duration::from_millis(1_000), 42);
        let mut a = CreditState::new(w, Duration::ZERO);
        let mut b = CreditState::new(w, Duration::ZERO);
        // same seed, same write timeline -> identical holds (and jitter stays within 1/8 period)
        for ms in [0u64, 500, 1_000, 1_500, 2_000, 5_000, 9_999] {
            let now = Duration::from_millis(ms);
            let ha = a.take(now);
            let hb = b.take(now);
            assert_eq!(ha, hb, "seed 42 diverged at t={ms} ms");
            assert!(ha <= w.refill_period + w.refill_period / 8 + Duration::from_nanos(1));
        }
        // window 0 has no jitter; later windows may differ from the base period
        let mut c = CreditState::new(w, Duration::ZERO);
        assert_eq!(c.take(Duration::ZERO), Duration::ZERO);
        let held = c.take(Duration::from_millis(1));
        assert!(held >= Duration::from_millis(999) && held <= Duration::from_millis(1_125));
    }

    #[test]
    fn credit_window_parse_round_trip() {
        let w = CreditWindow::parse("credits=8/8/250").unwrap();
        assert_eq!(w, CreditWindow::fixed(8, 8, Duration::from_millis(250)));
        let w = CreditWindow::parse("credits=3/2/150@7").unwrap();
        assert_eq!(w, CreditWindow::seeded(3, 2, Duration::from_millis(150), 7));
        assert_eq!(CreditWindow::parse("8/8/250").unwrap().burst, 8);
        assert!(CreditWindow::parse("credits=0/8/250").is_err());
        assert!(CreditWindow::parse("credits=8/8/0").is_err());
        assert!(CreditWindow::parse("credits=8/8").is_err());
        assert!(CreditWindow::parse("credits=8/8/250@x").is_err());
        assert!(CreditWindow::parse("credits=8/8/250/9").is_err());
    }
}
