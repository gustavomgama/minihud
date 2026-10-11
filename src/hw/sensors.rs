//! Hardware stats sampled from LibreHardwareMonitor — the only external
//! hardware data source. Anything missing is `None` and prints as `--`,
//! never a guess.

use std::time::{Duration, Instant};

use super::apply::apply;
use super::lhm::{LhmFeed, LhmSensor};

/// Noise thresholds: a field counts as unchanged when it moves by less than
/// this between polls. Named so the adaptive cadence reads as intent.
const LOAD_EPS_PCT: f32 = 2.0;
const TEMP_EPS_C: f32 = 0.5;
const POWER_EPS_W: f32 = 1.0;
const MEM_EPS_MB: u64 = 32;

/// A sample older than this is treated as missing (rows read `--`).
const SAMPLE_MAX_AGE_SECS: u64 = 3;
/// Consecutive quiet polls before the interval backs off toward idle.
const STABLE_POLLS: u8 = 4;

#[derive(Clone, Default, Debug)]
pub struct HwStats {
    pub cpu_percent: f32,
    pub cpu_temp_c: Option<f32>,
    pub cpu_power_w: Option<f32>,
    pub cpu_clock_mhz: Option<u32>,
    pub ram_used_mb: Option<u64>,
    pub ram_total_mb: Option<u64>,
    pub gpu_percent: Option<f32>,
    pub gpu_temp_c: Option<f32>,
    pub gpu_power_w: Option<f32>,
    pub gpu_core_mhz: Option<u32>,
    pub gpu_mem_mhz: Option<u32>,
    pub gpu_vram_used_mb: Option<u64>,
    pub gpu_vram_total_mb: Option<u64>,
    pub gpu_mem_used_mb: Option<u64>,
    pub gpu_d3d_dedicated_mb: Option<u64>,
    pub gpu_voltage_mv: Option<u32>,
    pub cpu_name: Option<String>,
    pub gpu_name: Option<String>,
}

/// A source of hardware samples. `HwPoller` depends on this, not on the
/// concrete `LhmFeed`, so the poll cadence is unit-testable with a fake.
pub trait SensorFeed: Send {
    fn latest(&self, max_age: Duration) -> Option<Vec<LhmSensor>>;
}

impl SensorFeed for LhmFeed {
    fn latest(&self, max_age: Duration) -> Option<Vec<LhmSensor>> {
        LhmFeed::latest(self, max_age)
    }
}

/// Polls the feed on an adaptive hardware cadence: anything changing beyond
/// noise resets to the active rate; `STABLE_POLLS` quiet polls in a row double
/// the interval up to the idle cap.
pub struct HwPoller {
    last: Instant,
    active: Duration,
    idle: Duration,
    next: Duration,
    stable_polls: u8,
    feed: Box<dyn SensorFeed>,
    cached: HwStats,
    prev: Option<HwStats>,
}

impl HwPoller {
    pub fn new(active_ms: u64, idle_ms: u64, bridge_ms: u64) -> Self {
        Self::with_feed(
            active_ms,
            idle_ms,
            Box::new(LhmFeed::start_with_poll(bridge_ms)),
        )
    }

    /// Build a poller over an arbitrary feed (used by tests).
    pub fn with_feed(active_ms: u64, idle_ms: u64, feed: Box<dyn SensorFeed>) -> Self {
        let active = Duration::from_millis(active_ms.max(1));
        Self {
            last: Instant::now() - active,
            next: active,
            stable_polls: 0,
            active,
            idle: Duration::from_millis(idle_ms.max(active_ms.max(1))),
            feed,
            cached: HwStats::default(),
            prev: None,
        }
    }

    /// Poll when due. Returns the current stats on a due poll, else `None`.
    pub fn update(&mut self) -> Option<HwStats> {
        if self.last.elapsed() < self.next {
            return None;
        }
        self.last = Instant::now();
        if let Some(sensors) = self.feed.latest(Duration::from_secs(SAMPLE_MAX_AGE_SECS)) {
            apply(&sensors, &mut self.cached);
        }
        if changed(self.prev.as_ref(), &self.cached) {
            self.next = self.active;
            self.stable_polls = 0;
        } else {
            self.stable_polls = self.stable_polls.saturating_add(1);
            if self.stable_polls >= STABLE_POLLS {
                self.next = (self.next * 2).min(self.idle);
                self.stable_polls = 0;
            }
        }
        self.prev = Some(self.cached.clone());
        Some(self.cached.clone())
    }

    pub fn cached(&self) -> &HwStats {
        &self.cached
    }

    /// How long until the next poll is due (`Duration::ZERO` when already due).
    /// The caller sleeps exactly this instead of a fixed slice, so an update is
    /// never delayed by up to one slice.
    pub fn due_in(&self) -> Duration {
        wait_until(Instant::now(), self.last, self.next)
    }
}

/// How long to wait before the next poll is due: the cadence minus the time
/// already elapsed since the last poll. Zero when already due. Pure, so the
/// scheduling math is unit-tested without sleeping.
pub(crate) fn wait_until(now: Instant, last: Instant, cadence: Duration) -> Duration {
    cadence.saturating_sub(now.saturating_duration_since(last))
}

/// True when `cur` moved beyond idle noise from `prev`. A `None` prev (the
/// first poll) always counts as changed. Pure so the noise thresholds are
/// unit-tested independently of the poller's timer and feed.
pub(crate) fn changed(prev: Option<&HwStats>, cur: &HwStats) -> bool {
    let Some(p) = prev else {
        return true;
    };
    scalar_changed(p, cur) || integer_changed(p, cur)
}

/// Continuous fields compared with an epsilon (they always jitter).
fn scalar_changed(p: &HwStats, c: &HwStats) -> bool {
    (c.cpu_percent - p.cpu_percent).abs() > LOAD_EPS_PCT
        || opt_u64_changed(c.ram_used_mb, p.ram_used_mb, MEM_EPS_MB)
        || opt_changed(c.gpu_percent, p.gpu_percent, LOAD_EPS_PCT)
        || opt_changed(c.gpu_temp_c, p.gpu_temp_c, TEMP_EPS_C)
        || opt_changed(c.gpu_power_w, p.gpu_power_w, POWER_EPS_W)
        || opt_changed(c.cpu_temp_c, p.cpu_temp_c, TEMP_EPS_C)
        || opt_changed(c.cpu_power_w, p.cpu_power_w, POWER_EPS_W)
}

/// Discrete fields compared exactly (a clock step is a real change).
fn integer_changed(p: &HwStats, c: &HwStats) -> bool {
    c.gpu_core_mhz != p.gpu_core_mhz
        || c.gpu_mem_mhz != p.gpu_mem_mhz
        || c.cpu_clock_mhz != p.cpu_clock_mhz
        || opt_u64_changed(c.gpu_vram_used_mb, p.gpu_vram_used_mb, MEM_EPS_MB)
}

fn opt_changed(a: Option<f32>, b: Option<f32>, eps: f32) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => (x - y).abs() > eps,
        (None, None) => false,
        _ => true,
    }
}

fn opt_u64_changed(a: Option<u64>, b: Option<u64>, eps: u64) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x.abs_diff(y) > eps,
        (None, None) => false,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sensor(ty: &str, name: &str, value: f64) -> LhmSensor {
        LhmSensor {
            hw: "Cpu:X".to_string(),
            sensor_type: ty.to_string(),
            name: name.to_string(),
            value,
        }
    }

    /// Feed that always returns the same sample.
    struct FixedFeed(Vec<LhmSensor>);
    impl SensorFeed for FixedFeed {
        fn latest(&self, _max_age: Duration) -> Option<Vec<LhmSensor>> {
            Some(self.0.clone())
        }
    }

    /// Feed that never has a sample.
    struct NoFeed;
    impl SensorFeed for NoFeed {
        fn latest(&self, _max_age: Duration) -> Option<Vec<LhmSensor>> {
            None
        }
    }

    #[test]
    fn opt_changed_flags_only_noise_above_epsilon() {
        assert!(!opt_changed(None, None, 1.0), "both absent is no change");
        assert!(opt_changed(Some(1.0), None, 1.0), "appearing is a change");
        assert!(
            opt_changed(None, Some(1.0), 1.0),
            "disappearing is a change"
        );
        assert!(!opt_changed(Some(10.0), Some(10.5), 1.0), "within epsilon");
        assert!(opt_changed(Some(10.0), Some(12.0), 1.0), "beyond epsilon");
    }

    #[test]
    fn opt_u64_changed_flags_only_noise_above_epsilon() {
        assert!(!opt_u64_changed(None, None, 32), "both absent is no change");
        assert!(
            opt_u64_changed(Some(100), None, 32),
            "appearing is a change"
        );
        assert!(!opt_u64_changed(Some(100), Some(120), 32), "within epsilon");
        assert!(opt_u64_changed(Some(100), Some(200), 32), "beyond epsilon");
    }

    #[test]
    fn changed_is_true_on_the_first_poll() {
        assert!(changed(None, &HwStats::default()));
    }

    #[test]
    fn changed_is_false_when_values_match() {
        let a = HwStats::default();
        assert!(!changed(Some(&a), &HwStats::default()));
    }

    #[test]
    fn changed_ignores_fluctuations_within_noise() {
        let a = HwStats {
            cpu_percent: 10.0,
            gpu_temp_c: Some(50.0),
            ..Default::default()
        };
        let b = HwStats {
            cpu_percent: 11.5,
            gpu_temp_c: Some(50.3),
            ..Default::default()
        };
        assert!(!changed(Some(&a), &b));
    }

    #[test]
    fn changed_flags_real_moves() {
        let a = HwStats {
            cpu_percent: 10.0,
            ..Default::default()
        };
        let b = HwStats {
            cpu_percent: 13.0,
            ..Default::default()
        };
        assert!(changed(Some(&a), &b), "cpu load moved past 2%");

        let a = HwStats {
            gpu_temp_c: Some(50.0),
            ..Default::default()
        };
        let b = HwStats {
            gpu_temp_c: Some(52.0),
            ..Default::default()
        };
        assert!(changed(Some(&a), &b), "gpu temp moved past 0.5C");

        let a = HwStats {
            gpu_core_mhz: Some(1900),
            ..Default::default()
        };
        let b = HwStats {
            gpu_core_mhz: Some(1901),
            ..Default::default()
        };
        assert!(changed(Some(&a), &b), "integer clocks compare exactly");
    }

    #[test]
    fn update_applies_the_feed_and_gates_by_cadence() {
        let feed = FixedFeed(vec![sensor("Load", "CPU Total", 42.0)]);
        let mut p = HwPoller::with_feed(200, 1000, Box::new(feed));
        assert!(p.update().is_some(), "first poll is due");
        assert_eq!(p.cached().cpu_percent, 42.0);
        assert!(p.update().is_none(), "an immediate second poll is not due");
    }

    #[test]
    fn update_without_a_sample_still_reports_cached() {
        let mut p = HwPoller::with_feed(200, 1000, Box::new(NoFeed));
        assert!(
            p.update().is_some(),
            "a due poll returns even with no sample"
        );
        assert_eq!(p.cached().cpu_percent, 0.0);
    }

    #[test]
    fn wait_until_returns_the_remaining_cadence_or_zero() {
        // Catches: a fixed sleep slice quantizing every update (up to a whole
        // slice of added latency) or a zero/negative wait busy-spinning.
        let now = Instant::now();
        assert_eq!(
            wait_until(now, now, Duration::from_millis(100)),
            Duration::from_millis(100),
            "no time elapsed -> wait the whole cadence"
        );
        let last = now - Duration::from_millis(30);
        assert_eq!(
            wait_until(now, last, Duration::from_millis(100)),
            Duration::from_millis(70),
            "30ms into a 100ms cadence -> 70ms left"
        );
        let overdue = now - Duration::from_millis(150);
        assert_eq!(
            wait_until(now, overdue, Duration::from_millis(100)),
            Duration::ZERO,
            "overdue -> do not sleep"
        );
    }

    #[test]
    fn due_in_is_zero_right_after_a_due_poll() {
        // Catches: the poller reporting a stale/zero wait so the loop either
        // busy-spins or sleeps past the next due poll.
        let feed = FixedFeed(vec![sensor("Load", "CPU Total", 42.0)]);
        let mut p = HwPoller::with_feed(200, 1000, Box::new(feed));
        assert!(p.update().is_some());
        let d = p.due_in();
        assert!(d > Duration::ZERO, "after a poll there is a wait: {d:?}");
        assert!(d <= Duration::from_millis(200), "within the cadence: {d:?}");
    }
}
