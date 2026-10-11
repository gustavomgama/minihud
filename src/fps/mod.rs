//! Tier-0 FPS capture (out-of-process, no injection).
//!
//! The default FPS source is ETW present events, collected by a spawned
//! `PresentMon.exe` and parsed from its stdout — the same technique as
//! PresentMon/EasyFPS/FpsOverlayer. Nothing is injected into the target, so it
//! is anti-cheat-safe by construction.
//!
//! - [`window`] — the pure frametime window and 1%-low math.
//! - [`presentmon`] — the `PresentMon.exe` sidecar feed (spawn, lenient CSV
//!   parse, graceful degradation).
//! - [`follow`] — the default/`--follow` orchestrator (target → PresentMon →
//!   status line), with no injection.

pub mod follow;
pub mod presentmon;
pub mod window;

/// Fixed fps averaging window (ms): the readout always averages the newest
/// `FPS_WINDOW_MS` of frames, independent of the refresh cadence. A fast
/// `--fps-poll` refreshes the number more often without shrinking this window,
/// so a fast update never makes the figure noisy.
pub const FPS_WINDOW_MS: u64 = 500;

/// Default fps poll cadence (ms): the **update cadence only** — how often the
/// readout refreshes and the target is re-resolved. The averaging window is the
/// fixed [`FPS_WINDOW_MS`]. Override with `--fps-poll <n>`.
pub const DEFAULT_FPS_POLL_MS: u64 = 500;

/// FPS-display options from the CLI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FpsOpts {
    /// Show the ETW fps readout (default on). `--no-fps` disables it.
    pub enabled: bool,
    /// Fps poll cadence in ms (`--fps-poll`): the update cadence only — how
    /// often the readout refreshes and the target is re-resolved. The averaging
    /// window is the fixed [`FPS_WINDOW_MS`], not this value.
    pub poll_ms: u64,
    /// Explicit fps target (`--fps-pid <pid|name>`); default: the followed
    /// (foreground/`--match`) target.
    pub pid: Option<String>,
}

impl Default for FpsOpts {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_ms: DEFAULT_FPS_POLL_MS,
            pid: None,
        }
    }
}

/// Parse the fps flags: `--fps`/`--no-fps`, `--fps-poll <n>`,
/// `--fps-pid <pid|name>`. A valueless/non-positive/unparseable numeric value
/// keeps the default; the last occurrence of a flag wins.
pub fn parse_fps_opts(args: &[String]) -> FpsOpts {
    let mut opts = FpsOpts::default();
    for (i, a) in args.iter().enumerate() {
        match a.as_str() {
            "--no-fps" => opts.enabled = false,
            "--fps" => opts.enabled = true,
            "--fps-poll" => {
                if let Some(n) = args
                    .get(i + 1)
                    .and_then(|s| s.parse::<u64>().ok())
                    .filter(|&n| n > 0)
                {
                    opts.poll_ms = n;
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
    fn fps_defaults_to_enabled_on_a_500ms_poll_with_no_explicit_target() {
        // Catches: fps off by default, a zero poll cadence (no refresh), or a
        // stray default target.
        let d = parse_fps_opts(&args(&[]));
        assert_eq!(d, FpsOpts::default());
        assert!(d.enabled);
        assert_eq!(d.poll_ms, 500, "the default fps poll must be 500 ms");
        assert_eq!(d.poll_ms, DEFAULT_FPS_POLL_MS);
        assert_eq!(d.pid, None);
    }

    #[test]
    fn the_default_fps_poll_is_500ms() {
        // Catches: DEFAULT_FPS_POLL_MS drifting off the intended 500 ms default.
        assert_eq!(
            DEFAULT_FPS_POLL_MS, 500,
            "the raw default constant must be 500 ms"
        );
        assert_eq!(FpsOpts::default().poll_ms, DEFAULT_FPS_POLL_MS);
    }

    #[test]
    fn the_fps_window_is_a_fixed_500ms_independent_of_the_poll() {
        // Catches: the averaging window following `--fps-poll` — a fast poll
        // would then shrink the window and make the number noisy.
        assert_eq!(FPS_WINDOW_MS, 500, "the fixed fps window must be 500 ms");
    }

    #[test]
    fn a_custom_fps_poll_sets_only_the_cadence_not_the_window() {
        // Catches: re-deriving the average window from the poll (so
        // `--fps-poll 50` would average only 50 ms). The poll still parses to
        // its flag value; the window constant stays fixed at 500 ms.
        let fast = parse_fps_opts(&args(&["--fps-poll", "50"]));
        assert_eq!(fast.poll_ms, 50, "the poll cadence must honor the flag");
        assert_eq!(FPS_WINDOW_MS, 500, "the window must not follow the poll");
    }

    #[test]
    fn parses_the_fps_poll_and_rejects_bad_values() {
        // Catches: `--fps-poll` ignored, or a zero/unparseable/valueless value
        // clobbering the 500 ms default (0 would busy-spin the loop).
        assert_eq!(parse_fps_opts(&args(&["--fps-poll", "250"])).poll_ms, 250);
        for bad in [
            &["--fps-poll", "0"][..],
            &["--fps-poll", "soon"][..],
            &["--fps-poll"][..],
        ] {
            assert_eq!(
                parse_fps_opts(&args(bad)).poll_ms,
                DEFAULT_FPS_POLL_MS,
                "{bad:?} must keep the default poll"
            );
        }
    }

    #[test]
    fn no_fps_disables_and_a_later_fps_reenables() {
        // Catches: `--no-fps` being ignored (fps keeps running) or `--fps` not
        // overriding an earlier `--no-fps` (last-wins).
        assert!(!parse_fps_opts(&args(&["--no-fps"])).enabled);
        assert!(parse_fps_opts(&args(&["--no-fps", "--fps"])).enabled);
    }

    #[test]
    fn removed_fps_cadence_flags_are_not_recognized() {
        // Catches: a removed flag still wired (a silent no-op that misleads the
        // user) — the poll must stay at the 500 ms default.
        let d = parse_fps_opts(&args(&[]));
        for flag in [
            "--fps-instant",
            "--fps-window",
            "--fps-window-ms",
            "--fps-rate-ms",
        ] {
            assert_eq!(
                parse_fps_opts(&args(&[flag, "50"])),
                d,
                "{flag} must be inert (removed)"
            );
        }
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
