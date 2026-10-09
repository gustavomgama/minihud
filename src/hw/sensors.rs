//! Hardware stats sampled from LibreHardwareMonitor — the only external
//! hardware data source. Anything missing is `None` and prints as `--`,
//! never a guess.

use std::time::{Duration, Instant};

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
        let (Some(p), c) = (self.prev.as_ref(), &self.cached) else {
            return true;
        };
        (c.cpu_percent - p.cpu_percent).abs() > 2.0
            || opt_u64_changed(c.ram_used_mb, p.ram_used_mb, 32)
            || opt_changed(c.gpu_percent, p.gpu_percent, 2.0)
            || opt_changed(c.gpu_temp_c, p.gpu_temp_c, 0.5)
            || opt_changed(c.gpu_power_w, p.gpu_power_w, 1.0)
            || opt_changed(c.cpu_temp_c, p.cpu_temp_c, 0.5)
            || opt_changed(c.cpu_power_w, p.cpu_power_w, 1.0)
            || c.gpu_core_mhz != p.gpu_core_mhz
            || c.gpu_mem_mhz != p.gpu_mem_mhz
            || c.cpu_clock_mhz != p.cpu_clock_mhz
            || opt_u64_changed(c.gpu_vram_used_mb, p.gpu_vram_used_mb, 32)
    }
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
