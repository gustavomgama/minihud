//! Default / `--follow` ETW orchestration (tier 0, no injection).
//!
//! Watches the selected target (foreground, `--match` glob, or an explicit
//! `--fps-pid`), points a [`PresentMonFeed`] at it, and updates the capture
//! segment of the status line **when a new frame arrives**. It never injects:
//! the target is only observed over ETW.
//!
//! One cadence drives the loop: `--fps-poll <n>` ([`FpsOpts::poll_ms`]) is the
//! target re-resolution interval and the loop wake. The fps readout always
//! averages the fixed [`FPS_WINDOW_MS`] window (see [`render_line`]), and it
//! redraws only when the feed's sample sequence changed ([`should_redraw`]).

use std::time::Instant;

use super::presentmon::PresentMonFeed;
use super::window::FpsSource;
use super::{FpsOpts, FPS_WINDOW_MS};
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
/// Reports the average FPS over `window_ms` (`ms` is its inverse frametime),
/// keeping the 1% low when the source has one. Missing data renders as `--`
/// (never `0.0 fps`): a source with no fresh samples is reported as waiting, not
/// as a stalled zero.
pub fn capture_line(label: &str, source: &dyn FpsSource, window_ms: f64) -> String {
    let Some(fps) = source.fps(window_ms) else {
        return format!("{label}: -- (waiting for presents)");
    };
    let ms = if fps > 0.0 { 1000.0 / fps } else { 0.0 };
    match source.one_percent_low() {
        Some(low) => format!("{label}: {fps:.1} fps  {ms:.2} ms  1% low {low:.1}"),
        None => format!("{label}: {fps:.1} fps  {ms:.2} ms"),
    }
}

/// Render the capture segment with the **fixed** [`FPS_WINDOW_MS`] window.
///
/// The window never follows `--fps-poll`: the poll only sets how often [`run`]
/// redraws (and re-resolves the target), so a fast poll refreshes a stable
/// 500 ms average instead of a noisy one.
pub fn render_line(label: &str, source: &dyn FpsSource) -> String {
    capture_line(label, source, FPS_WINDOW_MS as f64)
}

/// Whether the fps capture line must be redrawn this tick: only when the sample
/// sequence moved (a new frame was recorded), or on the first draw / the
/// transition into or out of the `--` state. `None` means nothing has been drawn
/// for the current feed yet.
///
/// This makes the readout frame-driven: an unchanged sequence is skipped here,
/// so the loop never composes or writes the same line on every wake.
pub fn should_redraw(last_seq: Option<u64>, cur_seq: u64) -> bool {
    last_seq != Some(cur_seq)
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
pub fn run(match_glob: Option<String>, secs: Option<u64>, opts: FpsOpts) -> Result<(), String> {
    let poll = poll_delay(opts.poll_ms);
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
    let mut feed: Option<PresentMonFeed> = None;
    let mut current: u32 = 0;
    let mut label = String::new();
    let mut next_poll = Instant::now();
    let mut last_status: Option<String> = None;
    // The last sample sequence drawn (None until the current feed first draws),
    // so the line redraws per frame instead of on every wake.
    let mut drawn_seq: Option<u64> = None;

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
                        drawn_seq = None; // a new feed must draw its first state
                    }
                }
                FeedAction::Stop => {
                    log_once(&mut last_status, "target gone".to_string());
                    feed = None;
                    current = 0;
                    label.clear();
                    drawn_seq = None;
                    crate::console::status_capture("");
                }
                FeedAction::Keep => {}
            }
        }
        if opts.enabled {
            if let Some(f) = &feed {
                let seq = f.sample_seq();
                if should_redraw(drawn_seq, seq) {
                    crate::console::status_capture(render_line(&label, f));
                    drawn_seq = Some(seq);
                }
            }
        }
        std::thread::sleep(poll);
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
    use crate::fps::window::FpsWindow;

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

    /// A fake source with fixed figures (an fps and an optional 1% low).
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

    /// A source that records the `window_ms` the renderer asked for, so a test
    /// can prove the fixed window is passed through unchanged.
    struct RecordingSource(std::sync::Mutex<Option<f64>>);
    impl FpsSource for RecordingSource {
        fn fps(&self, window_ms: f64) -> Option<f64> {
            *self.0.lock().expect("lock") = Some(window_ms);
            Some(60.0)
        }
        fn one_percent_low(&self) -> Option<f64> {
            None
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
    fn capture_line_shows_dashes_with_no_samples() {
        // Catches: printing `0.0 fps` (indistinguishable from a real stall)
        // instead of the honest `--` when the source has no frametime.
        let line = capture_line("pid 9", &NoFps, 500.0);
        assert!(line.contains("--"), "{line}");
        assert!(!line.contains("0.0 fps"), "{line}");
    }

    #[test]
    fn capture_line_averages_the_window_not_the_latest_frame() {
        // Catches: rendering one frame's frametime (the jitter source). Over a
        // 500 ms window a 120 fps burst plus a single slow 20 fps frame must
        // read the window average (~110 fps), never the newest 20 fps frame.
        let mut w = FpsWindow::default();
        for _ in 0..200 {
            w.record(1000.0 / 120.0); // burst at 120 fps
        }
        w.record(50.0); // newest frame at 20 fps
        let line = capture_line("pid 3", &w, 500.0);
        assert!(
            line.contains("110."),
            "the readout must show the window average (~110 fps): {line}"
        );
        assert!(
            !line.contains("20.0 fps"),
            "the readout must not show the single latest frame: {line}"
        );
        assert!(line.contains("1% low"), "the 1% low must be kept: {line}");
    }

    #[test]
    fn render_line_averages_the_fixed_500ms_window_not_a_poll_derived_one() {
        // Catches: the rendered window silently following `--fps-poll` instead
        // of the fixed 500 ms. A long 20 fps history plus a ~250 ms 120 fps
        // burst reads ~70 fps over 500 ms: not the latest frame (20 fps), not a
        // wider window (the history would drag it below 70), and not a narrower
        // one (which would read 120 fps).
        let mut w = FpsWindow::default();
        for _ in 0..1000 {
            w.record(50.0); // 20 fps history
        }
        for _ in 0..30 {
            w.record(1000.0 / 120.0); // ~250 ms burst at 120 fps
        }
        let line = render_line("pid 5", &w);
        assert!(
            line.contains("70."),
            "must read the 500 ms average (~70 fps): {line}"
        );
        assert!(
            !line.contains("20.0 fps"),
            "must not read the single latest frame: {line}"
        );
        assert!(
            !line.contains("120.0 fps"),
            "must not read a narrower window: {line}"
        );
    }

    #[test]
    fn render_line_asks_the_source_for_exactly_the_500ms_window() {
        // Catches: passing any other window to the source (e.g. the poll value):
        // the renderer must request the fixed FPS_WINDOW_MS, not `--fps-poll`.
        let src = RecordingSource(std::sync::Mutex::new(None));
        let _ = render_line("pid 2", &src);
        let seen = *src.0.lock().expect("lock");
        assert_eq!(
            seen,
            Some(FPS_WINDOW_MS as f64),
            "render_line must pass the fixed window"
        );
    }

    #[test]
    fn render_line_shows_dashes_with_no_samples() {
        // Catches: the fixed-window renderer dropping the graceful `--` path
        // (printing 0.0 fps, or panicking, when the source has no presents).
        let line = render_line("pid 9", &NoFps);
        assert!(line.contains("-- (waiting for presents)"), "{line}");
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
    fn should_redraw_only_when_the_sample_sequence_moves() {
        // Catches: redrawing every tick (the timer-gated bug this fixes) — the
        // redraw must fire on a changed seq and stay silent on an unchanged one.
        assert!(
            !should_redraw(Some(4), 4),
            "an unchanged seq must not redraw"
        );
        assert!(should_redraw(Some(4), 5), "a new frame must redraw");
    }

    #[test]
    fn should_redraw_on_the_first_draw_and_the_dash_transition() {
        // Catches: never drawing the first line (a None marker must redraw) or
        // not redrawing when samples go stale (seq -> 0 must show `--`).
        assert!(should_redraw(None, 0), "the initial `--` must be drawn");
        assert!(should_redraw(None, 1), "the first draw must happen");
        assert!(should_redraw(Some(4), 0), "going stale must redraw to `--`");
        assert!(
            !should_redraw(Some(0), 0),
            "`--` staying `--` must not redraw"
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
