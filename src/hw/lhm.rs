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
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
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
        let mut child = spawn_bridge()?;
        let (mut out, mut stdin) = bridge_streams(&mut child)?;
        read_handshake(&mut out)?;
        tracing::info!("lhm bridge: serving");
        pump(&mut out, &mut stdin, slot)
    }
}

/// Spawn the PowerShell bridge host with the DLL path as an argument.
fn spawn_bridge() -> Result<Child, String> {
    let (dll, script) = paths().ok_or_else(|| {
        "LibreHardwareMonitorLib.dll / lhm-bridge.ps1 not found (see README)".to_string()
    })?;
    Command::new("powershell")
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
        .map_err(|e| format!("spawn powershell: {e}"))
}

/// Take the bridge child's stdout / stdin pipes.
fn bridge_streams(child: &mut Child) -> Result<(BufReader<ChildStdout>, ChildStdin), String> {
    let out = BufReader::new(child.stdout.take().ok_or("no stdout")?);
    let stdin = child.stdin.take().ok_or("no stdin")?;
    Ok((out, stdin))
}

/// Read the bridge's two hello lines: a version banner, then `READY`.
fn read_handshake(out: &mut impl BufRead) -> Result<(), String> {
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
    Ok(())
}

/// Tick the bridge: send a newline, read one JSON line, store it. Returns the
/// bytes read so the caller can detect a closed stdout (0 = EOF).
fn pump(out: &mut impl BufRead, stdin: &mut impl Write, slot: &LhmLatest) -> Result<(), String> {
    let mut line = String::new();
    loop {
        let n = tick(out, stdin, &mut line, slot)?;
        if n == 0 {
            return Err("bridge closed stdout".to_string());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn tick(
    out: &mut impl BufRead,
    stdin: &mut impl Write,
    line: &mut String,
    slot: &LhmLatest,
) -> Result<usize, String> {
    stdin
        .write_all(b"\n")
        .map_err(|e| format!("bridge write: {e}"))?;
    stdin.flush().map_err(|e| format!("bridge flush: {e}"))?;
    line.clear();
    let n = out
        .read_line(line)
        .map_err(|e| format!("bridge read: {e}"))?;
    if n > 0 {
        store_sample(line, slot);
    }
    Ok(n)
}

/// Parse one JSON sensor dump and publish it; a bad line is logged, not fatal.
fn store_sample(line: &str, slot: &LhmLatest) {
    match serde_json::from_str::<Vec<LhmSensor>>(line) {
        Ok(sensors) => {
            tracing::debug!("lhm sensors: count={}", sensors.len());
            if let Ok(mut g) = slot.lock() {
                *g = Some((Instant::now(), sensors));
            }
        }
        Err(e) => tracing::debug!("lhm bridge: bad JSON line ({e})"),
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

/// First sensor value whose hardware block matches `hw_matches`, whose type is
/// `ty`, and whose name contains one of `names` (the first matching name wins).
/// Shared by every CPU/GPU/RAM lookup so there is one implementation.
fn pick(
    sensors: &[LhmSensor],
    hw_matches: impl Fn(&str) -> bool,
    ty: &str,
    names: &[&str],
) -> Option<f64> {
    names.iter().find_map(|n| {
        sensors
            .iter()
            .find(|s| hw_matches(&s.hw) && s.sensor_type == ty && s.name.contains(n))
            .map(|s| s.value)
    })
}

/// Fill stats from an LHM sample. Each group is applied by its own helper;
/// a missing sensor leaves its field untouched (which prints as `--`).
pub fn apply(sensors: &[LhmSensor], stats: &mut super::HwStats) {
    tracing::debug!("lhm apply: {} sensors", sensors.len());
    apply_cpu(sensors, stats);
    apply_ram(sensors, stats);
    apply_names(sensors, stats);
    apply_gpu(sensors, stats);
}

/// CPU load / temperature / power / clock from the `Cpu:` block.
fn apply_cpu(sensors: &[LhmSensor], stats: &mut super::HwStats) {
    let cpu = |ty: &str, names: &[&str]| pick(sensors, |hw| hw.starts_with("Cpu:"), ty, names);
    if let Some(v) = cpu("Load", &["CPU Total"]) {
        stats.cpu_percent = v.clamp(0.0, 100.0) as f32;
    }
    if let Some(v) = cpu(
        "Temperature",
        &["CPU Package", "Core Max", "Tctl", "Core ("],
    ) {
        stats.cpu_temp_c = Some(v as f32);
    }
    if let Some(v) = cpu("Power", &["Package", "CPU Package"]) {
        stats.cpu_power_w = Some(v as f32);
    }
    if let Some(v) = cpu(
        "Clock",
        &["Cores (Average)", "CPU Core", "Core #1", "Core ("],
    ) {
        stats.cpu_clock_mhz = Some(v as u32);
    }
}

/// RAM used / total in MB. LHM reports memory in GB.
fn apply_ram(sensors: &[LhmSensor], stats: &mut super::HwStats) {
    let mem = |names: &[&str]| pick(sensors, |hw| hw.starts_with("Memory:"), "Data", names);
    let used = mem(&["Memory Used", "Used Memory"]);
    let avail = mem(&["Memory Available", "Available Memory"]);
    if let (Some(u), Some(a)) = (used, avail) {
        stats.ram_used_mb = Some((u * 1024.0) as u64);
        stats.ram_total_mb = Some(((u + a) * 1024.0) as u64);
    }
}

/// CPU / GPU names from the first matching hardware block.
fn apply_names(sensors: &[LhmSensor], stats: &mut super::HwStats) {
    let name_of = |s: &LhmSensor| s.hw.split_once(':').map(|(_, n)| n.trim().to_string());
    if let Some(s) = sensors.iter().find(|s| s.hw.starts_with("Cpu:")) {
        stats.cpu_name = name_of(s);
    }
    if let Some(s) = sensors.iter().find(|s| s.hw.starts_with("Gpu")) {
        stats.gpu_name = name_of(s);
    }
}

/// GPU sensors from the first discrete GPU block (single-dGPU assumption,
/// documented). Everything is scoped to that exact hardware string.
fn apply_gpu(sensors: &[LhmSensor], stats: &mut super::HwStats) {
    let Some(hw) = sensors
        .iter()
        .find(|s| s.hw.starts_with("Gpu"))
        .map(|s| s.hw.clone())
    else {
        return;
    };
    let g = |ty: &str, names: &[&str]| pick(sensors, |h| h == hw.as_str(), ty, names);
    if let Some(v) = g("Temperature", &["GPU Core"]) {
        stats.gpu_temp_c = Some(v as f32);
    }
    if let Some(v) = g("Load", &["GPU Core"]) {
        stats.gpu_percent = Some(v.clamp(0.0, 100.0) as f32);
    }
    if let Some(v) = g("Power", &["GPU Package", "GPU Power"]) {
        stats.gpu_power_w = Some(v as f32);
    }
    if let Some(v) = g("Clock", &["GPU Core"]) {
        stats.gpu_core_mhz = Some(v as u32);
    }
    if let Some(v) = g("Clock", &["GPU Memory"]) {
        stats.gpu_mem_mhz = Some(v as u32);
    }
    if let Some(v) = g("Voltage", &["GPU Core", "GPU Voltage"]) {
        stats.gpu_voltage_mv = Some(v as u32);
    }
    if let Some(v) = g("SmallData", &["D3D Dedicated Memory Used"]) {
        stats.gpu_d3d_dedicated_mb = Some(v as u64);
    }
    let used = g("SmallData", &["GPU Memory Used"]);
    let total = g("SmallData", &["GPU Memory Total"]);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hw::HwStats;

    fn s(hw: &str, ty: &str, name: &str, value: f64) -> LhmSensor {
        LhmSensor {
            hw: hw.to_string(),
            sensor_type: ty.to_string(),
            name: name.to_string(),
            value,
        }
    }

    #[test]
    fn apply_maps_cpu_sensors() {
        let sensors = vec![
            s("Cpu:AMD Ryzen 9", "Load", "CPU Total", 42.0),
            s("Cpu:AMD Ryzen 9", "Temperature", "CPU Package", 61.5),
            s("Cpu:AMD Ryzen 9", "Power", "Package", 88.0),
            s("Cpu:AMD Ryzen 9", "Clock", "Cores (Average)", 4200.0),
        ];
        let mut st = HwStats::default();
        apply(&sensors, &mut st);
        assert_eq!(st.cpu_percent, 42.0);
        assert_eq!(st.cpu_temp_c, Some(61.5));
        assert_eq!(st.cpu_power_w, Some(88.0));
        assert_eq!(st.cpu_clock_mhz, Some(4200));
        assert_eq!(st.cpu_name.as_deref(), Some("AMD Ryzen 9"));
    }

    #[test]
    fn apply_clamps_percentages() {
        let sensors = vec![
            s("Cpu:X", "Load", "CPU Total", 150.0),
            s("GpuNvidia:RTX", "Load", "GPU Core", -5.0),
        ];
        let mut st = HwStats::default();
        apply(&sensors, &mut st);
        assert_eq!(st.cpu_percent, 100.0);
        assert_eq!(st.gpu_percent, Some(0.0));
    }

    #[test]
    fn apply_maps_ram_in_megabytes() {
        let sensors = vec![
            s("Memory:", "Data", "Memory Used", 8.0),
            s("Memory:", "Data", "Memory Available", 24.0),
        ];
        let mut st = HwStats::default();
        apply(&sensors, &mut st);
        assert_eq!(st.ram_used_mb, Some(8 * 1024));
        assert_eq!(st.ram_total_mb, Some(32 * 1024));
    }

    #[test]
    fn apply_maps_gpu_sensors() {
        let sensors = vec![
            s("GpuNvidia:RTX 3070", "Temperature", "GPU Core", 55.0),
            s("GpuNvidia:RTX 3070", "Load", "GPU Core", 80.0),
            s("GpuNvidia:RTX 3070", "Power", "GPU Package", 200.0),
            s("GpuNvidia:RTX 3070", "Clock", "GPU Core", 1900.0),
            s("GpuNvidia:RTX 3070", "Clock", "GPU Memory", 7000.0),
            s("GpuNvidia:RTX 3070", "Voltage", "GPU Core", 1050.0),
            s(
                "GpuNvidia:RTX 3070",
                "SmallData",
                "D3D Dedicated Memory Used",
                4096.0,
            ),
            s("GpuNvidia:RTX 3070", "SmallData", "GPU Memory Used", 3000.0),
            s(
                "GpuNvidia:RTX 3070",
                "SmallData",
                "GPU Memory Total",
                8192.0,
            ),
        ];
        let mut st = HwStats::default();
        apply(&sensors, &mut st);
        assert_eq!(st.gpu_temp_c, Some(55.0));
        assert_eq!(st.gpu_percent, Some(80.0));
        assert_eq!(st.gpu_power_w, Some(200.0));
        assert_eq!(st.gpu_core_mhz, Some(1900));
        assert_eq!(st.gpu_mem_mhz, Some(7000));
        assert_eq!(st.gpu_voltage_mv, Some(1050));
        assert_eq!(st.gpu_d3d_dedicated_mb, Some(4096));
        assert_eq!(st.gpu_mem_used_mb, Some(3000));
        assert_eq!(st.gpu_vram_used_mb, Some(3000));
        assert_eq!(st.gpu_vram_total_mb, Some(8192));
        assert_eq!(st.gpu_name.as_deref(), Some("RTX 3070"));
    }

    #[test]
    fn apply_leaves_missing_fields_untouched() {
        let mut st = HwStats {
            cpu_temp_c: Some(50.0),
            ..HwStats::default()
        };
        apply(&[], &mut st);
        assert_eq!(
            st.cpu_temp_c,
            Some(50.0),
            "no sensor must not clear a field"
        );
        assert_eq!(st.cpu_percent, 0.0);
        assert_eq!(st.gpu_temp_c, None);
        assert_eq!(st.cpu_name, None);
    }

    #[test]
    fn apply_ignores_vram_when_total_is_zero() {
        let sensors = vec![
            s("GpuX:GPU", "SmallData", "GPU Memory Used", 100.0),
            s("GpuX:GPU", "SmallData", "GPU Memory Total", 0.0),
        ];
        let mut st = HwStats::default();
        apply(&sensors, &mut st);
        assert_eq!(st.gpu_mem_used_mb, Some(100));
        assert_eq!(st.gpu_vram_used_mb, None);
        assert_eq!(st.gpu_vram_total_mb, None);
    }
}
