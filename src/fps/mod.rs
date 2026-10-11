//! Tier-0 FPS capture (out-of-process, no injection).
//!
//! The default FPS source is ETW present events, collected by a spawned
//! `PresentMon.exe` and parsed from its stdout — the same technique as
//! PresentMon/EasyFPS/FpsOverlayer. Nothing is injected into the target, so it
//! is anti-cheat-safe by construction.
//!
//! - [`window`] — the pure rolling frametime window and fps / 1%-low math.
//! - [`presentmon`] — the `PresentMon.exe` sidecar feed (spawn, lenient CSV
//!   parse, graceful degradation).
//! - [`follow`] — the default/`--follow` orchestrator (target → PresentMon →
//!   status line), with no injection.

pub mod follow;
pub mod presentmon;
pub mod window;

/// Default trailing FPS window (ms). There is no floor: this is the effective
/// default, and any `--fps-window-ms <n>` with `n > 0` is honored literally.
/// Below ~1 frame of history the readout degrades into meaningless noise, which
/// the user accepts for a deliberately tiny requested window.
pub const DEFAULT_FPS_WINDOW_MS: u64 = 100;

/// Default fps status-line redraw cadence (ms). Half a second keeps the counter
/// steady; a faster tick redraws an unchanged number and burns CPU without
/// showing anything new.
pub const DEFAULT_FPS_TICK_MS: u64 = 500;

/// FPS-display options from the CLI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FpsOpts {
    /// Show the ETW fps readout (default on). `--no-fps` disables it.
    pub enabled: bool,
    /// Rolling fps window in ms.
    pub window_ms: u64,
    /// Status-line redraw cadence in ms (`--fps-rate-ms`).
    pub tick_ms: u64,
    /// Explicit fps target (`--fps-pid <pid|name>`); default: the followed
    /// (foreground/`--match`) target.
    pub pid: Option<String>,
}

impl Default for FpsOpts {
    fn default() -> Self {
        Self {
            enabled: true,
            window_ms: DEFAULT_FPS_WINDOW_MS,
            tick_ms: DEFAULT_FPS_TICK_MS,
            pid: None,
        }
    }
}

/// Parse the fps flags: `--fps`/`--no-fps`, `--fps-window-ms <n>`,
/// `--fps-rate-ms <n>`, `--fps-pid <pid|name>`. A valueless/non-positive/
/// unparseable numeric value keeps the default; the last occurrence of a flag
/// wins.
pub fn parse_fps_opts(args: &[String]) -> FpsOpts {
    let mut opts = FpsOpts::default();
    for (i, a) in args.iter().enumerate() {
        match a.as_str() {
            "--no-fps" => opts.enabled = false,
            "--fps" => opts.enabled = true,
            "--fps-window-ms" => {
                if let Some(n) = args
                    .get(i + 1)
                    .and_then(|s| s.parse::<u64>().ok())
                    .filter(|&n| n > 0)
                {
                    opts.window_ms = n;
                }
            }
            "--fps-rate-ms" => {
                if let Some(n) = args
                    .get(i + 1)
                    .and_then(|s| s.parse::<u64>().ok())
                    .filter(|&n| n > 0)
                {
                    opts.tick_ms = n;
                }
            }
            "--fps-pid" => {
                if let Some(v) = args.get(i + 1).filter(|v| !v.is_empty()) {
                    opts.pid = Some(v.clone());
                }
            }
            _ => {}
        }
    }
    opts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn fps_defaults_to_enabled_on_the_100ms_default_window_with_no_explicit_target() {
        // Catches: fps off by default (the whole point of this change), a zero
        // window (no fps can be computed from an empty window), or the default
        // regressing to the raw below-the-old-floor constant (1 ms) instead of
        // the intended 100 ms window.
        let d = parse_fps_opts(&args(&[]));
        assert_eq!(d, FpsOpts::default());
        assert!(d.enabled);
        assert_eq!(
            d.window_ms, 100,
            "the default window must be 100 ms, was {}",
            d.window_ms
        );
        assert_eq!(d.window_ms, DEFAULT_FPS_WINDOW_MS);
        assert_eq!(d.pid, None);
    }

    #[test]
    fn the_default_window_is_100ms_with_no_hidden_floor() {
        // Catches: DEFAULT_FPS_WINDOW_MS reverting to a raw below-100 constant
        // (e.g. 1) that only looked like a 100 ms window because a clamp used to
        // raise it — with the clamp gone it would ship as a 1 ms window.
        assert_eq!(
            DEFAULT_FPS_WINDOW_MS, 100,
            "the raw default constant must name the real 100 ms default"
        );
        assert_eq!(
            FpsOpts::default().window_ms,
            DEFAULT_FPS_WINDOW_MS,
            "the default must be the raw constant directly, with no clamp"
        );
    }

    #[test]
    fn fps_defaults_to_a_500ms_redraw_tick() {
        // Catches: the redraw tick regressing to the old ~16 ms cadence instead
        // of the intended 500 ms, which burns CPU redrawing an unchanged number.
        let d = parse_fps_opts(&args(&[]));
        assert_eq!(
            d.tick_ms, 500,
            "the default redraw tick must be 500 ms, was {}",
            d.tick_ms
        );
        assert_eq!(d.tick_ms, DEFAULT_FPS_TICK_MS);
    }

    #[test]
    fn parses_the_tick_and_rejects_bad_values() {
        // Catches: a zero/unparseable `--fps-rate-ms` clobbering the default (a
        // 0ms tick would busy-spin the display loop).
        assert_eq!(parse_fps_opts(&args(&["--fps-rate-ms", "20"])).tick_ms, 20);
        assert_eq!(
            parse_fps_opts(&args(&["--fps-rate-ms", "0"])).tick_ms,
            DEFAULT_FPS_TICK_MS
        );
        assert_eq!(
            parse_fps_opts(&args(&["--fps-rate-ms"])).tick_ms,
            DEFAULT_FPS_TICK_MS
        );
    }

    #[test]
    fn a_small_window_is_honored_literally() {
        // Catches: removing the floor but silently still clamping (a leftover
        // `.max(MIN_FPS_WINDOW_MS)`) so `--fps-window-ms 1` comes back as 100
        // instead of the 1 ms the user asked for.
        assert_eq!(
            parse_fps_opts(&args(&["--fps-window-ms", "1"])).window_ms,
            1,
            "a requested 1 ms window must be honored literally, not clamped"
        );
        assert_eq!(
            parse_fps_opts(&args(&["--fps-window-ms", "5"])).window_ms,
            5
        );
        assert_eq!(
            parse_fps_opts(&args(&["--fps-window-ms", "100"])).window_ms,
            100
        );
    }

    #[test]
    fn no_fps_disables_and_a_later_fps_reenables() {
        // Catches: `--no-fps` being ignored (fps keeps running) or `--fps` not
        // overriding an earlier `--no-fps` (last-wins).
        assert!(!parse_fps_opts(&args(&["--no-fps"])).enabled);
        assert!(parse_fps_opts(&args(&["--no-fps", "--fps"])).enabled);
    }

    #[test]
    fn parses_the_window_and_rejects_bad_values() {
        // Catches: a zero/unparseable/valueless `--fps-window-ms` clobbering the
        // default (a zero window yields no fps at all); the fallback must keep
        // the 100 ms default, and a removed floor must not let 0 through.
        assert_eq!(
            parse_fps_opts(&args(&["--fps-window-ms", "750"])).window_ms,
            750
        );
        assert_eq!(
            parse_fps_opts(&args(&["--fps-window-ms", "0"])).window_ms,
            DEFAULT_FPS_WINDOW_MS
        );
        assert_eq!(
            parse_fps_opts(&args(&["--fps-window-ms", "soon"])).window_ms,
            DEFAULT_FPS_WINDOW_MS
        );
        assert_eq!(
            parse_fps_opts(&args(&["--fps-window-ms"])).window_ms,
            DEFAULT_FPS_WINDOW_MS
        );
    }

    #[test]
    fn parses_an_explicit_fps_target() {
        // Catches: `--fps-pid` being ignored (the explicit target never used) or
        // a valueless flag setting an empty target.
        assert_eq!(
            parse_fps_opts(&args(&["--fps-pid", "Overwatch"])).pid,
            Some("Overwatch".to_string())
        );
        assert_eq!(parse_fps_opts(&args(&["--fps-pid"])).pid, None);
        assert_eq!(parse_fps_opts(&args(&["--fps-pid", ""])).pid, None);
    }
}
