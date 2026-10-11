//! Rolling frametime window and the fps / 1%-low math.
//!
//! Pure and unit-tested: the window holds the most recent `cap` frametimes
//! (milliseconds between presents) and derives an average FPS over a short
//! trailing window plus a 1% low over the whole buffer. The math is ported from
//! EasyFPS's `fps_capture.rs` (MIT) and adapted to this crate's API (missing ⇒
//! `None`, never `0.0`/a guess).
//!
//! [`FpsSource`] lets the status-line renderer depend on an abstraction, so a
//! fake source (no real process) can prove the "no samples ⇒ `--`" path.

use std::collections::VecDeque;

/// Most recent frametimes kept in memory. A long history makes the 1% low
/// meaningful (slow frames are rare); the average FPS uses only the trailing
/// `window_ms` (see [`FpsWindow::fps`]).
pub const DEFAULT_CAP: usize = 2000;

/// A bounded deque of frametime samples (ms between presents).
#[derive(Debug, Clone)]
pub struct FpsWindow {
    samples: VecDeque<f64>,
    cap: usize,
}

impl FpsWindow {
    /// A window holding at most `cap` samples (`cap` is clamped to ≥ 1).
    pub fn new(cap: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(cap.max(1)),
            cap: cap.max(1),
        }
    }

    /// Record one frametime (ms). Non-positive or non-finite samples are
    /// ignored (a stalled/absurd present must not poison the average).
    pub fn record(&mut self, ms: f64) {
        if !ms.is_finite() || ms <= 0.0 {
            return;
        }
        if self.samples.len() == self.cap {
            self.samples.pop_front();
        }
        self.samples.push_back(ms);
    }

    /// Average FPS over the newest `window_ms` of samples: walk the samples
    /// newest-first, accumulating frametimes until they cover the window, then
    /// report `frames * 1000 / covered`. `None` when there are no samples or
    /// `window_ms` is non-positive. Invalid samples are dropped on [`record`],
    /// so a returned value is always finite and positive.
    ///
    /// [`record`]: FpsWindow::record
    pub fn fps(&self, window_ms: f64) -> Option<f64> {
        if window_ms <= 0.0 {
            return None;
        }
        let mut covered = 0.0;
        let mut frames = 0usize;
        for &ms in self.samples.iter().rev() {
            covered += ms;
            frames += 1;
            if covered >= window_ms {
                break;
            }
        }
        (covered > 0.0).then_some(frames as f64 * 1000.0 / covered)
    }

    /// The 1% low FPS: the frametime at the 99th percentile (slowest 1% of
    /// frames), inverted. `None` when there are no samples.
    pub fn one_percent_low(&self) -> Option<f64> {
        let n = self.samples.len();
        if n == 0 {
            return None;
        }
        let mut v: Vec<f64> = self.samples.iter().copied().collect();
        let idx = ((n as f64 * 0.01).ceil() as usize).min(n - 1);
        // Descending: index 0 is the slowest frame, so `idx` lands within the
        // slowest 1%. O(n) selection, no full sort.
        v.select_nth_unstable_by(idx, |a, b| {
            b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal)
        });
        let low_ms = v[idx];
        (low_ms > 0.0).then_some(1000.0 / low_ms)
    }
}

impl Default for FpsWindow {
    fn default() -> Self {
        Self::new(DEFAULT_CAP)
    }
}

/// A source of average fps / 1%-low figures. The status-line renderer depends
/// on this, not on the concrete [`super::presentmon::PresentMonFeed`], so the
/// "no data ⇒ `--`" path is testable with a fake.
pub trait FpsSource: Send + Sync {
    /// Average FPS over `window_ms`, or `None` when no fresh samples exist.
    fn fps(&self, window_ms: f64) -> Option<f64>;
    /// 1% low FPS, or `None` when no fresh samples exist.
    fn one_percent_low(&self) -> Option<f64>;
}

impl FpsSource for FpsWindow {
    fn fps(&self, window_ms: f64) -> Option<f64> {
        FpsWindow::fps(self, window_ms)
    }

    fn one_percent_low(&self) -> Option<f64> {
        FpsWindow::one_percent_low(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The production 500 ms trailing window (the `--fps-poll` default).
    const WINDOW_MS: f64 = 500.0;

    #[test]
    fn an_empty_window_has_no_fps() {
        // Catches: reporting 0 fps instead of None on no samples, which would
        // print "0.0 fps" at startup instead of waiting for presents.
        let w = FpsWindow::default();
        assert_eq!(w.fps(WINDOW_MS), None);
        assert_eq!(w.one_percent_low(), None);
    }

    #[test]
    fn steady_60fps_reads_as_60() {
        // Catches: window math that off-by-frames the accumulation (a constant
        // frametime must read back as its own frame rate).
        let mut w = FpsWindow::default();
        for _ in 0..500 {
            w.record(1000.0 / 60.0);
        }
        let fps = w.fps(WINDOW_MS).expect("fps");
        assert!((fps - 60.0).abs() < 0.01, "fps was {fps}");
        let low = w.one_percent_low().expect("low");
        assert!((low - 60.0).abs() < 0.01, "1% low was {low}");
    }

    #[test]
    fn fps_tracks_the_recent_window_not_the_whole_buffer() {
        // Catches: averaging the whole buffer, which would smooth the count over
        // tens of seconds (a long 20 fps history would drag a fast burst down)
        // instead of reporting the current window.
        let mut w = FpsWindow::default();
        for _ in 0..1000 {
            w.record(50.0); // long history at 20 fps
        }
        for _ in 0..200 {
            w.record(1000.0 / 120.0); // recent burst at 120 fps
        }
        let fps = w.fps(WINDOW_MS).expect("fps");
        assert!(
            (fps - 120.0).abs() < 1.0,
            "fps must track the recent ~120, was {fps}"
        );
    }

    #[test]
    fn fps_is_none_for_a_nonpositive_window() {
        // Catches: a zero/negative window silently returning a figure (a divide
        // by zero would be NaN, or the whole buffer would be averaged) instead
        // of the honest None.
        let mut w = FpsWindow::default();
        assert_eq!(w.fps(0.0), None);
        assert_eq!(w.fps(-1.0), None);
        w.record(16.6667);
        assert_eq!(w.fps(0.0), None);
        assert_eq!(w.fps(-1.0), None);
    }

    #[test]
    fn one_percent_low_reflects_the_slowest_frames() {
        // Catches: a 1% low computed from the wrong end of the distribution
        // (e.g. the fastest 1%), which would hide stutter instead of exposing it.
        let mut w = FpsWindow::default();
        for _ in 0..50 {
            w.record(50.0); // 20 fps
        }
        for _ in 0..950 {
            w.record(10.0); // 100 fps
        }
        let fps = w.fps(WINDOW_MS).expect("fps");
        let low = w.one_percent_low().expect("low");
        assert!(fps > low, "fps {fps} should exceed 1% low {low}");
        assert!((low - 20.0).abs() < 0.5, "1% low was {low}");
    }

    #[test]
    fn record_ignores_nonpositive_and_nonfinite_samples() {
        // Catches: a zero/NaN frametime (a malformed CSV field) skewing the
        // window toward an absurd fps instead of being dropped.
        let mut w = FpsWindow::default();
        w.record(0.0);
        w.record(-5.0);
        w.record(f64::NAN);
        w.record(f64::INFINITY);
        assert!(
            w.fps(1000.0).is_none(),
            "only invalid samples were recorded, so there must be no fps"
        );
        w.record(16.6667);
        let fps = w.fps(1000.0).expect("fps");
        assert!(
            (fps - 60.0).abs() < 0.01,
            "a valid sample must be recorded: {fps}"
        );
    }

    #[test]
    fn the_cap_evicts_the_oldest_samples() {
        // Catches: an unbounded deque growing without limit during a long
        // session. With a cap of 3 only 30..50 ms survive: 3 frames over 120 ms
        // = 25 fps; leaked history (10..50 ms) would instead read 33.3 fps.
        let mut w = FpsWindow::new(3);
        for i in 1..=5 {
            w.record(i as f64 * 10.0);
        }
        let fps = w.fps(1000.0).expect("fps");
        assert!(
            (fps - 25.0).abs() < 0.001,
            "only 3 samples may remain: {fps}"
        );
    }
}
