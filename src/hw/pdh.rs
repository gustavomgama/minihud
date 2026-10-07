use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Default)]
pub struct HwStats {
    pub cpu_percent: f32,
    pub gpu_vram_used_mb: u64,
    pub gpu_vram_total_mb: u64,
    pub gpu_temp_c: f32,
}

pub struct HwPoller {
    stats: Arc<AtomicU32>,
    last: Instant,
    interval: Duration,
}

impl HwPoller {
    pub fn new(interval_ms: u64) -> Self {
        Self {
            stats: Arc::new(AtomicU32::default()),
            last: Instant::now() - Duration::from_millis(interval_ms),
            interval: Duration::from_millis(interval_ms),
        }
    }

    pub fn update(&mut self) -> Option<HwStats> {
        if self.last.elapsed() < self.interval {
            return None;
        }
        self.last = Instant::now();
        // Stub: real PDH/DXGI to be wired next; keep minimal
        Some(HwStats::default())
    }
}
