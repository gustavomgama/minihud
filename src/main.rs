//! minihud — hardware stats + ETW FPS backend.
//!
//! Prints LibreHardwareMonitor hardware stats on an interval (via
//! `tools/lhm/lhm-bridge.ps1`) and, by default, shows the foreground target's
//! FPS from **ETW present events** (tier 0, no injection) — see the `fps`
//! module. FPS/log/hardware output shares one console. There is no UI or
//! overlay; `--stats-only` shows hardware only.

mod console;
mod fps;
mod hook;
mod hw;
mod render;

use std::time::{Duration, Instant};

use windows::core::{Result, PCWSTR};
use windows::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Default tracing filter: everything at `info`. Overridable with `RUST_LOG`.
const DEFAULT_LOG_FILTER: &str = "info";

/// Active hardware poll cadence (ms) for the status line. The bridge reads
/// continuously (see [`hw::lhm::BRIDGE_POLL_MS`]); this only sets how often the
/// displayed values refresh. Override with `--interval-ms <n>`.
const DEFAULT_INTERVAL_MS: u64 = 500;
/// Idle backoff cap (ms): `1` means **no backoff** — the poller never slows
/// below the active cadence (the value is clamped up to
/// [`DEFAULT_INTERVAL_MS`]). Override with `--hw-idle-ms <n>`.
const DEFAULT_HW_IDLE_MS: u64 = 1;
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
        "minihud v{} — hardware stats + ETW fps (tier 0, no injection)",
        env!("CARGO_PKG_VERSION")
    );

    // `--stats`: a background line reporting the achieved status-update rate.
    if args.iter().any(|a| a == "--stats") {
        std::thread::spawn(stats_reporter);
    }

    let hw = parse_hw_opts(&args);
    let interval = hw.active_ms;
    let result = match select_mode(&args) {
        // Hardware stats only, never capture.
        Mode::StatsOnly => {
            ensure_elevated();
            stats_loop(hw);
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
            let fps = fps::parse_fps_opts(&args);
            report(
                "--follow",
                run_with_stats(hw, move || {
                    fps::follow::run(follow.match_glob, poll, follow.secs, fps)
                }),
            )
        }
    };
    console::finish();
    result
}

/// The hardware cadence knobs: active poll, idle backoff cap, and bridge tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HwOpts {
    active_ms: u64,
    idle_ms: u64,
    bridge_ms: u64,
}

impl Default for HwOpts {
    fn default() -> Self {
        Self {
            active_ms: DEFAULT_INTERVAL_MS,
            idle_ms: DEFAULT_HW_IDLE_MS,
            bridge_ms: hw::lhm::BRIDGE_POLL_MS,
        }
    }
}

/// Parse `--interval-ms <n>` (active), `--hw-idle-ms <n>` (idle cap), and
/// `--bridge-ms <n>` (LHM tick). A zero/unparseable/valueless flag keeps the
/// default; the idle cap is raised to the active cadence so the backoff stays
/// meaningful.
fn parse_hw_opts(args: &[String]) -> HwOpts {
    let mut o = HwOpts::default();
    let num = |flag: &str| {
        args.windows(2)
            .find(|w| w[0] == flag)
            .and_then(|w| w[1].parse::<u64>().ok())
            .filter(|&n| n > 0)
    };
    if let Some(n) = num("--interval-ms") {
        o.active_ms = n;
    }
    if let Some(n) = num("--hw-idle-ms") {
        o.idle_ms = n;
    }
    if let Some(n) = num("--bridge-ms") {
        o.bridge_ms = n;
    }
    o.idle_ms = o.idle_ms.max(o.active_ms);
    o
}

/// The run mode resolved from the CLI arguments.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    StatsOnly,
    Follow(hook::follow::FollowArgs),
}

/// Resolve the run mode: `--stats-only`, else the default `--follow`.
fn select_mode(args: &[String]) -> Mode {
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

/// Run a closure on this thread while the hardware-stats poller updates the
/// status line on the same console in the background, so the fps logging and
/// the hardware stats share one output.
fn run_with_stats<F>(hw: HwOpts, capture: F) -> std::result::Result<(), String>
where
    F: FnOnce() -> std::result::Result<(), String>,
{
    std::thread::spawn(move || stats_loop(hw));
    capture()
}

/// Poll the hardware sensors at the active cadence (backing off to idle when
/// they settle) and update the status line, forever. Sleeps exactly until the
/// next due poll (`HwPoller::due_in`) instead of a fixed slice, so an update is
/// never delayed by up to a slice.
fn stats_loop(opts: HwOpts) {
    let mut poller = hw::HwPoller::new(opts.active_ms, opts.idle_ms, opts.bridge_ms);
    loop {
        if poller.update().is_some() {
            console::status_hw(render::line(poller.cached()));
        }
        std::thread::sleep(poller.due_in());
    }
}

/// Log the achieved status-update rate (hardware and fps) once per second, so
/// "as fast as possible" is a measured number. Only spawned for `--stats`.
fn stats_reporter() {
    let period = Duration::from_secs(1);
    let mut prev = console::counters();
    let mut last = Instant::now();
    loop {
        std::thread::sleep(period);
        let cur = console::counters();
        let (hw, fps, ticks) = rate_per_sec(prev, cur, last.elapsed().as_secs_f64());
        tracing::info!("rate: hw {hw:.1}/s, fps {fps:.1}/s (fps tick {ticks:.0}/s)");
        prev = cur;
        last = Instant::now();
    }
}

/// Updates per second for `(hardware, fps, fps ticks)` between two counter
/// snapshots over `dt` seconds. A non-positive `dt` reports 0 rather than
/// dividing by zero. Pure, so the rate math is unit-tested.
fn rate_per_sec(prev: (u64, u64, u64), cur: (u64, u64, u64), dt: f64) -> (f64, f64, f64) {
    let rate = |a: u64, b: u64| {
        if dt > 0.0 {
            b.saturating_sub(a) as f64 / dt
        } else {
            0.0
        }
    };
    (
        rate(prev.0, cur.0),
        rate(prev.1, cur.1),
        rate(prev.2, cur.2),
    )
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

/// Re-quote a full argument vector into one parameter string, quoting each token
/// with [`quote_arg`].
///
/// Used when relaunching elevated: the child re-parses the string, so a token
/// with spaces (e.g. a `--match "My Game.exe"` glob) must keep its quotes or it
/// splits into two arguments.
fn requote_args(args: &[String]) -> String {
    args.iter()
        .map(|a| quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// CLI help text.
fn usage() -> String {
    format!(
        "minihud {version} — LibreHardwareMonitor hardware backend + ETW fps\n\
         \n\
         USAGE:\n\
         \x20   minihud                                  show the foreground target's fps + hardware stats (default)\n\
         \x20   minihud --stats-only                     print hardware stats only (no fps)\n\
         \x20   minihud --follow [--match <glob>] [--poll-ms <n>] [--follow-secs <n>]\n\
         \x20   minihud -h | --help                      print this help\n\
         \n\
         A <pid|name> is a numeric pid or a process image name (case-insensitive,\n\
         `.exe` optional: `Overwatch`, `overwatch.exe`, or `1234`).\n\
         \n\
         FPS (default; tier 0, ETW, never injects):\n\
         \x20   The default and --follow show the target's fps/frametime + 1% low from\n\
         \x20   ETW present events via a spawned PresentMon.exe — no dll injection or game\n\
         \x20   hook of any kind. If an anti-cheat blocks the ETW session the fps reads `--`\n\
         \x20   and the run continues.\n\
         \x20   --fps / --no-fps            enable / disable the ETW fps readout (default: on).\n\
         \x20   --fps-window-ms <n>        rolling fps window in ms (default {fps_window}). Any value\n\
         \x20                              >= 1 is honored literally; below ~1 frame of history the\n\
         \x20                              readout is meaningless, so a tiny window is your choice.\n\
         \x20   --fps-rate-ms <n>          fps status-line redraw cadence in ms (default {fps_tick}).\n\
         \x20   --fps-pid <pid|name>       explicit fps target (default: the followed/foreground\n\
         \x20                              target).\n\
         \x20   --follow                   follow the foreground target (or --match) and show its\n\
         \x20                              fps via ETW (needs elevation for ETW). This is the DEFAULT;\n\
         \x20                              Ctrl+C to stop.\n\
         \x20   --match <glob>             with --follow, target an exe by case-insensitive glob\n\
         \x20                              instead of the foreground process (e.g. deadlock*.exe).\n\
         \x20   --poll-ms <n>              with --follow, how often to re-check the target (ms,\n\
         \x20                              default: the --interval-ms value). The fps line redraws\n\
         \x20                              faster, on its own tick.\n\
         \x20   --follow-secs <n>          with --follow, stop after <n> seconds (default: until Ctrl+C).\n\
         \x20   --interval-ms <n>          active hardware-stats poll cadence in ms (default {interval}).\n\
         \x20   --hw-idle-ms <n>           idle backoff cap in ms once values settle (default {hw_idle} =\n\
         \x20                              no backoff; clamped up to --interval-ms).\n\
         \x20   --bridge-ms <n>            LHM bridge tick in ms — how often sensors are read\n\
         \x20                              (default {bridge}; 1 = read continuously — each read costs\n\
         \x20                              ~92 ms, so the sensor rate is ~10/s regardless).\n\
         \x20   --stats                    log the achieved hw/fps updates-per-second once a second.\n\
         \x20   --stats-only               print hardware stats only; never show fps.\n",
        version = env!("CARGO_PKG_VERSION"),
        interval = DEFAULT_INTERVAL_MS,
        hw_idle = DEFAULT_HW_IDLE_MS,
        bridge = hw::lhm::BRIDGE_POLL_MS,
        fps_window = fps::DEFAULT_FPS_WINDOW_MS,
        fps_tick = fps::DEFAULT_FPS_TICK_MS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn usage_documents_the_commands() {
        // Catches: a flag that exists but is not documented (the user cannot
        // discover it), or the tier-0 framing disappearing from the help text.
        let u = usage();
        assert!(u.contains("--follow"), "{u}");
        assert!(u.contains("--match"), "{u}");
        assert!(u.contains("--poll-ms"), "{u}");
        assert!(u.contains("--follow-secs"), "{u}");
        assert!(u.contains("--stats-only"), "{u}");
        assert!(u.contains("--interval-ms"), "{u}");
        assert!(u.contains("--fps"), "{u}");
        assert!(u.contains("--no-fps"), "{u}");
        assert!(u.contains("--fps-window-ms"), "{u}");
        assert!(u.contains("--fps-rate-ms"), "{u}");
        assert!(u.contains("--fps-pid"), "{u}");
        assert!(u.contains("--hw-idle-ms"), "{u}");
        assert!(u.contains("--bridge-ms"), "{u}");
        assert!(u.contains("--stats"), "{u}");
        assert!(u.contains("tier 0"), "{u}");
    }

    #[test]
    fn hw_opts_default_to_500ms_active_1ms_bridge_and_no_backoff() {
        // Catches: the cadence regressing from the intended 500 ms status-line
        // refresh / continuous 1 ms bridge reads, or `--hw-idle-ms` reverting to
        // a real backoff cap above the active interval.
        let o = parse_hw_opts(&a(&[]));
        assert_eq!(
            o.active_ms, 500,
            "the status line must show hardware every 500 ms"
        );
        assert_eq!(
            o.bridge_ms, 1,
            "the bridge must read continuously (1 ms tick)"
        );
        assert_eq!(
            o.idle_ms, o.active_ms,
            "idle (1) is clamped up to the active cadence, so there is no backoff"
        );
        assert_eq!(o.idle_ms, 500, "1 means no backoff, not a 1 ms idle cap");
    }

    #[test]
    fn hw_opts_parse_the_three_knobs_and_clamp_idle() {
        // Catches: `--hw-idle-ms`/`--bridge-ms` being ignored, a zero value
        // clobbering a default, or an idle cap below the active cadence (which
        // would make the backoff meaningless).
        let o = parse_hw_opts(&a(&[
            "--interval-ms",
            "150",
            "--hw-idle-ms",
            "800",
            "--bridge-ms",
            "120",
        ]));
        assert_eq!(o.active_ms, 150);
        assert_eq!(o.idle_ms, 800);
        assert_eq!(o.bridge_ms, 120);

        let clamped = parse_hw_opts(&a(&["--interval-ms", "900", "--hw-idle-ms", "300"]));
        assert_eq!(clamped.active_ms, 900);
        assert_eq!(clamped.idle_ms, 900, "idle is raised to the active cadence");

        // A zero/valueless/unparseable flag keeps the default cadence. Compare
        // against the parsed default (`HwOpts::default()` is pre-clamp, so with
        // idle=1 it differs from the effective idle=500).
        let bad = parse_hw_opts(&a(&["--interval-ms", "0", "--bridge-ms", "soon"]));
        assert_eq!(
            bad,
            parse_hw_opts(&a(&[])),
            "zero/unparseable must not clobber the default cadence"
        );
    }

    #[test]
    fn select_mode_defaults_to_follow_and_stats_only_wins() {
        // Catches: the default silently regressing to stats-only, or
        // `--stats-only` losing to the default follow. With the capture modes
        // removed there is no other precedence left to get wrong.
        use hook::follow::FollowArgs;
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(select_mode(&a(&[])), Mode::Follow(FollowArgs::default()));
        assert_eq!(
            select_mode(&a(&["--follow"])),
            Mode::Follow(FollowArgs::default())
        );
        assert_eq!(select_mode(&a(&["--stats-only"])), Mode::StatsOnly);
        // `--match` selects follow mode (it is the default) with the glob set.
        assert_eq!(
            select_mode(&a(&["--match", "deadlock*.exe"])),
            Mode::Follow(FollowArgs {
                match_glob: Some("deadlock*.exe".into()),
                poll_ms: hook::follow::DEFAULT_POLL_MS,
                secs: None,
            })
        );
    }

    #[test]
    fn rate_per_sec_divides_the_delta_by_the_interval() {
        // Catches: reporting a cumulative count instead of a rate, dividing by
        // the wrong interval, or a zero interval producing inf/NaN.
        assert_eq!(
            rate_per_sec((0, 0, 0), (10, 20, 30), 1.0),
            (10.0, 20.0, 30.0)
        );
        assert_eq!(rate_per_sec((5, 5, 5), (15, 9, 13), 2.0), (5.0, 2.0, 4.0));
        assert_eq!(
            rate_per_sec((0, 0, 0), (5, 5, 5), 0.0),
            (0.0, 0.0, 0.0),
            "a zero interval must not divide by zero"
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

    #[test]
    fn requote_args_preserves_a_spaced_token() {
        // Catches: the elevated relaunch joining args with a bare space, which
        // splits a spaced `--match "My Game.exe"` glob into two tokens so the
        // elevated instance follows the wrong target.
        let args = vec![
            "--follow".to_string(),
            "--match".to_string(),
            r"C:\Games\My Game.exe".to_string(),
        ];
        assert_eq!(
            requote_args(&args),
            r#"--follow --match "C:\Games\My Game.exe""#
        );
    }

    #[test]
    fn requote_args_leaves_plain_tokens_and_empty_args_intact() {
        // Catches: spurious quoting of plain tokens, or an empty arg collapsing
        // (which shifts every later argument on the relaunch).
        assert_eq!(requote_args(&a(&["--stats-only"])), "--stats-only");
        assert_eq!(requote_args(&a(&["--fps-pid", "1234"])), "--fps-pid 1234");
        assert_eq!(requote_args(&a(&[""])), "\"\"");
    }
}
