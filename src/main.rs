//! minihud — headless LibreHardwareMonitor backend.
//!
//! Starts the LHM hardware poller (via `tools/lhm/lhm-bridge.ps1`) and prints
//! the hardware stats on an interval. No UI, overlay, capture, hooks, or
//! application detection.

mod hook;
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
/// ShellExecuteW reports success as a value greater than 32 (Win32 docs).
const SHELLEXECUTE_SUCCESS: isize = 32;

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    tracing::info!(
        "minihud v{} — LibreHardwareMonitor hardware backend",
        env!("CARGO_PKG_VERSION")
    );

    // `--capture-hook <pid> [secs]`: inject the present-hook recorder and print
    // fps/frametime. Opt-in, tier-3 (see HOOKING.md); needs elevation.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{}", usage());
        return Ok(());
    }
    if let Some(cap) = hook::parse_capture_hook(&args) {
        ensure_elevated();
        if let Err(e) = hook::capture_hook(cap.pid, cap.secs) {
            eprintln!("minihud: --capture-hook: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // `--launch <exe> [args...]`: start the target suspended, inject the
    // recorder before it creates its device, then print fps. Opt-in, tier-3;
    // needs elevation.
    if let Some(launch) = hook::parse_launch(&args) {
        ensure_elevated();
        if let Err(e) = hook::launch_capture(&launch.exe, &launch.args) {
            eprintln!("minihud: --launch: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // `--read-frames <pid> [secs]`: observe an existing frame ring read-only
    // (no injection). Used to watch a ring the Vulkan implicit layer created.
    if let Some(read) = hook::parse_read_frames(&args) {
        if let Err(e) = hook::read_frames(read.pid, read.secs) {
            eprintln!("minihud: --read-frames: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // `--follow [--match <glob>] [--poll-ms <n>]`: watch for a target and manage
    // the present-hook injection lifecycle. Opt-in, tier-3; needs elevation.
    if let Some(follow) = hook::follow::parse_follow(&args) {
        ensure_elevated();
        if let Err(e) = hook::follow::run(follow.match_glob, follow.poll_ms, follow.secs) {
            eprintln!("minihud: --follow: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }

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
    let skip = std::env::var_os("MINIHUD_NO_ELEVATE").is_some();
    let is_admin = unsafe { IsUserAnAdmin() }.as_bool();
    if !needs_elevation(skip, is_admin) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let params = hook::requote_args(&std::env::args().skip(1).collect::<Vec<_>>());
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
    if r.0 as isize > SHELLEXECUTE_SUCCESS {
        std::process::exit(0);
    }
    eprintln!("minihud: elevation refused; running unelevated (CPU temp/power will read --)");
}

/// True when we should relaunch elevated: not explicitly skipped and not
/// already an administrator.
fn needs_elevation(skip: bool, is_admin: bool) -> bool {
    !skip && !is_admin
}

fn wstr(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

/// CLI help text.
fn usage() -> String {
    format!(
        "minihud {version} — LibreHardwareMonitor hardware backend\n\
         \n\
         USAGE:\n\
         \x20   minihud                                        poll and print hardware stats\n\
         \x20   minihud --capture-hook <pid> [secs]            inject the present-hook recorder into <pid> and print fps\n\
         \x20   minihud --read-frames <pid> [secs]             observe an existing frame ring (no injection) and print fps\n\
         \x20   minihud --launch <exe> [args...]               start <exe> suspended, inject before it makes its device, print fps\n\
         \x20   minihud --follow [--match <glob>] [--poll-ms <n>] [--follow-secs <n>]  watch a target and manage the hook lifecycle\n\
         \x20   minihud -h | --help                            print this help\n\
         \n\
         OPTIONS:\n\
         \x20   --capture-hook <pid> [secs]   capture fps for <pid> (default {secs}s); needs elevation\n\
         \x20   --read-frames <pid> [secs]    observe an existing ring for <pid> read-only (default {secs}s); no elevation\n\
         \x20   --launch <exe> [args...]      launch <exe> with args, suspended-inject, capture {lsecs}s (or until it exits); needs elevation\n\
         \x20   --follow                      follow the foreground process (or --match), injecting and unhooking as it changes; Ctrl+C to stop\n\
         \x20   --match <glob>                with --follow, target an exe by case-insensitive glob (e.g. deadlock*.exe)\n\
         \x20   --poll-ms <n>                 with --follow, poll interval in ms (default {poll})\n\
         \x20   --follow-secs <n>             with --follow, stop automatically after <n> seconds (default: run until Ctrl+C)\n",
        version = env!("CARGO_PKG_VERSION"),
        secs = hook::DEFAULT_CAPTURE_SECS,
        lsecs = hook::LAUNCH_CAPTURE_SECS,
        poll = hook::follow::DEFAULT_POLL_MS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_documents_the_hook_commands() {
        let u = usage();
        assert!(u.contains("--follow"), "{u}");
        assert!(u.contains("--capture-hook"), "{u}");
        assert!(u.contains("--read-frames"), "{u}");
        assert!(u.contains("--launch"), "{u}");
        assert!(u.contains("--match"), "{u}");
        assert!(u.contains("--poll-ms"), "{u}");
        assert!(u.contains("--follow-secs"), "{u}");
    }

    #[test]
    fn wstr_is_null_terminated_utf16() {
        assert_eq!(wstr("ab"), vec![0x61, 0x62, 0x00]);
    }

    #[test]
    fn needs_elevation_only_when_not_skipped_and_not_admin() {
        assert!(!needs_elevation(true, false), "skip wins over not-admin");
        assert!(!needs_elevation(true, true), "skip wins over admin");
        assert!(!needs_elevation(false, true), "already an administrator");
        assert!(needs_elevation(false, false), "needs elevation");
    }
}
