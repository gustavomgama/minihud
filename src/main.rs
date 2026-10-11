//! minihud — hardware stats + ETW FPS backend.
//!
//! Prints LibreHardwareMonitor hardware stats on an interval (via
//! `tools/lhm/lhm-bridge.ps1`) and, by default, shows the foreground target's
//! FPS from **ETW present events** (tier 0, no injection) — see the `fps`
//! module. FPS/log/hardware output shares one console. There is no UI or
//! overlay; `--stats-only` shows hardware only.

mod console;
mod fps;
mod gui;
mod hook;
mod hw;
mod render;
mod retry;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::{Result, PCWSTR};
use windows::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Default tracing filter: everything at `info`. Overridable with `RUST_LOG`.
const DEFAULT_LOG_FILTER: &str = "info";

/// Hardware poll cadence (ms): one value drives both the status-line refresh
/// and the LHM bridge tick. Override with `--hardware-poll <n>`.
const DEFAULT_HW_POLL_MS: u64 = 500;
/// Longest slice the shutdown-aware poll wait sleeps before re-checking the stop
/// flag, so exit reaps the LHM child promptly at any poll cadence.
const SHUTDOWN_POLL: Duration = Duration::from_millis(100);
/// fps samples kept for the overlay's Full-density sparkline.
#[cfg(feature = "gui")]
const FPS_HISTORY: usize = 120;
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
    let result = match select_mode(&args) {
        // Hardware stats only, never capture.
        Mode::StatsOnly => {
            ensure_elevated();
            stats_loop(hw, Arc::new(AtomicBool::new(false)));
            Ok(())
        }
        // The OSD overlay window (feature `gui`).
        Mode::Gui(match_glob) => {
            ensure_elevated();
            let fps_opts = fps::parse_fps_opts(&args);
            report("--gui", run_gui(hw, fps_opts, match_glob))
        }
        // Default: follow the foreground process (or `--match`), showing fps +
        // hardware stats on the same line. `--fps-poll` drives the target poll.
        Mode::Follow(follow) => {
            ensure_elevated();
            let fps = fps::parse_fps_opts(&args);
            report(
                "--follow",
                run_with_stats(hw, move || {
                    fps::follow::run(follow.match_glob, follow.secs, fps)
                }),
            )
        }
    };
    console::finish();
    result
}

/// The hardware cadence: one poll value drives the status-line refresh and the
/// LHM bridge tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HwOpts {
    poll_ms: u64,
}

impl Default for HwOpts {
    fn default() -> Self {
        Self {
            poll_ms: DEFAULT_HW_POLL_MS,
        }
    }
}

/// Parse `--hardware-poll <n>`. A zero/unparseable/valueless flag keeps the
/// default.
fn parse_hw_opts(args: &[String]) -> HwOpts {
    let mut o = HwOpts::default();
    if let Some(n) = args
        .windows(2)
        .find(|w| w[0] == "--hardware-poll")
        .and_then(|w| w[1].parse::<u64>().ok())
        .filter(|&n| n > 0)
    {
        o.poll_ms = n;
    }
    o
}

/// The run mode resolved from the CLI arguments.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    StatsOnly,
    Follow(hook::follow::FollowArgs),
    /// The overlay, carrying the optional `--match` glob it should follow.
    Gui(Option<String>),
}

/// Resolve the run mode: `--gui` wins, then `--stats-only`, else the default
/// `--follow`.
fn select_mode(args: &[String]) -> Mode {
    if args.iter().any(|a| a == "--gui") {
        return Mode::Gui(hook::follow::parse_follow(args).and_then(|f| f.match_glob));
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

/// Run a closure on this thread while the hardware-stats poller updates the
/// status line on the same console in the background, so the fps logging and
/// the hardware stats share one output.
///
/// After the capture returns, the stats thread is asked to stop and joined, so
/// its [`hw::HwPoller`] — and the LHM child it owns — is dropped deterministically
/// instead of being abandoned at process exit.
fn run_with_stats<F>(hw: HwOpts, capture: F) -> std::result::Result<(), String>
where
    F: FnOnce() -> std::result::Result<(), String>,
{
    let stop = Arc::new(AtomicBool::new(false));
    let worker = {
        let stop = stop.clone();
        std::thread::spawn(move || stats_loop(hw, stop))
    };
    let result = capture();
    stop.store(true, Ordering::SeqCst);
    let _ = worker.join();
    result
}

/// Run the OSD overlay: poll hardware and follow the target's fps in background
/// threads, and paint the window on this (the main) thread, which the windowing
/// system requires.
#[cfg(feature = "gui")]
fn run_gui(
    hw: HwOpts,
    fps_opts: fps::FpsOpts,
    match_glob: Option<String>,
) -> std::result::Result<(), String> {
    let cfg = gui::config::resolve();
    // A `--match` on the command line wins; else the config's pinned target.
    let match_glob = match_glob.or_else(|| cfg.target.clone());
    let data = Arc::new(std::sync::Mutex::new(gui::app::Snapshot::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let quit = Arc::new(AtomicBool::new(false));
    spawn_quit_hotkey(quit.clone());

    let hw_worker = {
        let data = data.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut poller = hw::HwPoller::new(hw.poll_ms);
            while !stop.load(Ordering::SeqCst) && console::should_run() {
                if poller.update().is_some() {
                    if let Ok(mut snap) = data.lock() {
                        snap.stats = poller.cached().clone();
                    }
                }
                std::thread::sleep(poller.due_in());
            }
        })
    };

    let fps_worker = {
        let data = data.clone();
        let stop = stop.clone();
        std::thread::spawn(move || fps_loop(&data, &stop, fps_opts, match_glob))
    };

    let result = gui::app::run(cfg, data, quit);
    stop.store(true, Ordering::SeqCst);
    let _ = hw_worker.join();
    let _ = fps_worker.join();
    result.map_err(|e| e.to_string())
}

/// Follow the foreground target over ETW (tier 0, no injection) and publish its
/// fps / 1% low into the overlay snapshot. Mirrors `fps::follow::run` without
/// the console output; the debounce and target resolution are the same tested
/// helpers.
#[cfg(feature = "gui")]
fn fps_loop(
    data: &std::sync::Mutex<gui::app::Snapshot>,
    stop: &AtomicBool,
    opts: fps::FpsOpts,
    match_glob: Option<String>,
) {
    use fps::follow::{feed_action, resolve_follow_target, FeedAction, FeedState};
    use fps::presentmon::PresentMonFeed;
    use fps::window::FpsSource;

    let own = std::process::id();
    let poll = Duration::from_millis(opts.poll_ms.max(1));
    let mut feed: Option<PresentMonFeed> = None;
    let mut state = FeedState::default();
    let mut current: u32 = 0;
    let mut next = Instant::now();
    let mut last_log: Option<String> = None;

    while !stop.load(Ordering::SeqCst) && console::should_run() {
        if Instant::now() >= next {
            next = Instant::now() + poll;
            let selected = if opts.enabled {
                resolve_follow_target(own, &match_glob, opts.pid.as_deref())
            } else {
                None
            };
            let action = feed_action(&mut state, selected.as_ref().map(|(pid, _)| *pid), current);
            match action {
                FeedAction::Start => {
                    if let Some((pid, exe)) = selected {
                        log_target(&mut last_log, format!("following {exe} (pid {pid})"));
                        feed = Some(PresentMonFeed::start(pid));
                        current = pid;
                    }
                }
                FeedAction::Stop => {
                    log_target(&mut last_log, "target gone".to_string());
                    feed = None;
                    current = 0;
                    // Clear the readout, so a dead target does not leave a stale
                    // fps frozen on the overlay.
                    if let Ok(mut snap) = data.lock() {
                        snap.fps = None;
                        snap.one_percent_low = None;
                        snap.fps_history.clear();
                    }
                }
                FeedAction::Keep => {}
            }
        }
        if let Some(f) = &feed {
            let fps = f.fps(fps::FPS_WINDOW_MS as f64);
            let low = f.one_percent_low();
            if let Ok(mut snap) = data.lock() {
                snap.fps = fps;
                snap.one_percent_low = low;
                if let Some(v) = fps {
                    snap.fps_history.push(v as f32);
                    if snap.fps_history.len() > FPS_HISTORY {
                        snap.fps_history.remove(0);
                    }
                }
            }
        }
        std::thread::sleep(poll);
    }
}

/// `--gui` needs the crate built with `--features gui`.
#[cfg(not(feature = "gui"))]
fn run_gui(
    _hw: HwOpts,
    _fps_opts: fps::FpsOpts,
    _match_glob: Option<String>,
) -> std::result::Result<(), String> {
    tracing::error!("--gui requires a build with the `gui` feature (cargo build --features gui)");
    std::process::exit(2);
}

/// Register a global quit hotkey (Ctrl+Alt+H) and set `quit` when it fires, so
/// a click-through overlay with no taskbar entry can still be closed.
#[cfg(feature = "gui")]
fn spawn_quit_hotkey(quit: Arc<AtomicBool>) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, VK_H,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

    /// Arbitrary id for the single hotkey this process registers.
    const HOTKEY_ID: i32 = 1;

    std::thread::spawn(move || {
        // SAFETY: a null window posts WM_HOTKEY to this thread's queue, which the
        // loop below drains. The arguments are plain values.
        let registered = unsafe {
            RegisterHotKey(
                None,
                HOTKEY_ID,
                MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
                VK_H.0 as u32,
            )
        };
        if let Err(e) = registered {
            tracing::warn!("overlay: could not register the Ctrl+Alt+H quit hotkey: {e}");
            return;
        }
        tracing::info!("overlay: press Ctrl+Alt+H to close");
        let mut msg = MSG::default();
        // SAFETY: `msg` is a valid, writable MSG; the loop stops on WM_QUIT (0).
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            if msg.message == WM_HOTKEY {
                quit.store(true, Ordering::SeqCst);
                return;
            }
        }
    });
}

/// Log an overlay target change once (the resolution repeats every tick).
#[cfg(feature = "gui")]
fn log_target(last: &mut Option<String>, msg: String) {
    if last.as_deref() != Some(msg.as_str()) {
        tracing::info!("overlay: {msg}");
        *last = Some(msg);
    }
}

/// Poll the hardware sensors at the configured cadence and update the status
/// line until `stop` is set or Ctrl+C. Owns the [`hw::HwPoller`] (and thus the
/// LHM feed), so returning drops it and reaps the child.
fn stats_loop(opts: HwOpts, stop: Arc<AtomicBool>) {
    let mut poller = hw::HwPoller::new(opts.poll_ms);
    while !stop.load(Ordering::SeqCst) && console::should_run() {
        if poller.update().is_some() {
            console::status_hw(render::line(poller.cached()));
        }
        wait_for_due(&stop, poller.due_in());
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
        tracing::info!(
            "{}",
            stats_line(
                hw,
                fps,
                ticks,
                retry::FPS_RETRIES.total(),
                retry::HW_RETRIES.total()
            )
        );
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

/// Format the `--stats` line: the achieved update rates plus the cumulative
/// per-feed retry counts, so a wedged dependency is visible live. Pure, so the
/// diagnostics content is unit-tested.
fn stats_line(hw: f64, fps: f64, ticks: f64, fps_retries: u64, hw_retries: u64) -> String {
    format!(
        "rate: hw {hw:.1}/s, fps {fps:.1}/s (fps tick {ticks:.0}/s); retries fps {fps_retries}, hw {hw_retries}"
    )
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
         \x20   minihud --gui [--match <glob>]           show the OSD overlay window (needs --features gui)\n\
         \x20   minihud --follow [--match <glob>] [--follow-secs <n>]\n\
         \x20   minihud -h | --help                      print this help\n\
         \n\
         A <pid|name> is a numeric pid or a process image name (case-insensitive,\n\
         `.exe` optional: `Overwatch`, `overwatch.exe`, or `1234`).\n\
         \n\
         FPS (default; tier 0, ETW, never injects):\n\
         \x20   The default and --follow show the target's fps/frametime + 1% low from\n\
         \x20   ETW present events via a spawned PresentMon.exe — no dll injection or game\n\
         \x20   hook of any kind. If an anti-cheat blocks the ETW session the fps reads `--`\n\
         \x20   and the run continues. The readout is the average fps over a fixed\n\
         \x20   {fps_window} ms window, refreshed every --fps-poll ms.\n\
         \x20   --fps / --no-fps            enable / disable the ETW fps readout (default: on).\n\
         \x20   --fps-poll <n>             fps refresh cadence in ms (default {fps_poll}): how often\n\
         \x20                              the readout redraws and the target is re-resolved. The\n\
         \x20                              average is always over a fixed {fps_window} ms window.\n\
         \x20   --fps-pid <pid|name>       explicit fps target (default: the followed/foreground\n\
         \x20                              target).\n\
         \x20   --follow                   follow the foreground target (or --match) and show its\n\
         \x20                              fps via ETW (needs elevation for ETW). This is the DEFAULT;\n\
         \x20                              Ctrl+C to stop.\n\
         \x20   --match <glob>             with --follow or --gui, target an exe by case-insensitive glob\n\
         \x20                              instead of the foreground process (e.g. deadlock*.exe).\n\
         \x20   --follow-secs <n>          with --follow, stop after <n> seconds (default: until Ctrl+C).\n\
         \x20   --hardware-poll <n>        hardware-stats poll cadence in ms (default {hw_poll}). One\n\
         \x20                              value drives both the status-line refresh and the LHM\n\
         \x20                              bridge tick.\n\
         \x20   --stats                    log the achieved hw/fps updates-per-second once a second.\n\
         \x20   --stats-only               print hardware stats only; never show fps.\n",
        version = env!("CARGO_PKG_VERSION"),
        hw_poll = DEFAULT_HW_POLL_MS,
        fps_poll = fps::DEFAULT_FPS_POLL_MS,
        fps_window = fps::FPS_WINDOW_MS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn select_mode_recognizes_gui_and_lets_it_win() {
        // Catches: `--gui` being ignored (the overlay never launches) or losing
        // to `--stats-only` when both are passed.
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(select_mode(&a(&["--gui"])), Mode::Gui(None));
        assert_eq!(select_mode(&a(&["--gui", "--stats-only"])), Mode::Gui(None));
    }

    #[test]
    fn gui_carries_a_match_glob() {
        // Catches: `--gui --match <glob>` silently ignoring the glob (the
        // overlay would follow the foreground instead of the named target).
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            select_mode(&a(&["--gui", "--match", "deadlock*.exe"])),
            Mode::Gui(Some("deadlock*.exe".to_string()))
        );
        assert_eq!(select_mode(&a(&["--gui"])), Mode::Gui(None));
    }

    #[test]
    fn usage_lists_the_two_poll_flags_and_no_removed_cadence_flags() {
        // Catches: a removed cadence flag still documented (the help lies about
        // a flag that no longer exists) or the two surviving poll flags missing
        // (undiscoverable). Also pins the kept flags so a rename cannot silently
        // drop one from the help.
        let u = usage();
        assert!(
            u.contains("--hardware-poll"),
            "usage must list --hardware-poll: {u}"
        );
        assert!(u.contains("--fps-poll"), "usage must list --fps-poll: {u}");
        for gone in [
            "--interval-ms",
            "--hw-idle-ms",
            "--bridge-ms",
            "--fps-rate-ms",
            "--fps-window-ms",
            "--poll-ms",
            "--fps-instant",
            "--fps-window",
        ] {
            assert!(
                !u.contains(gone),
                "removed flag {gone} is still documented: {u}"
            );
        }
        for kept in [
            "--follow",
            "--match",
            "--follow-secs",
            "--stats-only",
            "--no-fps",
            "--fps-pid",
            "--stats",
        ] {
            assert!(u.contains(kept), "kept flag {kept} is undocumented: {u}");
        }
        assert!(u.contains("tier 0"), "{u}");
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
    fn stats_line_reports_rates_and_per_feed_retry_counts() {
        // Catches: the diagnostics line dropping the per-feed retry counters
        // (a wedged dependency would be invisible) or misformatting the rates.
        let line = stats_line(12.5, 60.0, 60.0, 3, 7);
        assert!(line.contains("hw 12.5/s"), "{line}");
        assert!(line.contains("fps 60.0/s"), "{line}");
        assert!(line.contains("fps tick 60/s"), "{line}");
        assert!(line.contains("retries fps 3, hw 7"), "{line}");
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
