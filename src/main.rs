//! minihud — LibreHardwareMonitor hardware-stats backend for Windows.
//!
//! Reads CPU/GPU/RAM sensors through the LibreHardwareMonitor bridge
//! (`tools/lhm/lhm-bridge.ps1`) and prints one stats line per poll. This is the
//! whole product: it captures no fps, injects nothing, and draws no overlay.

mod hw;
mod render;
mod retry;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows::core::{Result, PCWSTR};
use windows::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::Win32::System::Console::SetConsoleCtrlHandler;

/// Default tracing filter: everything at `info`. Overridable with `RUST_LOG`.
const DEFAULT_LOG_FILTER: &str = "info";

/// Hardware poll cadence (ms): one value drives both the stats-line refresh
/// and the LHM bridge tick. Override with `--hardware-poll <n>`.
const DEFAULT_HW_POLL_MS: u64 = 500;
/// Longest slice the shutdown-aware poll wait sleeps before re-checking the stop
/// flag, so exit reaps the LHM child promptly at any poll cadence.
const SHUTDOWN_POLL: Duration = Duration::from_millis(100);
/// ShellExecuteW reports success as a value greater than 32 (Win32 docs).
const SHELLEXECUTE_SUCCESS: isize = 32;

/// Stats lines printed so far, for the `--stats` rate readout.
static HW_UPDATES: AtomicU64 = AtomicU64::new(0);
/// Cleared by the Ctrl+C handler so the poll loop exits and the LHM feed — and
/// with it the PowerShell child — is dropped instead of orphaned.
static RUNNING: AtomicBool = AtomicBool::new(true);

fn main() -> Result<()> {
    // Handle `--help` before any logging so the help text is clean.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{}", usage());
        return Ok(());
    }

    // Logs go to stderr so they never interleave with the stats lines on stdout.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER)),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
    tracing::info!(
        "minihud v{} — LibreHardwareMonitor hardware stats",
        env!("CARGO_PKG_VERSION")
    );

    let hw = parse_hw_opts(&args);
    ensure_elevated();
    // `--stats`: a background line reporting the achieved status-update rate.
    if hw.stats {
        std::thread::spawn(stats_reporter);
    }
    // Register Ctrl+C handler so the LHM child gets reaped on interrupt.
    // SAFETY: SetConsoleCtrlHandler with a valid signature; process-global handler.
    let _ = unsafe {
        SetConsoleCtrlHandler(
            Some(ctrl_handler),
            true, // add handler
        )
    };
    stats_loop(hw);
    Ok(())
}

/// The hardware capture options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HwOpts {
    poll_ms: u64,
    /// Whether to run the `--stats` rate reporter.
    stats: bool,
}

impl Default for HwOpts {
    fn default() -> Self {
        Self {
            poll_ms: DEFAULT_HW_POLL_MS,
            stats: false,
        }
    }
}

/// Parse `--hardware-poll <n>` and `--stats`. A zero/unparseable/valueless poll
/// keeps the default.
fn parse_hw_opts(args: &[String]) -> HwOpts {
    HwOpts {
        poll_ms: cli_poll_ms(args).unwrap_or(DEFAULT_HW_POLL_MS),
        stats: args.iter().any(|a| a == "--stats"),
    }
}

/// The `--hardware-poll <n>` value when the flag is present with a usable
/// value. `None` means absent, so the caller falls through to the default.
fn cli_poll_ms(args: &[String]) -> Option<u64> {
    args.windows(2)
        .find(|w| w[0] == "--hardware-poll")
        .and_then(|w| w[1].parse::<u64>().ok())
        .filter(|&n| n > 0)
}

/// Poll the hardware sensors at the configured cadence and print each sample
/// until Ctrl+C. Owns the [`hw::HwPoller`] (and thus the LHM feed), so returning
/// drops it and reaps the child.
fn stats_loop(opts: HwOpts) {
    let mut poller = hw::HwPoller::new(opts.poll_ms);
    while RUNNING.load(Ordering::SeqCst) {
        if poller.update().is_some() {
            HW_UPDATES.fetch_add(1, Ordering::Relaxed);
            println!("{}", render::line(poller.cached()));
        }
        wait_for_due(&RUNNING, poller.due_in());
    }
}

/// Log the achieved stats-update rate once per second, so "as fast as
/// possible" is a measured number. Only spawned for `--stats`.
fn stats_reporter() {
    let period = Duration::from_secs(1);
    let mut prev = HW_UPDATES.load(Ordering::Relaxed);
    let mut last = Instant::now();
    loop {
        std::thread::sleep(period);
        let cur = HW_UPDATES.load(Ordering::Relaxed);
        let rate = rate_per_sec(prev, cur, last.elapsed().as_secs_f64());
        tracing::info!("{}", stats_line(rate, retry::HW_RETRIES.total()));
        prev = cur;
        last = Instant::now();
    }
}

/// Updates per second between two counter snapshots over `dt` seconds. A
/// non-positive `dt` reports 0 rather than dividing by zero. Pure, so the rate
/// math is unit-tested.
fn rate_per_sec(prev: u64, cur: u64, dt: f64) -> f64 {
    if dt > 0.0 {
        cur.saturating_sub(prev) as f64 / dt
    } else {
        0.0
    }
}

/// Format the `--stats` line: the achieved update rate plus the cumulative
/// retry count, so a wedged dependency is visible live. Pure, so the
/// diagnostics content is unit-tested.
fn stats_line(hw: f64, hw_retries: u64) -> String {
    format!("rate: hw {hw:.1}/s; retries hw {hw_retries}")
}

/// Sleep until `due` from now, waking early when `stop` is set. Bounded so
/// shutdown reaps the LHM child promptly even at a large `--hardware-poll`.
fn wait_for_due(stop: &AtomicBool, due: Duration) {
    let deadline = Instant::now() + due;
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        std::thread::sleep((deadline - now).min(SHUTDOWN_POLL));
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
    let params = requote_args(&std::env::args().skip(1).collect::<Vec<_>>());
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

/// Quote one token for a Windows command line.
///
/// A plain token is passed through unchanged; otherwise the Microsoft argv
/// quoting rules apply: wrap in `"`, double every backslash that precedes a
/// (literal or closing) quote, and escape embedded quotes.
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains(' ') && !arg.contains('\t') && !arg.contains('"') {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut chars = arg.chars().peekable();
    loop {
        let mut backslashes = 0;
        while chars.peek() == Some(&'\\') {
            chars.next();
            backslashes += 1;
        }
        match chars.peek() {
            None => {
                // Trailing backslashes must be doubled so the closing quote
                // stays a quote rather than escaping a preceding backslash.
                for _ in 0..backslashes * 2 {
                    out.push('\\');
                }
                break;
            }
            Some(&'"') => {
                for _ in 0..backslashes * 2 + 1 {
                    out.push('\\');
                }
                out.push('"');
                chars.next();
            }
            Some(&c) => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                out.push(c);
                chars.next();
            }
        }
    }
    out.push('"');
    out
}

/// Ctrl+C handler: sets the RUNNING flag so the poll loop exits and the LHM
/// child is reaped instead of surviving as an orphan.
unsafe extern "system" fn ctrl_handler(_ctrl_type: u32) -> windows::core::BOOL {
    RUNNING.store(false, Ordering::SeqCst);
    windows::core::BOOL(1)
}

/// Re-quote a full argument vector into one parameter string, quoting each token
/// with [`quote_arg`].
///
/// Used when relaunching elevated: the child re-parses the string, so a token
/// with spaces must keep its quotes or it splits into two arguments.
fn requote_args(args: &[String]) -> String {
    args.iter()
        .map(|a| quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// CLI help text.
fn usage() -> String {
    format!(
        "minihud {version} — LibreHardwareMonitor hardware-stats backend\n\
         \n\
         USAGE:\n\
         \x20   minihud                        print the hardware status line\n\
         \x20   minihud --hardware-poll <n>    set the poll cadence in ms (default {hw_poll})\n\
         \x20   minihud --stats                log the achieved updates/second once a second\n\
         \x20   minihud -h | --help            print this help\n\
         \n\
         Hardware stats come from the LibreHardwareMonitor bridge\n\
         (tools/lhm/lhm-bridge.ps1). Ctrl+C to stop.\n",
        version = env!("CARGO_PKG_VERSION"),
        hw_poll = DEFAULT_HW_POLL_MS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn hw_opts_default_to_500ms_and_parse_hardware_poll() {
        // Catches: the hardware cadence default drifting off 500 ms, or
        // `--hardware-poll` being ignored (the flag does nothing).
        let d = parse_hw_opts(&a(&[]));
        assert_eq!(d.poll_ms, 500, "the default hardware poll must be 500 ms");
        assert_eq!(d.poll_ms, DEFAULT_HW_POLL_MS);
        assert_eq!(
            parse_hw_opts(&a(&["--hardware-poll", "250"])).poll_ms,
            250,
            "--hardware-poll must set the cadence"
        );
    }

    #[test]
    fn hw_opts_keep_the_default_on_zero_unparseable_or_valueless() {
        // Catches: `--hardware-poll 0` busy-spinning the poll loop, or an
        // unparseable/valueless value clobbering the 500 ms default.
        let d = parse_hw_opts(&a(&[]));
        assert_eq!(parse_hw_opts(&a(&["--hardware-poll", "0"])), d);
        assert_eq!(parse_hw_opts(&a(&["--hardware-poll", "soon"])), d);
        assert_eq!(parse_hw_opts(&a(&["--hardware-poll"])), d);
    }

    #[test]
    fn removed_hardware_cadence_flags_are_not_recognized() {
        // Catches: a removed flag still parsed (a silent no-op that misleads the
        // user into thinking it changed the cadence) — each must leave the
        // default 500 ms poll intact.
        let d = parse_hw_opts(&a(&[]));
        for flag in ["--interval-ms", "--hw-idle-ms", "--bridge-ms"] {
            assert_eq!(
                parse_hw_opts(&a(&[flag, "50"])),
                d,
                "{flag} must be inert (removed)"
            );
        }
    }

    #[test]
    fn stats_is_opt_in_via_the_flag() {
        // Catches: the rate reporter running unasked, or `--stats` being inert.
        assert!(!parse_hw_opts(&a(&[])).stats, "must be off by default");
        assert!(parse_hw_opts(&a(&["--stats"])).stats);
    }

    #[test]
    fn removed_fps_and_overlay_flags_are_inert() {
        // Catches: a removed fps/overlay flag still changing the hardware
        // options, which would suggest the run does something it does not.
        let d = parse_hw_opts(&a(&[]));
        for flag in [
            "--gui",
            "--follow",
            "--stats-only",
            "--no-fps",
            "--fps-poll",
            "--match",
        ] {
            assert_eq!(parse_hw_opts(&a(&[flag])), d, "{flag} must be inert");
        }
    }

    #[test]
    fn rate_per_sec_divides_the_delta_by_the_interval() {
        // Catches: reporting a cumulative count instead of a rate, dividing by
        // the wrong interval, or a zero interval producing inf/NaN.
        assert_eq!(rate_per_sec(0, 10, 1.0), 10.0);
        assert_eq!(rate_per_sec(5, 15, 2.0), 5.0);
        assert_eq!(
            rate_per_sec(0, 5, 0.0),
            0.0,
            "a zero interval must not divide by zero"
        );
    }

    #[test]
    fn stats_line_reports_the_rate_and_the_retry_count() {
        // Catches: the diagnostics line dropping the retry counter (a wedged
        // LHM bridge would be invisible) or misformatting the rate.
        let line = stats_line(12.5, 7);
        assert!(line.contains("hw 12.5/s"), "{line}");
        assert!(line.contains("retries hw 7"), "{line}");
    }

    #[test]
    fn wait_for_due_returns_immediately_when_stopped() {
        // Catches: a shutdown that blocks a whole poll cadence (up to a large
        // --hardware-poll) before the LHM child is reaped, leaving an orphan.
        let stop = AtomicBool::new(true);
        let t = Instant::now();
        wait_for_due(&stop, Duration::from_secs(30));
        assert!(
            t.elapsed() < Duration::from_secs(1),
            "must not sleep a cadence when stopped: {:?}",
            t.elapsed()
        );
    }

    #[test]
    fn wait_for_due_sleeps_out_a_short_wait_while_running() {
        // Catches: a wait that returns early (the poller would busy-spin instead
        // of pacing to the cadence).
        let stop = AtomicBool::new(false);
        let t = Instant::now();
        wait_for_due(&stop, Duration::from_millis(150));
        assert!(
            t.elapsed() >= Duration::from_millis(140),
            "must wait the requested duration: {:?}",
            t.elapsed()
        );
    }

    #[test]
    #[cfg(windows)]
    fn ctrl_handler_clears_the_run_flag() {
        // Catches: Ctrl+C leaving the poll loop running, so the LHM feed is
        // never dropped and its PowerShell child survives the process.
        RUNNING.store(true, Ordering::SeqCst);
        assert!(RUNNING.load(Ordering::SeqCst), "the loop starts out running");
        let _ = unsafe { ctrl_handler(0) };
        assert!(!RUNNING.load(Ordering::SeqCst), "Ctrl+C must ask the poll loop to stop");
        RUNNING.store(true, Ordering::SeqCst);
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

    #[test]
    fn requote_args_preserves_a_spaced_token() {
        // Catches: the elevated relaunch joining args with a bare space, which
        // splits a spaced token into two arguments.
        let args = vec!["--hardware-poll".to_string(), "My Poll".to_string()];
        assert_eq!(requote_args(&args), r#"--hardware-poll "My Poll""#);
    }

    #[test]
    fn requote_args_leaves_plain_tokens_and_empty_args_intact() {
        // Catches: spurious quoting of plain tokens, or an empty arg collapsing
        // (which shifts every later argument on the relaunch).
        assert_eq!(requote_args(&a(&["--stats"])), "--stats");
        assert_eq!(
            requote_args(&a(&["--hardware-poll", "250"])),
            "--hardware-poll 250"
        );
        assert_eq!(requote_args(&a(&[""])), "\"\"");
    }

    #[test]
    fn usage_documents_the_surviving_flags_only() {
        // Catches: the help advertising a removed flag (it would lie about what
        // the program does) or failing to list the ones that remain.
        let u = usage();
        for kept in ["--hardware-poll", "--stats", "--help"] {
            assert!(u.contains(kept), "kept flag {kept} is undocumented: {u}");
        }
        for gone in ["--gui", "--follow", "--fps", "--match", "overlay", "ETW"] {
            assert!(
                !u.contains(gone),
                "removed flag/term {gone} still in help: {u}"
            );
        }
    }
}
