//! minihud — LibreHardwareMonitor hardware-stats backend for Windows.
//!
//! Reads CPU/GPU/RAM sensors through the LibreHardwareMonitor bridge
//! (`tools/lhm/lhm-bridge.ps1`) and prints one stats line per poll. This is the
//! whole product: it captures no fps, injects nothing, and draws no overlay.

mod console;
mod diag;
mod hw;
mod render;
mod retry;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::Layer as _;
use windows::core::{Result, PCWSTR};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Default tracing filter: everything at `info`. Overridable with `RUST_LOG`.
const DEFAULT_LOG_FILTER: &str = "info";
/// The live filter `--diagnostics` selects when `RUST_LOG` is unset.
const DIAGNOSTICS_LOG_FILTER: &str = "trace";

/// Hardware poll cadence (ms): one value drives both the stats-line refresh
/// and the LHM bridge tick. Override with `--hardware-poll <n>`.
const DEFAULT_HW_POLL_MS: u64 = 500;
/// Longest slice the shutdown-aware poll wait sleeps before re-checking the stop
/// flag, so exit reaps the LHM child promptly at any poll cadence.
const SHUTDOWN_POLL: Duration = Duration::from_millis(100);
/// Longest `--dump-inventory` waits for the bridge's first sample before giving
/// up: the sidecar loads the LHM assembly and warms its sensors before it
/// answers, which takes a second or two even on a fast machine.
const DUMP_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the dump wait re-checks the feed while it has no sample.
const DUMP_POLL: Duration = Duration::from_millis(50);
/// A dump older than this counts as missing in the inventory wait, the same
/// staleness rule the stats poller applies.
const DUMP_SAMPLE_AGE: Duration = Duration::from_secs(3);
/// ShellExecuteW reports success as a value greater than 32 (Win32 docs).
const SHELLEXECUTE_SUCCESS: isize = 32;

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

    let hw = parse_hw_opts(&args);

    // The console layer shows what a user sees: the app's single-line format,
    // with the span scope included only when `--diagnostics` asks for the live
    // trace surface, filtered to [`live_level`]. The diagnostics layer beside
    // it takes every event at every depth, so the persisted record is complete
    // whatever the console filter does.
    let console = tracing_subscriber::fmt::layer()
        .event_format(diag::console::ConsoleFormat::new(
            console::CompactTime,
            hw.diagnostics,
        ))
        .with_writer(console::LogMakeWriter)
        .with_filter(tracing_subscriber::EnvFilter::new(live_level(
            hw.diagnostics,
            std::env::var("RUST_LOG").ok().as_deref(),
        )));
    tracing_subscriber::registry()
        .with(diag::layer())
        .with(console)
        .init();
    console::enable_vt();
    console::install_ctrl_handler();
    // Register main's own Ctrl+C flag too: the `--dump-inventory` wait loop
    // blocks on `RUNNING`, and only a handler that clears it can interrupt it.
    // SAFETY: SetConsoleCtrlHandler with a valid signature; process-global handler.
    let _ = unsafe {
        SetConsoleCtrlHandler(
            Some(ctrl_handler),
            true, // add handler
        )
    };
    tracing::info!("{}", banner(env!("CARGO_PKG_VERSION")));

    let elevated = unsafe { IsUserAnAdmin() }.as_bool();
    if ensure_elevated(elevated) {
        // The elevated copy takes over. This process never opened a run, so it
        // leaves no record of its own — only the run that survives does.
        // SAFETY: nothing is held that a destructor must release.
        std::process::exit(0);
    }

    // Every run persists its own record: the artifacts land in a `diagnostics`
    // folder next to the exe, so `cargo clean` reclaims them. Opened here, after
    // the elevation decision, so the process that hands off leaves no folder.
    diag::begin(&args, elevated);
    // Closes the run's record on the way out; the run above is the only path
    // that exits without it, and it has no record to close.
    let _stop = diag::lifecycle::StopGuard::new(diag::sink());

    // `--stats`: a background line reporting the achieved status-update rate.
    if hw.stats {
        std::thread::spawn(stats_reporter);
    }
    // `--dump-inventory`: print the complete bridge inventory once and exit. A
    // development-inspection surface; the shipped status line never depends on
    // it. Dropping the feed below reaps the bridge child.
    if hw.dump {
        dump_inventory(hw.poll_ms);
        return Ok(());
    }
    stats_loop(hw);
    Ok(())
}

/// The startup log line. ASCII only: on a Windows console with a legacy
/// code page, an em-dash renders as mojibake ("ΓÇö").
fn banner(version: &str) -> String {
    format!("minihud v{version} - LibreHardwareMonitor hardware stats")
}

/// The hardware capture options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HwOpts {
    poll_ms: u64,
    /// Whether to run the `--stats` rate reporter.
    stats: bool,
    /// Whether to dump the whole bridge inventory and exit.
    dump: bool,
    /// Whether to enable the live trace surface. Developer-only: it raises the
    /// console filter and shows span scopes, and changes nothing else.
    diagnostics: bool,
}

impl Default for HwOpts {
    fn default() -> Self {
        Self {
            poll_ms: DEFAULT_HW_POLL_MS,
            stats: false,
            dump: false,
            diagnostics: false,
        }
    }
}

/// The live stream's log level: an explicit `RUST_LOG` always wins, then
/// `--diagnostics` raises the floor to trace, else the default `info`. Pure, so
/// the precedence is unit-tested.
fn live_level(diagnostics: bool, rust_log: Option<&str>) -> String {
    match rust_log {
        Some(level) if !level.is_empty() => level.to_string(),
        _ if diagnostics => DIAGNOSTICS_LOG_FILTER.to_string(),
        _ => DEFAULT_LOG_FILTER.to_string(),
    }
}

/// Parse `--hardware-poll <n>`, `--stats`, `--dump-inventory` and
/// `--diagnostics`. A zero/unparseable/valueless poll keeps the default.
fn parse_hw_opts(args: &[String]) -> HwOpts {
    HwOpts {
        poll_ms: cli_poll_ms(args).unwrap_or(DEFAULT_HW_POLL_MS),
        stats: args.iter().any(|a| a == "--stats"),
        dump: args.iter().any(|a| a == "--dump-inventory"),
        diagnostics: args.iter().any(|a| a == "--diagnostics"),
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
///
/// The line updates in place (`console::status_hw`, `\r` redraw) instead of
/// scrolling a new line per poll — one self-updating status line, logs
/// scrolling above it. `console::finish()` moves off it so the shell prompt
/// does not overwrite it.
///
/// Every cycle runs inside a `poll` span, so each poll's sensor dump, render
/// stats and timings are attributed to it in the persisted record (and shown
/// live under `--diagnostics`).
fn stats_loop(opts: HwOpts) {
    let sink = diag::sink();
    let mut poller = hw::HwPoller::new(opts.poll_ms);
    let mut poll: u64 = 0;
    // Both run flags gate the loop: `console::should_run` is console's Ctrl+C
    // flag, `RUNNING` main's. Either one clearing asks the loop to stop.
    while console::should_run() && RUNNING.load(Ordering::SeqCst) {
        // One span per poll cycle: every event and timing of the cycle is
        // attributed to it in the record (and shown live with --diagnostics).
        let _cycle = tracing::info_span!("poll", poll_ms = opts.poll_ms).entered();
        if poller.update().is_some() {
            let line = render::line(poller.cached());
            diag::sensor_poll(sink, poll, poller.last_dump());
            console::status_hw(line.clone());
            diag::render_stat(sink, &line, poller.cached());
            poll += 1;
        }
        let due = poller.due_in();
        wait_for_due(&RUNNING, due);
    }
    console::finish();
}

/// `--dump-inventory`: print every sensor the bridge reports, once, and exit.
/// This is the development-inspection surface for the complete LHM data set —
/// coverage is checked against LHM directly instead of by reading the curated
/// status line, which only shows one field per group.
fn dump_inventory(poll_ms: u64) {
    let _dump = tracing::info_span!("dump_inventory", poll_ms = poll_ms).entered();
    let feed = hw::LhmFeed::start_with_poll(poll_ms);
    let Some(sensors) = wait_for_sample(|| feed.latest(DUMP_SAMPLE_AGE), &RUNNING, DUMP_TIMEOUT)
    else {
        eprintln!("{}", dump_timeout_notice(DUMP_TIMEOUT.as_secs()));
        return;
    };
    print!("{}", hw::inventory::render(&sensors));
    diag::sensor_poll(diag::sink(), 0, &sensors);
}

/// Wait for the first sample `sample` produces, re-checking every
/// [`DUMP_POLL`], until `timeout` elapses or the run is interrupted. `None`
/// means nothing arrived, so a wedged bridge is reported rather than hanging the
/// run.
fn wait_for_sample(
    sample: impl Fn() -> Option<Vec<hw::LhmSensor>>,
    running: &AtomicBool,
    timeout: Duration,
) -> Option<Vec<hw::LhmSensor>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(sensors) = sample() {
            return Some(sensors);
        }
        if !running.load(Ordering::SeqCst) || Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(DUMP_POLL);
    }
}

/// Log the achieved stats-update rate once per second, so "as fast as
/// possible" is a measured number. Only spawned for `--stats`.
fn stats_reporter() {
    let period = Duration::from_secs(1);
    let mut prev = console::hw_updates();
    let mut last = Instant::now();
    loop {
        std::thread::sleep(period);
        let _tick = tracing::info_span!("stats_tick").entered();
        let cur = console::hw_updates();
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

/// Sleep until `due` from now, waking early when `running` is cleared (the
/// Ctrl+C handlers set their flag false). Bounded so shutdown reaps the LHM
/// child promptly even at a large `--hardware-poll`.
///
/// The flag is TRUE while the app runs: reading it as a stop flag would make
/// the wait return at once and the poll loop spin at full tilt — see
/// `the_wait_treats_the_run_flag_as_running`.
fn wait_for_due(running: &AtomicBool, due: Duration) {
    let deadline = Instant::now() + due;
    loop {
        if !running.load(Ordering::SeqCst) {
            return;
        }
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let d = (deadline - now).min(SHUTDOWN_POLL);
        std::thread::sleep(d);
    }
}

/// The `--dump-inventory` no-sample message. ASCII only (see [`banner`]).
fn dump_timeout_notice(secs: u64) -> String {
    format!("no LHM sample within {secs}s - is the bridge staged next to this exe?")
}

/// Why the console just flashed with nothing readable: this process relaunches
/// itself elevated for Super I/O access, and the elevated copy prints into a
/// fresh console that dies with it. The opt-out keeps output in this console,
/// at the cost of `--` for CPU power/temp. ASCII only (see [`banner`]).
fn elevation_notice() -> &'static str {
    "relaunching elevated for full sensors (CPU temp/power need admin); \
     set MINIHUD_NO_ELEVATE=1 to stay in this console"
}

/// Ask for UAC elevation when not already elevated, so CPU temp/power
/// (SuperIO) and the LHM bridge work. Relaunches self via ShellExecuteW
/// "runas". Returns true when the elevated copy has taken over and this process
/// must exit; a refusal falls back to running unelevated.
/// `MINIHUD_NO_ELEVATE=1` skips this (headless tests/diagnostics).
fn ensure_elevated(elevated: bool) -> bool {
    let skip = std::env::var_os("MINIHUD_NO_ELEVATE").is_some();
    if !needs_elevation(skip, elevated) {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let params = requote_args(&std::env::args().skip(1).collect::<Vec<_>>());
    let verb = wstr("runas");
    let file = wstr(&exe.to_string_lossy());
    let params_w = wstr(&params);
    // Say it before relaunching: the elevated copy prints into its own console.
    eprintln!("{}", elevation_notice());
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
        return true;
    }
    tracing::warn!("elevation refused; running unelevated (CPU temp/power will read --)");
    false
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
unsafe extern "system" fn ctrl_handler(t: u32) -> windows::core::BOOL {
    let _ = t;
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
        "minihud {version} - LibreHardwareMonitor hardware-stats backend\n\
          \n\
          USAGE:\n\
          \x20   minihud                        print the hardware status line\n\
          \x20   minihud --hardware-poll <n>    set the poll cadence in ms (default {hw_poll})\n\
          \x20   minihud --dump-inventory       print every sensor the bridge reports, then exit\n\
          \x20   minihud --stats                log the achieved updates/second once a second\n\
          \x20   minihud --diagnostics          developer: trace-level live stream plus the\n\
          \x20                                  per-run diagnostics record next to the exe\n\
          \x20   minihud -h | --help            print this help\n\
          \n\
          Hardware stats come from the LibreHardwareMonitor bridge\n\
          (tools/lhm/lhm-bridge.ps1). Every run writes its diagnostics record\n\
          (a verbose log and a JSONL) next to the exe; Ctrl+C to stop.\n",
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

    /// Catches: em-dash / smart quotes in user-visible output. On a Windows
    /// console with a legacy code page they render as mojibake ("ΓÇö"), which
    /// is what the user saw instead of a readable startup line.
    #[test]
    fn every_user_visible_message_is_ascii_only() {
        let messages = [
            usage(),
            banner(env!("CARGO_PKG_VERSION")),
            dump_timeout_notice(5),
            elevation_notice().to_string(),
        ];
        for m in messages {
            assert!(
                m.is_ascii(),
                "non-ASCII bytes in a user-visible message: {m:?}"
            );
        }
    }

    /// Catches the silent-relaunch blind spot: the elevated copy prints into
    /// a fresh console that dies with the process, so the notice must name
    /// the opt-out env var or the user watches a console flash with nothing
    /// to read.
    #[test]
    fn the_elevation_notice_names_the_opt_out_and_the_reason() {
        let n = elevation_notice();
        assert!(n.contains("MINIHUD_NO_ELEVATE"), "no opt-out named: {n}");
        assert!(n.contains("elevat"), "no reason given: {n}");
    }

    /// One bridge sensor row for the dump tests (same shape the sidecar emits).
    fn sensor(hw: &str, ty: &str, name: &str, value: f64) -> hw::LhmSensor {
        hw::LhmSensor {
            hw: hw.to_string(),
            sensor_type: ty.to_string(),
            name: name.to_string(),
            value,
        }
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
    fn dump_inventory_is_opt_in_via_the_flag() {
        // Catches: `--dump-inventory` being inert (a developer cannot inspect
        // what the bridge actually reported) or dumping unasked on a normal
        // stats run, which would spam the console once and exit.
        let d = parse_hw_opts(&a(&[]));
        assert!(!d.dump, "must be off by default");
        assert!(
            parse_hw_opts(&a(&["--dump-inventory"])).dump,
            "--dump-inventory must be recognized"
        );
        assert!(
            !parse_hw_opts(&a(&["--stats"])).dump,
            "the stats reporter must not imply a dump"
        );
    }

    #[test]
    fn removed_overlay_flags_still_do_not_trigger_a_dump() {
        // Catches: a removed overlay/fps flag being read as the dump flag, so a
        // run would dump the inventory and exit instead of printing stats.
        for flag in ["--gui", "--follow", "--no-fps", "--stats-only"] {
            assert!(
                !parse_hw_opts(&a(&[flag])).dump,
                "{flag} must not request a dump"
            );
        }
    }

    /// Catches: `--diagnostics` being silently ignored, so the live trace
    /// surface can never be switched on, or another dev flag implying it.
    #[test]
    fn parse_accepts_diagnostics_alongside_the_existing_flags() {
        let o = parse_hw_opts(&a(&["--stats", "--diagnostics", "--hardware-poll", "250"]));
        assert!(o.diagnostics, "--diagnostics must parse");
        assert!(o.stats);
        assert!(!o.dump);
        assert_eq!(o.poll_ms, 250);

        let only = parse_hw_opts(&a(&["--diagnostics"]));
        assert!(only.diagnostics);
        assert!(!only.stats, "must not imply --stats");
        assert!(!only.dump, "must not imply --dump-inventory");
        assert_eq!(only.poll_ms, DEFAULT_HW_POLL_MS);

        assert!(
            !parse_hw_opts(&a(&["--stats", "--dump-inventory"])).diagnostics,
            "the record is persisted on every run; the flag only opens the live surface"
        );
    }

    /// Catches: the flag leaking into the live level when RUST_LOG is set
    /// (an explicit filter must win), or the live surface staying at info so
    /// `--diagnostics` shows no trace output at all.
    #[test]
    fn live_level_prefers_rust_log_then_the_flag_then_info() {
        assert_eq!(live_level(true, Some("warn")), "warn");
        assert_eq!(live_level(true, None), "trace");
        assert_eq!(live_level(true, Some("")), "trace", "empty means unset");
        assert_eq!(live_level(false, None), DEFAULT_LOG_FILTER);
        assert_eq!(live_level(false, Some("debug")), "debug");
    }

    /// Catches: a new dev flag that no help text documents.
    #[test]
    fn usage_documents_the_diagnostics_flag() {
        assert!(usage().contains("--diagnostics"), "{}", usage());
    }

    #[test]
    fn wait_for_sample_returns_the_first_sample_it_sees() {
        // Catches: the dump ignoring a sample that is already available (so the
        // flag prints nothing even though the bridge answered).
        let running = AtomicBool::new(true);
        let got = wait_for_sample(
            || Some(vec![sensor("Cpu:X", "Load", "CPU Total", 42.0)]),
            &running,
            DUMP_TIMEOUT,
        );
        let sensors = got.expect("an available sample must be returned");
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].value, 42.0);
    }

    #[test]
    fn wait_for_sample_waits_for_a_sample_that_arrives_late() {
        // Catches: the dump wait giving up at once because the run flag was
        // read as a stop flag (a run that is `true` = "running" would look like
        // "stop"), so `--dump-inventory` would always print nothing.
        let running = AtomicBool::new(true);
        let t = Instant::now();
        // Nothing for the first four polls, then a sample: the wait must span
        // them all rather than return on the first empty poll.
        let polls = std::sync::atomic::AtomicUsize::new(0);
        let got = wait_for_sample(
            || {
                let n = polls.fetch_add(1, Ordering::SeqCst);
                (n >= 4).then(|| vec![sensor("Cpu:X", "Load", "CPU Total", 7.0)])
            },
            &running,
            Duration::from_secs(30),
        );
        let sensors = got.expect("a late sample must still be returned");
        assert_eq!(sensors[0].value, 7.0);
        assert!(
            t.elapsed() >= Duration::from_millis(200),
            "a late sample must be waited for, not given up on: {:?}",
            t.elapsed()
        );
    }

    #[test]
    fn wait_for_sample_gives_up_after_the_timeout_without_hanging() {
        // Catches: a bridge that never answers (missing DLL, PowerShell error)
        // hanging the CLI dump forever with no output.
        let running = AtomicBool::new(true);
        let t = Instant::now();
        let got = wait_for_sample(|| None, &running, Duration::from_millis(60));
        assert!(got.is_none(), "no sample must give up");
        assert!(
            t.elapsed() >= Duration::from_millis(50),
            "must wait out the timeout before giving up: {:?}",
            t.elapsed()
        );
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "must return promptly after the timeout: {:?}",
            t.elapsed()
        );
    }

    #[test]
    fn wait_for_sample_stops_when_the_run_is_interrupted() {
        // Catches: Ctrl+C during a dump wait blocking until the full timeout
        // (and keeping the bridge child alive) instead of returning at once.
        let running = AtomicBool::new(false);
        let t = Instant::now();
        assert!(
            wait_for_sample(|| None, &running, Duration::from_secs(30)).is_none(),
            "a stopped run must not wait out the timeout"
        );
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "must return promptly once stopped: {:?}",
            t.elapsed()
        );
    }

    #[test]
    fn usage_documents_the_dump_flag() {
        // Catches: the help omitting the dump flag, so the one way to inspect
        // the complete LHM inventory stays undiscoverable.
        let u = usage();
        assert!(
            u.contains("--dump-inventory"),
            "the dump flag must be documented: {u}"
        );
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

    /// Catches: the wait's flag being read as "stopped" while the app's run
    /// flag says "running" (true = running). The wait then returned at once and
    /// the poll loop spun at full tilt — a core burned on every run, and, once
    /// diagnostics persisted a span per iteration, ~30k identical span timings
    /// per second in the record.
    #[test]
    fn the_wait_treats_the_run_flag_as_running() {
        let running = AtomicBool::new(true);
        let t = Instant::now();
        wait_for_due(&running, Duration::from_millis(200));
        assert!(
            t.elapsed() >= Duration::from_millis(190),
            "a running app must wait out the due time: {:?}",
            t.elapsed()
        );
    }

    #[test]
    fn wait_for_due_returns_immediately_when_stopped() {
        // Catches: a shutdown that blocks a whole poll cadence (up to a large
        // --hardware-poll) before the LHM child is reaped, leaving an orphan.
        let running = AtomicBool::new(false);
        let t = Instant::now();
        wait_for_due(&running, Duration::from_secs(30));
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
        let running = AtomicBool::new(true);
        let t = Instant::now();
        wait_for_due(&running, Duration::from_millis(150));
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
        assert!(
            RUNNING.load(Ordering::SeqCst),
            "the loop starts out running"
        );
        let _ = unsafe { ctrl_handler(0) };
        assert!(
            !RUNNING.load(Ordering::SeqCst),
            "Ctrl+C must ask the poll loop to stop"
        );
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
