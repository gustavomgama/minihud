//! minihud — hardware stats + present-hook capture backend.
//!
//! Prints LibreHardwareMonitor hardware stats on an interval (via
//! `tools/lhm/lhm-bridge.ps1`) and, by default, follows the foreground process
//! to manage the present-hook injection lifecycle (see the `hook` module).
//! Capture/hook/injection logging and the hardware stats share one console.
//! There is no UI or overlay; `--stats-only` disables capture.

mod console;
mod hook;
mod hw;
mod render;

use std::time::Duration;

use windows::core::{Result, PCWSTR};
use windows::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Default tracing filter: everything at `info`, with the hook/follow modules
/// verbose (`debug`) for injection debugging. Overridable with `RUST_LOG`.
const DEFAULT_LOG_FILTER: &str = "info,minihud::hook=debug";

/// Fast cadence while hardware values are moving, backing off to idle when
/// they settle (see `HwPoller`).
/// Default poll cadence (ms) for the hardware stats **and** the fps/frametime
/// line. Override with `--interval-ms <n>`.
const DEFAULT_INTERVAL_MS: u64 = 400;
/// How often the stats loop wakes to check whether a poll is due.
const POLL_SLEEP_MS: u64 = 50;
/// ShellExecuteW reports success as a value greater than 32 (Win32 docs).
const SHELLEXECUTE_SUCCESS: isize = 32;

fn main() -> Result<()> {
    // Handle `--help` before any logging so the help text is clean.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{}", usage());
        return Ok(());
    }

    // Enable console colour (best-effort) before logging is configured.
    let ansi = console::enable_vt();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER)),
        )
        .with_target(false)
        .with_timer(console::CompactTime)
        .with_ansi(ansi)
        .with_writer(console::LogMakeWriter)
        .init();
    tracing::info!(
        "minihud v{} — hardware stats + present-hook capture",
        env!("CARGO_PKG_VERSION")
    );

    let interval = parse_interval_ms(&args);
    let result = match select_mode(&args) {
        // Inject into an already-running process and print fps + tracking.
        Mode::CaptureHook(cap) => {
            ensure_elevated();
            report(
                "--capture-hook",
                run_with_stats(interval, || {
                    hook::capture_hook(&cap.target, cap.secs, interval)
                }),
            )
        }
        // Start the target suspended, inject before it creates its device, run.
        Mode::Launch(launch) => {
            ensure_elevated();
            report(
                "--launch",
                run_with_stats(interval, || {
                    hook::launch_capture(&launch.exe, &launch.args, launch.secs, interval)
                }),
            )
        }
        // Observe an existing frame ring read-only (no injection, no elevation).
        Mode::ReadFrames(read) => report(
            "--read-frames",
            run_with_stats(interval, || {
                hook::read_frames(&read.target, read.secs, interval)
            }),
        ),
        // Hardware stats only, never capture.
        Mode::StatsOnly => {
            ensure_elevated();
            stats_loop(interval);
            Ok(())
        }
        // Default: follow the foreground process (or `--match`), showing fps +
        // hardware stats on the same line. `--poll-ms` overrides the tick.
        Mode::Follow(follow) => {
            ensure_elevated();
            let poll = if args.iter().any(|a| a == "--poll-ms") {
                follow.poll_ms
            } else {
                interval
            };
            report(
                "--follow",
                run_with_stats(interval, || {
                    hook::follow::run(follow.match_glob, poll, follow.secs)
                }),
            )
        }
    };
    console::finish();
    result
}

/// `--interval-ms <n>`: poll cadence for the hardware stats and the fps line
/// (default [`DEFAULT_INTERVAL_MS`]; 0/absent → default).
fn parse_interval_ms(args: &[String]) -> u64 {
    args.windows(2)
        .find(|w| w[0] == "--interval-ms")
        .and_then(|w| w[1].parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_INTERVAL_MS)
}

/// The run mode resolved from the CLI arguments.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    CaptureHook(hook::CaptureArgs),
    Launch(hook::LaunchArgs),
    ReadFrames(hook::ReadFramesArgs),
    StatsOnly,
    Follow(hook::follow::FollowArgs),
}

/// Resolve the run mode. Precedence: an explicit capture command, then
/// `--stats-only`, then the default `--follow`.
fn select_mode(args: &[String]) -> Mode {
    if let Some(cap) = hook::parse_capture_hook(args) {
        return Mode::CaptureHook(cap);
    }
    if let Some(launch) = hook::parse_launch(args) {
        return Mode::Launch(launch);
    }
    if let Some(read) = hook::parse_read_frames(args) {
        return Mode::ReadFrames(read);
    }
    if args.iter().any(|a| a == "--stats-only") {
        return Mode::StatsOnly;
    }
    Mode::Follow(hook::follow::parse_follow(args).unwrap_or_default())
}

/// Print a capture error and exit non-zero, else return `Ok`.
fn report(flag: &str, result: std::result::Result<(), String>) -> Result<()> {
    if let Err(e) = result {
        tracing::error!("{flag}: {e}");
        std::process::exit(1);
    }
    Ok(())
}

/// Run a capture closure on this thread while the hardware-stats poller updates
/// the status line on the same console in the background, so capture/hook/
/// injection logging and the hardware stats share one output.
fn run_with_stats<F>(interval: u64, capture: F) -> std::result::Result<(), String>
where
    F: FnOnce() -> std::result::Result<(), String>,
{
    std::thread::spawn(move || stats_loop(interval));
    capture()
}

/// Poll the hardware sensors at `interval` and update the status line, forever.
fn stats_loop(interval: u64) {
    let mut poller = hw::HwPoller::new(interval, interval);
    loop {
        if poller.update().is_some() {
            console::status_hw(render::line(poller.cached()));
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
    tracing::warn!("elevation refused; running unelevated (CPU temp/power will read --)");
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
        "minihud {version} — LibreHardwareMonitor hardware backend + present-hook capture\n\
         \n\
         USAGE:\n\
         \x20   minihud                                  follow a target and print hardware stats (default)\n\
         \x20   minihud --stats-only                     print hardware stats only (no capture)\n\
         \x20   minihud --capture-hook <pid|name> [secs]  inject the recorder into a running process\n\
         \x20   minihud --read-frames <pid|name> [secs]   observe an existing frame ring (no injection)\n\
         \x20   minihud --launch <exe> [args...]         start <exe> suspended, inject, then run it\n\
         \x20   minihud --follow [--match <glob>] [--poll-ms <n>] [--follow-secs <n>]\n\
         \x20   minihud -h | --help                      print this help\n\
         \n\
         A <pid|name> is a numeric pid or a process image name (case-insensitive,\n\
         `.exe` optional: `Overwatch`, `overwatch.exe`, or `1234`).\n\
         \n\
         CAPTURE FLAGS (each prints the hardware stats on the same console):\n\
         \x20   --capture-hook <pid|name> [secs]  inject the present-hook recorder into an already-running\n\
         \x20                                     process and print fps/frametime + tracking. [secs] is the\n\
         \x20                                     capture window (default: until Ctrl+C). Needs elevation.\n\
         \x20   --launch <exe> [args...]     launch <exe> suspended, inject before it creates its device\n\
         \x20                               (so the D3D12 command queue is reachable too), resume, then\n\
         \x20                               capture until it exits (or --launch-secs). Needs elevation.\n\
         \x20   --launch-secs <n>            with --launch, capture for <n> seconds instead of until exit.\n\
         \x20                               Must precede --launch.\n\
         \x20   --read-frames <pid|name> [secs]  open an existing frame ring for the process read-only\n\
         \x20                                     (no injection) and print fps (default: until Ctrl+C). Use it\n\
         \x20                                     to watch a ring the Vulkan implicit layer created. No elevation.\n\
         \x20   --follow                     watch the foreground process and manage the hook lifecycle\n\
         \x20                               (detect -> inject -> retarget -> unhook) as the target\n\
         \x20                               changes. This is the DEFAULT when no other flag is given.\n\
         \x20                               Needs elevation; Ctrl+C to stop.\n\
         \x20   --match <glob>               with --follow, target an exe by case-insensitive glob\n\
         \x20                               instead of the foreground process (e.g. deadlock*.exe).\n\
         \x20   --poll-ms <n>                with --follow, how often to re-check the target (ms,\n\
         \x20                               default {poll}).\n\
         \x20   --follow-secs <n>            with --follow, stop after <n> seconds (default: until Ctrl+C).\n\
         \x20   --interval-ms <n>            poll cadence in ms for the hardware stats and the fps line\n\
         \x20                               (default {interval}).\n\
         \x20   --stats-only                 print hardware stats only; never capture a target.\n",
        version = env!("CARGO_PKG_VERSION"),
        poll = hook::follow::DEFAULT_POLL_MS,
        interval = DEFAULT_INTERVAL_MS,
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
        assert!(u.contains("--stats-only"), "{u}");
        assert!(u.contains("--launch-secs"), "{u}");
        assert!(u.contains("--interval-ms"), "{u}");
    }

    #[test]
    fn select_mode_defaults_to_follow() {
        // Catches: the default silently regressing to stats-only, or an explicit
        // capture command losing precedence to `--stats-only`.
        use hook::follow::FollowArgs;
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(select_mode(&a(&[])), Mode::Follow(FollowArgs::default()));
        assert_eq!(
            select_mode(&a(&["--follow"])),
            Mode::Follow(FollowArgs::default())
        );
        assert_eq!(select_mode(&a(&["--stats-only"])), Mode::StatsOnly);
        assert_eq!(
            select_mode(&a(&["--capture-hook", "42", "5"])),
            Mode::CaptureHook(hook::CaptureArgs {
                target: "42".into(),
                secs: Some(5)
            })
        );
        assert_eq!(
            select_mode(&a(&["--capture-hook", "Overwatch.exe"])),
            Mode::CaptureHook(hook::CaptureArgs {
                target: "Overwatch.exe".into(),
                secs: None
            })
        );
        assert_eq!(
            select_mode(&a(&["--read-frames", "7"])),
            Mode::ReadFrames(hook::ReadFramesArgs {
                target: "7".into(),
                secs: None
            })
        );
        assert!(matches!(
            select_mode(&a(&["--launch", "game.exe"])),
            Mode::Launch(_)
        ));
        // An explicit capture command wins over `--stats-only`.
        assert_eq!(
            select_mode(&a(&["--stats-only", "--capture-hook", "1"])),
            Mode::CaptureHook(hook::CaptureArgs {
                target: "1".into(),
                secs: None
            })
        );
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
