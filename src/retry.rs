//! Bounded, jittered backoff and failure-report discipline shared by the
//! sidecar feed supervisors (`fps::presentmon`, `hw::lhm`), plus the
//! process-wide retry counters the `--stats` line surfaces so a wedged
//! dependency stays visible instead of silent.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Largest jitter added to or removed from a fixed retry backoff. Independent
/// feeds that fail at the same moment must not retry in lockstep.
pub const MAX_JITTER_MS: u64 = 2000;

/// Consecutive failures between periodic "still retrying" info lines (roughly
/// one minute at the 5 s backoff) — enough to keep a wedged dependency visible
/// without spamming the log.
pub const REPORT_EVERY: u64 = 12;

/// Map a uniform fraction in `[0, 1]` onto `base ± max_jitter`, clamped at zero.
/// Pure, so the jitter bounds are unit-tested without any randomness.
pub fn jittered(base: Duration, max_jitter: Duration, frac: f64) -> Duration {
    let span = max_jitter.as_millis() as f64;
    let delta = (frac.clamp(0.0, 1.0) * 2.0 - 1.0) * span; // [-span, +span]
    Duration::from_millis((base.as_millis() as f64 + delta).max(0.0) as u64)
}

/// A tiny seedable PRNG (splitmix64) so backoff jitter is deterministic in
/// tests and decorrelated between feeds at runtime.
pub struct Jitter {
    state: u64,
}

impl Jitter {
    /// Seed the generator; any seed (zero included) yields a valid stream.
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The next uniform fraction in `[0, 1)`.
    pub fn frac(&mut self) -> f64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64
    }

    /// The next jittered backoff around `base` (± `max_jitter`).
    pub fn next(&mut self, base: Duration, max_jitter: Duration) -> Duration {
        jittered(base, max_jitter, self.frac())
    }
}

/// A per-feed jitter seed: the clock nanos and the pid, so two feeds (and two
/// runs) do not jitter in lockstep. Pure, so it is unit-tested directly.
pub fn seed(now_nanos: u64, pid: u32) -> u64 {
    now_nanos ^ (pid as u64).rotate_left(17)
}

/// A fresh jitter seed for `pid` from the wall clock.
pub fn seed_now(pid: u32) -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    seed(nanos, pid)
}

/// How loudly to report the `n`th consecutive session failure (1-based).
#[derive(Debug, PartialEq, Eq)]
pub enum FailureLog {
    /// The first failure — a loud warning that a new dependency just broke.
    Warn,
    /// A periodic info so a long-wedged dependency stays visible.
    Info,
    /// Routine retries — debug only, never spam.
    Debug,
}

/// The log level for the `n`th consecutive failure: the first is loud, every
/// [`REPORT_EVERY`]th after that is a periodic info, the rest are debug.
pub fn failure_log(n: u64) -> FailureLog {
    if n <= 1 {
        FailureLog::Warn
    } else if n.is_multiple_of(REPORT_EVERY) {
        FailureLog::Info
    } else {
        FailureLog::Debug
    }
}

/// Emit a feed session failure at the discipline level ([`failure_log`]): warn
/// on the first, a periodic info while the dependency stays wedged, debug
/// otherwise — so a broken dependency is seen but a retry storm is not spammed.
/// The level selection is the tested part; this is the tracing glue.
pub fn emit_failure(feed: &str, err: &str, wait: Duration, n: u64, total: u64) {
    let msg = format!(
        "{feed}: {err}; retry in {:.1}s (failures {total})",
        wait.as_secs_f64()
    );
    match failure_log(n) {
        FailureLog::Warn => tracing::warn!("{msg}"),
        FailureLog::Info => tracing::info!("{msg}"),
        FailureLog::Debug => tracing::debug!("{msg}"),
    }
}

/// A process-wide monotonic failure counter for one feed.
#[derive(Default)]
pub struct RetryCounter(AtomicU64);

impl RetryCounter {
    /// A zeroed counter, usable in a `static`.
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// Record one failure; returns the new total.
    pub fn bump(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The total recorded so far.
    pub fn total(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// PresentMon (ETW fps) session failures/retries.
pub static FPS_RETRIES: RetryCounter = RetryCounter::new();
/// LibreHardwareMonitor bridge session failures/retries.
pub static HW_RETRIES: RetryCounter = RetryCounter::new();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jittered_spans_plus_minus_the_bound_and_clamps_at_zero() {
        // Catches: a jitter that only ever adds delay, or one that underflows a
        // small base into a huge unsigned duration.
        let base = Duration::from_millis(5000);
        let max = Duration::from_millis(2000);
        assert_eq!(jittered(base, max, 0.0), Duration::from_millis(3000));
        assert_eq!(jittered(base, max, 0.5), Duration::from_millis(5000));
        assert_eq!(jittered(base, max, 1.0), Duration::from_millis(7000));
        assert_eq!(
            jittered(Duration::from_millis(500), max, 0.0),
            Duration::ZERO,
            "a base below the jitter span must clamp to zero, not wrap"
        );
    }

    #[test]
    fn jittered_stays_within_the_bound_for_every_fraction() {
        // Catches: an out-of-range fraction producing a delay outside the
        // intended band (an unbounded backoff).
        let base = Duration::from_millis(5000);
        let max = Duration::from_millis(2000);
        let lo = Duration::from_millis(3000);
        let hi = Duration::from_millis(7000);
        for i in 0..=100 {
            let d = jittered(base, max, i as f64 / 100.0);
            assert!((lo..=hi).contains(&d), "frac {i}% -> {d:?} out of band");
        }
    }

    #[test]
    fn jitter_is_deterministic_per_seed_and_actually_varies() {
        // Catches: a jitter that ignores the seed (all feeds back off in
        // lockstep) or a constant generator (no jitter at all).
        let mut a = Jitter::new(1);
        let mut b = Jitter::new(1);
        let first = a.frac();
        assert_eq!(first, b.frac(), "the same seed must replay the same stream");
        assert!(
            (0.0..1.0).contains(&first),
            "frac must be in [0,1): {first}"
        );
        assert_ne!(
            a.frac(),
            a.frac(),
            "successive fractions must differ (not a constant)"
        );
    }

    #[test]
    fn jitter_next_is_bounded() {
        // Catches: `next` computing a duration outside the ± bound.
        let base = Duration::from_millis(5000);
        let max = Duration::from_millis(MAX_JITTER_MS);
        let mut j = Jitter::new(42);
        for _ in 0..1000 {
            let d = j.next(base, max);
            assert!(
                (Duration::from_millis(3000)..=Duration::from_millis(7000)).contains(&d),
                "backoff {d:?} outside 3..=7s"
            );
        }
    }

    #[test]
    fn seed_decorrelates_feeds_with_the_same_time() {
        // Catches: two feeds sharing a seed (identical retry timing) or a seed
        // that ignores the pid.
        assert_ne!(seed(123, 1), seed(123, 2), "pid must change the seed");
        assert_ne!(seed(1, 7), seed(2, 7), "time must change the seed");
    }

    #[test]
    fn failure_log_is_loud_once_then_periodic_then_quiet() {
        // Catches: logging every retry (spam) or never re-reporting a wedged
        // dependency (silent after the first warning).
        assert_eq!(failure_log(1), FailureLog::Warn, "the first is loud");
        assert_eq!(
            failure_log(2),
            FailureLog::Debug,
            "routine retries are quiet"
        );
        assert_eq!(failure_log(REPORT_EVERY - 1), FailureLog::Debug);
        assert_eq!(
            failure_log(REPORT_EVERY),
            FailureLog::Info,
            "a wedged dependency must resurface periodically"
        );
        assert_eq!(failure_log(REPORT_EVERY + 1), FailureLog::Debug);
        assert_eq!(failure_log(2 * REPORT_EVERY), FailureLog::Info);
    }

    #[test]
    fn retry_counter_counts_up() {
        // Catches: a counter that does not accumulate (the `--stats` line would
        // always report zero retries).
        let c = RetryCounter::new();
        assert_eq!(c.total(), 0);
        assert_eq!(c.bump(), 1);
        assert_eq!(c.bump(), 2);
        assert_eq!(c.total(), 2);
    }

    #[test]
    fn emit_failure_covers_every_level_without_panicking() {
        // Catches: an arm of the failure-report dispatch panicking, which would
        // abort a supervisor thread on a routine retry. tracing with no
        // subscriber is a safe no-op, so all three arms run deterministically.
        let wait = Duration::from_secs(5);
        emit_failure("fps: presentmon", "boom", wait, 1, 1); // warn
        emit_failure("fps: presentmon", "boom", wait, 2, 2); // debug
        emit_failure("fps: presentmon", "boom", wait, REPORT_EVERY, REPORT_EVERY);
        // info
    }
}
