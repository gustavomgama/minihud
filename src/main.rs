//! minihud — headless LibreHardwareMonitor backend.
//!
//! Starts the LHM hardware poller (via `tools/lhm/lhm-bridge.ps1`) and prints
//! the hardware stats on an interval. No UI, overlay, capture, hooks, or
//! application detection.

mod hw;

use std::time::Duration;

use hw::HwStats;
use windows::core::{Result, PCWSTR};
use windows::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Fast cadence while hardware values are moving, backing off to idle when
/// they settle (see `HwPoller`).
const ACTIVE_MS: u64 = 200;
const IDLE_MS: u64 = 1000;

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    tracing::info!(
        "minihud v{} — LibreHardwareMonitor hardware backend",
        env!("CARGO_PKG_VERSION")
    );
    ensure_elevated();

    let mut poller = hw::HwPoller::new(ACTIVE_MS, IDLE_MS);
    loop {
        if poller.update().is_some() {
            println!("{}", line(poller.cached()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// One hardware sample, formatted as a single text line. Missing values
/// print as `--`.
fn line(s: &HwStats) -> String {
    format!(
        "CPU {:.0}% {} {} {} | RAM {} | GPU {} {} {} {}x{}MHz | {}{}",
        s.cpu_percent,
        f32u(s.cpu_temp_c, "C"),
        f32u(s.cpu_power_w, "W"),
        u32u(s.cpu_clock_mhz, "MHz"),
        ram(s.ram_used_mb, s.ram_total_mb),
        f32u(s.gpu_percent, "%"),
        f32u(s.gpu_temp_c, "C"),
        f32u(s.gpu_power_w, "W"),
        u32u(s.gpu_core_mhz, ""),
        u32u(s.gpu_mem_mhz, ""),
        vram(s),
        names(s),
    )
}

fn f32u(v: Option<f32>, unit: &str) -> String {
    v.map(|v| format!("{v:.0}{unit}"))
        .unwrap_or_else(|| "--".to_string())
}

fn u32u(v: Option<u32>, unit: &str) -> String {
    v.map(|v| format!("{v}{unit}"))
        .unwrap_or_else(|| "--".to_string())
}

fn ram(used: Option<u64>, total: Option<u64>) -> String {
    match (used, total) {
        (Some(u), Some(t)) => format!("{u}/{t}MB"),
        _ => "--".to_string(),
    }
}

fn vram(s: &HwStats) -> String {
    match (s.gpu_vram_used_mb, s.gpu_vram_total_mb) {
        (Some(u), Some(t)) => format!("VRAM {u}/{t}MB"),
        _ => "VRAM --".to_string(),
    }
}

fn names(s: &HwStats) -> String {
    match (&s.cpu_name, &s.gpu_name) {
        (Some(c), Some(g)) => format!("[{c} / {g}]"),
        (Some(c), None) => format!("[{c}]"),
        (None, Some(g)) => format!("[{g}]"),
        (None, None) => String::new(),
    }
}

/// Ask for UAC elevation when not already elevated, so CPU temp/power
/// (SuperIO) and the LHM bridge work. Relaunches self via ShellExecuteW
/// "runas" and exits; falls back to running unelevated if refused.
/// `MINIHUD_NO_ELEVATE=1` skips this (headless tests/diagnostics).
fn ensure_elevated() {
    if std::env::var_os("MINIHUD_NO_ELEVATE").is_some() {
        return;
    }
    if unsafe { IsUserAnAdmin() }.as_bool() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let params = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let verb = wstr("runas");
    let file = wstr(&exe.to_string_lossy());
    let params_w = wstr(&params);
    let r = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR(params_w.as_ptr()),
            None,
            SW_SHOWNORMAL,
        )
    };
    if r.0 as isize > 32 {
        std::process::exit(0);
    }
    eprintln!("minihud: elevation refused; running unelevated (CPU temp/power will read --)");
}

fn wstr(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}
