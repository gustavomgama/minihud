//! Default / `--follow` ETW orchestration (tier 0, no injection).
//!
//! Watches the selected target (foreground, `--match` glob, or an explicit
//! `--fps-pid`), points a [`PresentMonFeed`] at it, and updates the capture
//! segment of the status line on a fast tick. It never injects: the target is
//! only observed over ETW.
//!
//! Cadences are decoupled: the hardware stats keep their own `--interval-ms`
//! thread ([`crate::stats_loop`]), the target is re-resolved every `--poll-ms`,
//! and the fps/status line redraws every `--fps-rate-ms`
//! ([`super::DEFAULT_FPS_TICK_MS`]).

use std::time::{Duration, Instant};

use super::presentmon::PresentMonFeed;
use super::window::FpsSource;
use super::FpsOpts;
use crate::hook::follow::{follow_deadline, poll_delay, select_target, skip_reason};
use crate::hook::proc;

/// What to do with the PresentMon feed this tick.
#[derive(Debug, PartialEq, Eq)]
pub enum FeedAction {
    /// Start (or retarget) the feed for a newly selected pid.
    Start,
    /// Stop the feed: the target is gone.
    Stop,
    /// Nothing to do.
    Keep,
}

/// Decide the feed's next action from the selected pid and the current one.
///
/// The same pid is a no-op (never restart PresentMon on every tick); a different
/// selected pid is a retarget; a dropped selection with one running is a stop.
pub fn feed_action(selected: Option<u32>, current: u32) -> FeedAction {
    match selected {
        Some(pid) if pid != current => FeedAction::Start,
        None if current != 0 => FeedAction::Stop,
        _ => FeedAction::Keep,
    }
}

/// Render the capture segment (the fps readout) for the status line.
///
/// Missing data renders as `--` (never `0.0 fps`): a source that has no fresh
/// samples is reported as waiting, not as a stalled zero.
pub fn capture_line(label: &str, source: &dyn FpsSource, window_ms: f64) -> String {
    match source.fps(window_ms) {
        Some(fps) => {
            let ms = if fps > 0.0 { 1000.0 / fps } else { 0.0 };
            match source.one_percent_low() {
                Some(low) => format!("{label}: {fps:.1} fps  {ms:.2} ms  1% low {low:.1}"),
                None => format!("{label}: {fps:.1} fps  {ms:.2} ms"),
            }
        }
        None => format!("{label}: -- (waiting for presents)"),
    }
}

/// Resolve the pid to observe, honoring an explicit target over the glob.
fn select(own: u32, match_glob: &Option<String>, explicit: Option<&str>) -> Option<(u32, String)> {
    if let Some(target) = explicit {
        let pid = proc::find_pid(target)?;
        let name = proc::process_name(pid).unwrap_or_else(|| target.to_string());
        return (skip_reason(pid, &name, own).is_none()).then_some((pid, name));
    }
    let foreground =
        proc::foreground_pid().and_then(|pid| proc::process_name(pid).map(|n| (pid, n)));
    let processes = proc::list_processes();
    select_target(match_glob.as_deref(), foreground, &processes)
        .filter(|(pid, exe)| skip_reason(*pid, exe, own).is_none())
}

/// Emit a lifecycle log line only when it differs from the previous one (the
/// target resolution repeats every tick).
fn log_once(last: &mut Option<String>, msg: String) {
    if last.as_deref() != Some(msg.as_str()) {
        tracing::info!("follow: {msg}");
        *last = Some(msg);
    }
}

/// Watch the target and show its ETW fps/frametime beside the hardware stats.
///
/// Stops on Ctrl+C or after `secs` seconds. `opts.enabled == false` (`--no-fps`)
/// keeps the loop alive (for the hardware thread) but shows no fps segment.
pub fn run(
    match_glob: Option<String>,
    poll_ms: u64,
    secs: Option<u64>,
    opts: FpsOpts,
) -> Result<(), String> {
    let poll = poll_delay(poll_ms);
    let own = std::process::id();
    crate::console::install_ctrl_handler();

    let target_desc = opts
        .pid
        .as_deref()
        .or(match_glob.as_deref())
        .unwrap_or("<foreground>");
    let bound = secs.map(|s| format!(", secs={s}")).unwrap_or_default();
    tracing::info!(
        "fps: ETW (tier 0, no injection){}",
        if opts.enabled { "" } else { " (disabled)" }
    );
    log_once(
        &mut None,
        format!(
            "started (match={target_desc}, poll={}ms, own pid={own}{bound})",
            poll.as_millis()
        ),
    );

    let deadline = follow_deadline(Instant::now(), secs);
    let window_ms = opts.window_ms as f64;
    let tick = Duration::from_millis(opts.tick_ms.max(1));
    let mut feed: Option<PresentMonFeed> = None;
    let mut current: u32 = 0;
    let mut label = String::new();
    let mut next_poll = Instant::now();
    let mut last_status: Option<String> = None;

    while crate::console::should_run() && deadline.is_none_or(|d| Instant::now() < d) {
        if Instant::now() >= next_poll {
            next_poll = Instant::now() + poll;
            let selected = select(own, &match_glob, opts.pid.as_deref());
            match feed_action(selected.as_ref().map(|(pid, _)| *pid), current) {
                FeedAction::Start => {
                    if let Some((pid, exe)) = selected {
                        log_once(&mut last_status, format!("target={exe} (pid {pid})"));
                        feed = opts.enabled.then(|| PresentMonFeed::start(pid));
                        current = pid;
                        label = format!("pid {pid}");
                    }
                }
                FeedAction::Stop => {
                    log_once(&mut last_status, "target gone".to_string());
                    feed = None;
                    current = 0;
                    label.clear();
                    crate::console::status_capture("");
                }
                FeedAction::Keep => {}
            }
        }
        if opts.enabled {
            if let Some(f) = &feed {
                crate::console::status_capture(capture_line(&label, f, window_ms));
            }
        }
        std::thread::sleep(tick);
    }

    drop(feed.take()); // kills the PresentMon child
    let reason = if crate::console::should_run() {
        "follow-secs elapsed"
    } else {
        "interrupt"
    };
    log_once(&mut last_status, format!("stopping ({reason})"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake source with no data — proves the graceful `--` path with no
    /// process involved (FpsOverlayer's lesson: a blocked ETW session reads
    /// `--`, never a crash or a bogus zero).
    struct NoFps;
    impl FpsSource for NoFps {
        fn fps(&self, _window_ms: f64) -> Option<f64> {
            None
        }
        fn one_percent_low(&self) -> Option<f64> {
            None
        }
    }

    /// A fake source with fixed figures (and a 1% low, or none).
    struct FixedFps {
        fps: f64,
        low: Option<f64>,
    }
    impl FpsSource for FixedFps {
        fn fps(&self, _window_ms: f64) -> Option<f64> {
            Some(self.fps)
        }
        fn one_percent_low(&self) -> Option<f64> {
            self.low
        }
    }

    #[test]
    fn capture_line_shows_fps_frametime_and_one_percent_low() {
        // Catches: a status line that omits the frametime or mislabels the 1%
        // low, so the operator cannot read the figures they asked for. 120 fps
        // is exactly 8.33 ms.
        let src = FixedFps {
            fps: 120.0,
            low: Some(45.5),
        };
        let line = capture_line("pid 42", &src, 500.0);
        assert!(line.contains("120.0 fps"), "{line}");
        assert!(line.contains("8.33 ms"), "{line}");
        assert!(line.contains("1% low 45.5"), "{line}");
        assert!(line.contains("pid 42"), "{line}");
    }

    #[test]
    fn capture_line_omits_the_low_when_absent_but_keeps_fps() {
        // Catches: requiring a 1% low to show fps at all (a short-lived target
        // with too few samples would show nothing).
        let line = capture_line(
            "pid 7",
            &FixedFps {
                fps: 60.0,
                low: None,
            },
            500.0,
        );
        assert!(line.contains("60.0 fps"), "{line}");
        assert!(!line.contains("1% low"), "{line}");
    }

    #[test]
    fn capture_line_shows_dashes_for_a_source_with_no_samples() {
        // Catches: a fake/blocked source printing `0.0 fps` (indistinguishable
        // from a real stall) instead of the honest `--`.
        let line = capture_line("pid 9", &NoFps, 500.0);
        assert!(line.contains("--"), "{line}");
        assert!(!line.contains("0.0 fps"), "{line}");
    }

    #[test]
    fn capture_line_reflects_the_newest_sample_on_every_call() {
        // Catches: caching the rendered line (or the fps figure) so a redraw
        // shows the previous sample — the number would lag by one tick.
        struct LiveFps(std::sync::Mutex<f64>);
        impl FpsSource for LiveFps {
            fn fps(&self, _window_ms: f64) -> Option<f64> {
                Some(*self.0.lock().expect("lock"))
            }
            fn one_percent_low(&self) -> Option<f64> {
                None
            }
        }
        let src = LiveFps(std::sync::Mutex::new(60.0));
        assert!(capture_line("pid 1", &src, 250.0).contains("60.0 fps"));
        *src.0.lock().expect("lock") = 144.0;
        assert!(
            capture_line("pid 1", &src, 250.0).contains("144.0 fps"),
            "the renderer must read the live source, not cache the line"
        );
    }

    #[test]
    fn feed_action_starts_stops_and_keeps() {
        // Catches: restarting PresentMon every tick for an unchanged pid, never
        // retargeting when the pid changes, or leaving a feed running after the
        // target is gone.
        assert_eq!(feed_action(Some(5), 0), FeedAction::Start);
        assert_eq!(feed_action(Some(5), 5), FeedAction::Keep);
        assert_eq!(feed_action(None, 0), FeedAction::Keep);
        assert_eq!(feed_action(None, 5), FeedAction::Stop);
        assert_eq!(feed_action(Some(9), 5), FeedAction::Start);
    }
}
