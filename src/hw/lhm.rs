//! LibreHardwareMonitor sidecar feed: the SOLE hardware data source.
//!
//! LHM is .NET-only, so a persistent `powershell` host runs
//! `tools/lhm/lhm-bridge.ps1` and emits one JSON sensor dump per blank
//! line on stdin. This module owns that child on a dedicated thread:
//! the main loop NEVER blocks on it — `latest()` just reads the last
//! good sample, and rows read "--" until the first one lands.

use serde::Deserialize;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One sensor reading from the bridge.
#[derive(Clone, Debug, Deserialize)]
pub struct LhmSensor {
    /// e.g. "GpuNvidia:NVIDIA GeForce RTX 3070".
    pub hw: String,
    /// e.g. "Temperature", "Load", "Power", "Clock", "SmallData".
    #[serde(rename = "type")]
    pub sensor_type: String,
    /// e.g. "GPU Core", "CPU Total", "Package".
    pub name: String,
    pub value: f64,
}

/// Shared latest-sample slot: `(timestamp, sensors)` behind a mutex.
type LhmLatest = Arc<Mutex<Option<(Instant, Vec<LhmSensor>)>>>;

#[derive(Clone)]
pub struct LhmFeed {
    latest: LhmLatest,
}

impl LhmFeed {
    pub fn start() -> Self {
        let latest = Arc::new(Mutex::new(None));
        let slot = latest.clone();
        std::thread::spawn(move || Self::run(slot));
        Self { latest }
    }

    /// Last sample if younger than `max_age`, else None (rows read
    /// "--"). Never blocks, never fails.
    pub fn latest(&self, max_age: Duration) -> Option<Vec<LhmSensor>> {
        let g = self.latest.lock().ok()?;
        let (t, v) = g.as_ref()?;
        if t.elapsed() < max_age {
            Some(v.clone())
        } else {
            None
        }
    }

    fn run(slot: LhmLatest) {
        loop {
            if let Err(e) = Self::session(&slot) {
                tracing::warn!("lhm bridge: {e}; retry in 5s");
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    }

    fn session(slot: &LhmLatest) -> Result<(), String> {
        let (dll, script) = paths().ok_or_else(|| {
            "LibreHardwareMonitorLib.dll / lhm-bridge.ps1 not found (see README)".to_string()
        })?;
        let mut child: Child = Command::new("powershell")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                &script.to_string_lossy(),
                "-DllPath",
                &dll.to_string_lossy(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("spawn powershell: {e}"))?;
        let mut out = BufReader::new(child.stdout.take().ok_or("no stdout")?);
        let mut stdin = child.stdin.take().ok_or("no stdin")?;
        // Version line + READY.
        let mut line = String::new();
        out.read_line(&mut line)
            .map_err(|e| format!("bridge hello: {e}"))?;
        tracing::info!("lhm bridge: {}", line.trim());
        line.clear();
        out.read_line(&mut line)
            .map_err(|e| format!("bridge ready: {e}"))?;
        if line.trim() != "READY" {
            return Err(format!("bridge unexpected hello: {}", line.trim()));
        }
        tracing::info!("lhm bridge: serving");
        loop {
            stdin
                .write_all(b"\n")
                .map_err(|e| format!("bridge write: {e}"))?;
            stdin.flush().map_err(|e| format!("bridge flush: {e}"))?;
            line.clear();
            let n = out
                .read_line(&mut line)
                .map_err(|e| format!("bridge read: {e}"))?;
            if n == 0 {
                return Err("bridge closed stdout".to_string());
            }
            match serde_json::from_str::<Vec<LhmSensor>>(&line) {
                Ok(sensors) => {
                    tracing::debug!("lhm sensors: count={}", sensors.len());
                    if let Ok(mut g) = slot.lock() {
                        *g = Some((Instant::now(), sensors));
                    }
                }
                Err(e) => {
                    tracing::debug!("lhm bridge: bad JSON line ({e})");
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

/// DLL + script live next to the exe in every profile. `cargo xtask build`
/// stages them there; there is no build-time / manifest-dir fallback, so
/// debug and release resolve assets identically.
fn paths() -> Option<(PathBuf, PathBuf)> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.to_path_buf();
    let find = |name: &str| -> Option<PathBuf> {
        let p = dir.join(name);
        p.exists().then_some(p)
    };
    Some((
        find("LibreHardwareMonitorLib.dll")?,
        find("lhm-bridge.ps1")?,
    ))
}

/// Fill stats from an LHM sample. First match wins per rule; anything
/// absent leaves the field for the legacy fallbacks. LHM is the ONLY
/// source for CPU temp/power (no user-mode alternative exists).
pub fn apply(sensors: &[LhmSensor], stats: &mut super::HwStats) {
    tracing::debug!("lhm apply: {} sensors", sensors.len());
    let find = |hw_prefix: &str, ty: &str, names: &[&str]| -> Option<f64> {
        for n in names {
            if let Some(s) = sensors
                .iter()
                .find(|s| s.hw.starts_with(hw_prefix) && s.sensor_type == ty && s.name.contains(n))
            {
                return Some(s.value);
            }
        }
        None
    };
    if let Some(v) = find("Cpu:", "Load", &["CPU Total"]) {
        stats.cpu_percent = v.clamp(0.0, 100.0) as f32;
    }
    if let Some(v) = find(
        "Cpu:",
        "Temperature",
        &["CPU Package", "Core Max", "Tctl", "Core ("],
    ) {
        stats.cpu_temp_c = Some(v as f32);
    }
    if let Some(v) = find("Cpu:", "Power", &["Package", "CPU Package"]) {
        stats.cpu_power_w = Some(v as f32);
    }
    if let Some(v) = find(
        "Cpu:",
        "Clock",
        &["Cores (Average)", "CPU Core", "Core #1", "Core ("],
    ) {
        stats.cpu_clock_mhz = Some(v as u32);
    }
    // LHM reports memory in GB; everything downstream is MB.
    let mem_used = find("Memory:", "Data", &["Memory Used", "Used Memory"]);
    let mem_avail = find("Memory:", "Data", &["Memory Available", "Available Memory"]);
    if let (Some(u), Some(a)) = (mem_used, mem_avail) {
        stats.ram_used_mb = Some((u * 1024.0) as u64);
        stats.ram_total_mb = Some(((u + a) * 1024.0) as u64);
    }
    // Extract CPU/GPU names from the first matching hardware block.
    if let Some(cpu_s) = sensors.iter().find(|s| s.hw.starts_with("Cpu:")) {
        if let Some((_, name)) = cpu_s.hw.split_once(':') {
            stats.cpu_name = Some(name.trim().to_string());
        }
    }
    if let Some(gpu_s) = sensors.iter().find(|s| s.hw.starts_with("Gpu")) {
        if let Some((_, name)) = gpu_s.hw.split_once(':') {
            stats.gpu_name = Some(name.trim().to_string());
        }
    }
    // First discrete GPU block wins (single-dGPU assumption, documented).
    let gpu_hw = sensors
        .iter()
        .find(|s| s.hw.starts_with("Gpu"))
        .map(|s| s.hw.clone());
    if let Some(hw) = gpu_hw {
        let gfind = |ty: &str, names: &[&str]| -> Option<f64> {
            for n in names {
                if let Some(s) = sensors
                    .iter()
                    .find(|s| s.hw == hw && s.sensor_type == ty && s.name.contains(n))
                {
                    return Some(s.value);
                }
            }
            None
        };
        if let Some(v) = gfind("Temperature", &["GPU Core"]) {
            stats.gpu_temp_c = Some(v as f32);
        }
        if let Some(v) = gfind("Load", &["GPU Core"]) {
            stats.gpu_percent = Some(v.clamp(0.0, 100.0) as f32);
        }
        if let Some(v) = gfind("Power", &["GPU Package", "GPU Power"]) {
            stats.gpu_power_w = Some(v as f32);
        }
        if let Some(v) = gfind("Clock", &["GPU Core"]) {
            stats.gpu_core_mhz = Some(v as u32);
        }
        if let Some(v) = gfind("Clock", &["GPU Memory"]) {
            stats.gpu_mem_mhz = Some(v as u32);
        }
        if let Some(v) = gfind("Voltage", &["GPU Core", "GPU Voltage"]) {
            stats.gpu_voltage_mv = Some(v as u32);
        }
        if let Some(v) = gfind("SmallData", &["D3D Dedicated Memory Used"]) {
            stats.gpu_d3d_dedicated_mb = Some(v as u64);
        }
        let used = gfind("SmallData", &["GPU Memory Used"]);
        let total = gfind("SmallData", &["GPU Memory Total"]);
        if let Some(v) = used {
            stats.gpu_mem_used_mb = Some(v as u64);
        }
        if let (Some(u), Some(t)) = (used, total) {
            if t > 0.0 {
                stats.gpu_vram_used_mb = Some(u as u64);
                stats.gpu_vram_total_mb = Some(t as u64);
            }
        }
    }
}
