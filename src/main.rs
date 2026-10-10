//! minihud — headless LibreHardwareMonitor backend.
//!
//! Starts the LHM hardware poller (via `tools/lhm/lhm-bridge.ps1`) and prints
//! the hardware stats on an interval. No UI, overlay, capture, hooks, or
//! application detection.

mod hw;
mod render;

use std::time::Duration;

use windows::core::{Result, PCWSTR};
use windows::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Fast cadence while hardware values are moving, backing off to idle when
/// they settle (see `HwPoller`).
const ACTIVE_MS: u64 = 200;
const IDLE_MS: u64 = 1000;
/// How often the loop wakes to check whether a poll is due.
const POLL_SLEEP_MS: u64 = 50;

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
            println!("{}", render::line(poller.cached()));
        }
        std::thread::sleep(Duration::from_millis(POLL_SLEEP_MS));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wstr_is_null_terminated_utf16() {
        assert_eq!(wstr("ab"), vec![0x61, 0x62, 0x00]);
    }
}
