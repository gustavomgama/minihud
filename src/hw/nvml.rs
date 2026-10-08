//! NVIDIA telemetry via NVML (temp, power, clocks, utilization, VRAM).
//! Single `nvml-wrapper` dependency, DLL loaded lazily at first poll.
//! No NVIDIA driver -> init fails once and every reading stays None,
//! leaving the PDH/DXGI fallbacks in charge. Voltage (mV) has no NVML
//! API (that's NVAPI territory) and is intentionally not faked.

use std::sync::{Mutex, OnceLock};

#[derive(Clone, Debug, Default)]
pub struct NvmlGpu {
    pub temp_c: Option<f32>,
    pub power_w: Option<f32>,
    pub core_mhz: Option<u32>,
    pub mem_mhz: Option<u32>,
    pub util_percent: Option<f32>,
    pub vram_used_mb: Option<u64>,
    pub vram_total_mb: Option<u64>,
}

static NVML: OnceLock<Option<Mutex<nvml_wrapper::Nvml>>> = OnceLock::new();

fn nvml() -> Option<std::sync::MutexGuard<'static, nvml_wrapper::Nvml>> {
    let m = NVML.get_or_init(|| nvml_wrapper::Nvml::init().ok().map(Mutex::new));
    m.as_ref()?.lock().ok()
}

/// One best-effort poll; any failing reading stays None.
pub fn poll() -> NvmlGpu {
    use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
    let mut out = NvmlGpu::default();
    let guard = match nvml() {
        Some(g) => g,
        None => return out,
    };
    let dev = match guard.device_by_index(0) {
        Ok(d) => d,
        Err(_) => return out,
    };
    if let Ok(t) = dev.temperature(TemperatureSensor::Gpu) {
        out.temp_c = Some(t as f32);
    }
    if let Ok(mw) = dev.power_usage() {
        out.power_w = Some(mw as f32 / 1000.0);
    }
    if let Ok(c) = dev.clock_info(Clock::Graphics) {
        out.core_mhz = Some(c);
    }
    if let Ok(c) = dev.clock_info(Clock::Memory) {
        out.mem_mhz = Some(c);
    }
    if let Ok(u) = dev.utilization_rates() {
        out.util_percent = Some(u.gpu as f32);
    }
    if let Ok(m) = dev.memory_info() {
        out.vram_used_mb = Some(m.used / (1024 * 1024));
        out.vram_total_mb = Some(m.total / (1024 * 1024));
    }
    out
}
