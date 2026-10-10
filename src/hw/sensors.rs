//! Hardware stats sampled from LibreHardwareMonitor — the only external
//! hardware data source. Anything missing is `None` and prints as `--`,
//! never a guess.

use std::time::{Duration, Instant};

/// Noise thresholds: a field counts as unchanged when it moves by less than
/// this between polls. Named so the adaptive cadence reads as intent.
const LOAD_EPS_PCT: f32 = 2.0;
const TEMP_EPS_C: f32 = 0.5;
const POWER_EPS_W: f32 = 1.0;
const MEM_EPS_MB: u64 = 32;

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

/// Polls the LHM bridge on an adaptive hardware cadence: anything changing
/// beyond noise resets to the active rate; 4 quiet polls in a row double the
/// interval up to the idle cap.
pub struct HwPoller {
    last: Instant,
    active: Duration,
    idle: Duration,
    next: Duration,
    stable_polls: u8,
    lhm: super::lhm::LhmFeed,
    cached: HwStats,
    prev: Option<HwStats>,
}

impl HwPoller {
    pub fn new(active_ms: u64, idle_ms: u64) -> Self {
        let active = Duration::from_millis(active_ms.max(1));
        Self {
            last: Instant::now() - active,
            next: active,
            stable_polls: 0,
            active,
            idle: Duration::from_millis(idle_ms.max(active_ms.max(1))),
            lhm: super::lhm::LhmFeed::start(),
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
        if let Some(sensors) = self.lhm.latest(Duration::from_secs(3)) {
            super::lhm::apply(&sensors, &mut self.cached);
        }
        if self.changed_since_last() {
            self.next = self.active;
            self.stable_polls = 0;
        } else {
            self.stable_polls = self.stable_polls.saturating_add(1);
            if self.stable_polls >= 4 {
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

    /// True when the latest poll moved anything beyond idle noise.
    fn changed_since_last(&self) -> bool {
        changed(self.prev.as_ref(), &self.cached)
    }
}

/// True when `cur` moved beyond idle noise from `prev`. A `None` prev (the
/// first poll) always counts as changed. Pure so the noise thresholds are
/// unit-tested independently of the poller's timer and feed.
pub(crate) fn changed(prev: Option<&HwStats>, cur: &HwStats) -> bool {
    let Some(p) = prev else {
        return true;
    };
    (cur.cpu_percent - p.cpu_percent).abs() > LOAD_EPS_PCT
        || opt_u64_changed(cur.ram_used_mb, p.ram_used_mb, MEM_EPS_MB)
        || opt_changed(cur.gpu_percent, p.gpu_percent, LOAD_EPS_PCT)
        || opt_changed(cur.gpu_temp_c, p.gpu_temp_c, TEMP_EPS_C)
        || opt_changed(cur.gpu_power_w, p.gpu_power_w, POWER_EPS_W)
        || opt_changed(cur.cpu_temp_c, p.cpu_temp_c, TEMP_EPS_C)
        || opt_changed(cur.cpu_power_w, p.cpu_power_w, POWER_EPS_W)
        || cur.gpu_core_mhz != p.gpu_core_mhz
        || cur.gpu_mem_mhz != p.gpu_mem_mhz
        || cur.cpu_clock_mhz != p.cpu_clock_mhz
        || opt_u64_changed(cur.gpu_vram_used_mb, p.gpu_vram_used_mb, MEM_EPS_MB)
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
}
