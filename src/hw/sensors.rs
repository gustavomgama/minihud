//! The ONLY hardware data path: LibreHardwareMonitor.
//! PDH, NVML, DXGI VRAM, CallNt clocks and GlobalMemoryStatus are gone by
//! decision. Anything LHM lacks shows "--", never a guess. Unknown keys
//! in old configs are ignored, so this removal breaks no toml files.

use std::time::{Duration, Instant};

#[derive(Clone, Default, Debug)]
pub struct HwStats {
    pub cpu_percent: f32,
    pub cpu_temp_c: Option<f32>,
    pub cpu_power_w: Option<f32>,
    pub ram_used_mb: Option<u64>,
    pub ram_total_mb: Option<u64>,
    pub gpu_percent: Option<f32>,
    pub gpu_temp_c: Option<f32>,
    pub gpu_power_w: Option<f32>,
    pub gpu_core_mhz: Option<u32>,
    pub gpu_mem_mhz: Option<u32>,
    pub gpu_vram_used_mb: Option<u64>,
    pub gpu_vram_total_mb: Option<u64>,
}

pub struct HwPoller {
    last: Instant,
    active: Duration,
    idle: Duration,
    next: Duration,
    stable_polls: u8,
    lhm: super::lhm::LhmFeed,
    cached: HwStats,
    prev: Option<HwStats>,
    sampled: bool,
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
            sampled: false,
        }
    }

    /// Poll when due. Adaptive cadence: anything changing beyond noise
    /// resets to the active rate (floor: the configured minimum);
    /// 4 quiet polls in a row double the interval up to the idle cap.
    /// Returns Some on every poll; use `ready()` to know whether any
    /// real sample has landed yet (LHM bridge takes seconds to warm up).
    pub fn update(&mut self) -> Option<HwStats> {
        if self.last.elapsed() < self.next {
            return None;
        }
        self.last = Instant::now();
        if let Some(sensors) = self.lhm.latest(Duration::from_secs(3)) {
            super::lhm::apply(&sensors, &mut self.cached);
            self.sampled = true;
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

    /// True once at least one LHM sample has been applied. Before that
    /// every row reads "--" instead of startup zeros.
    pub fn ready(&self) -> bool {
        self.sampled
    }

    pub fn cached(&self) -> &HwStats {
        &self.cached
    }

    /// Current poll interval (for logs): active when moving, up to idle.
    pub fn interval_ms(&self) -> u64 {
        self.next.as_millis() as u64
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
