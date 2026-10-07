use std::time::{Duration, Instant};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory, IDXGIAdapter3, IDXGIFactory, DXGI_MEMORY_SEGMENT_GROUP_LOCAL,
};
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCollectQueryData, PdhGetFormattedCounterValue, PdhOpenQueryW,
    PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

#[derive(Clone, Default, Debug)]
pub struct HwStats {
    pub cpu_percent: f32,
    pub ram_used_mb: u64,
    pub ram_total_mb: u64,
    pub gpu_percent: Option<f32>,
    pub gpu_vram_used_mb: u64,
    pub gpu_vram_total_mb: u64,
    /// None = no vendor sensor wired yet (NVAPI/ADL gated later).
    pub gpu_temp_c: Option<f32>,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

struct PdhCounter {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
}

impl PdhCounter {
    fn open(path: &str) -> Option<Self> {
        unsafe {
            let mut query = PDH_HQUERY::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                return None;
            }
            let mut counter = PDH_HCOUNTER::default();
            let w = wide(path);
            if PdhAddEnglishCounterW(query, PCWSTR(w.as_ptr()), 0, &mut counter) != 0 {
                return None;
            }
            // First collect primes the counter; value becomes valid on next tick.
            let _ = PdhCollectQueryData(query);
            Some(Self { query, counter })
        }
    }

    fn read(&self) -> Option<f64> {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }
            let mut value = PDH_FMT_COUNTERVALUE::default();
            if PdhGetFormattedCounterValue(self.counter, PDH_FMT_DOUBLE, None, &mut value) != 0 {
                return None;
            }
            Some(value.Anonymous.doubleValue)
        }
    }
}

pub struct HwPoller {
    last: Instant,
    interval: Duration,
    cpu: Option<PdhCounter>,
    gpu: Option<PdhCounter>,
    cached: HwStats,
    warmup: u8,
}

impl HwPoller {
    pub fn new(interval_ms: u64) -> Self {
        let cpu = PdhCounter::open("\\Processor(_Total)\\% Processor Time");
        // Best-effort: present on Win10+ with GPU scheduler; absent on some configs.
        let gpu = PdhCounter::open("\\GPU Engine(*)\\Utilization Percentage");
        Self {
            last: Instant::now() - Duration::from_millis(interval_ms),
            interval: Duration::from_millis(interval_ms),
            cpu,
            gpu,
            cached: HwStats::default(),
            warmup: 0,
        }
    }

    pub fn update(&mut self) -> Option<HwStats> {
        if self.last.elapsed() < self.interval {
            return None;
        }
        self.last = Instant::now();
        // PDH's first sample after priming is unreliable; discard it.
        let fresh = self.warmup >= 1;
        self.warmup = self.warmup.saturating_add(1);
        if fresh {
            if let Some(v) = self.cpu.as_ref().and_then(|c| c.read()) {
                self.cached.cpu_percent = v.clamp(0.0, 100.0) as f32;
            }
            if let Some(g) = self.gpu.as_ref().and_then(|c| c.read()) {
                // Wildcard counter aggregates oddly; clamp and treat >0 as signal.
                self.cached.gpu_percent = Some(g.clamp(0.0, 100.0) as f32);
            }
        } else {
            let _ = self.cpu.as_ref().and_then(|c| c.read());
            let _ = self.gpu.as_ref().and_then(|c| c.read());
        }
        let (used, total) = ram_mb();
        self.cached.ram_used_mb = used;
        self.cached.ram_total_mb = total;
        let (vu, vt) = vram_mb();
        self.cached.gpu_vram_used_mb = vu;
        self.cached.gpu_vram_total_mb = vt;
        Some(self.cached.clone())
    }

    pub fn cached(&self) -> &HwStats {
        &self.cached
    }
}

fn ram_mb() -> (u64, u64) {
    unsafe {
        let mut st = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        if GlobalMemoryStatusEx(&mut st).is_err() {
            return (0, 0);
        }
        let total = st.ullTotalPhys / (1024 * 1024);
        let avail = st.ullAvailPhys / (1024 * 1024);
        (total.saturating_sub(avail), total)
    }
}

fn vram_mb() -> (u64, u64) {
    unsafe {
        let factory: IDXGIFactory = match CreateDXGIFactory() {
            Ok(f) => f,
            Err(_) => return (0, 0),
        };
        let adapter = match factory.EnumAdapters(0) {
            Ok(a) => a,
            Err(_) => return (0, 0),
        };
        // Prefer live usage via IDXGIAdapter3; fall back to dedicated memory size.
        if let Ok(a3) = adapter.cast::<IDXGIAdapter3>() {
            let mut info = std::mem::zeroed();
            if a3
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info)
                .is_ok()
            {
                let used = info.CurrentUsage / (1024 * 1024);
                let total = info.Budget / (1024 * 1024);
                if total > 0 {
                    return (used, total);
                }
            }
        }
        if let Ok(desc) = adapter.GetDesc() {
            let total = (desc.DedicatedVideoMemory / (1024 * 1024)) as u64;
            return (0, total);
        }
        (0, 0)
    }
}
