//! Trailing fps / frametime math over a chronological slice of frame records.
//!
//! Pure: given present timestamps (`QueryPerformanceCounter` ticks), a QPC
//! frequency, and a window, it returns the frames within the window and the
//! present-to-present frametime over that window. Kept free of I/O so the
//! host's printed numbers are unit-tested independently of the mapping.

use crate::layout::FrameRecord;

/// A trailing-window measurement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Trailing {
    /// Frames whose present fell inside the window.
    pub frames: usize,
    /// Frames per second over the window.
    pub fps: f64,
    /// Mean present-to-present frametime (ms).
    pub frametime_ms: f64,
    /// Wall-clock span covered by the window (ms).
    pub span_ms: f64,
}

/// Compute trailing metrics over `records` (oldest first) using the newest
/// present as "now" and `window_ms` as the look-back.
///
/// Returns `None` when there are fewer than two records, the frequency is
/// zero, or the selected records span no time (all identical timestamps).
pub fn trailing(records: &[FrameRecord], qpc_freq: u64, window_ms: f64) -> Option<Trailing> {
    if qpc_freq == 0 || window_ms <= 0.0 || records.len() < 2 {
        return None;
    }
    let newest = records.last()?.qpc_start;
    let window_ticks = (window_ms * qpc_freq as f64 / 1000.0).round() as u64;
    let cutoff = newest.saturating_sub(window_ticks);
    let first = records.iter().find(|r| r.qpc_start >= cutoff)?.qpc_start;
    if newest <= first {
        return None;
    }
    let frames = records.iter().filter(|r| r.qpc_start >= cutoff).count();
    if frames < 2 {
        return None;
    }
    let span_ticks = (newest - first) as f64;
    let frametime_ms = span_ticks / (frames - 1) as f64 / qpc_freq as f64 * 1000.0;
    if frametime_ms <= 0.0 {
        return None;
    }
    Some(Trailing {
        frames,
        fps: 1000.0 / frametime_ms,
        frametime_ms,
        span_ms: span_ticks / qpc_freq as f64 * 1000.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Api;

    fn rec(qpc_start: u64) -> FrameRecord {
        FrameRecord {
            qpc_start,
            api: Api::DxgiPresent,
            ..FrameRecord::default()
        }
    }

    /// 1 GHz QPC: one tick is one nanosecond.
    const FREQ: u64 = 1_000_000_000;

    #[test]
    fn needs_at_least_two_frames() {
        assert_eq!(trailing(&[], FREQ, 500.0), None);
        assert_eq!(trailing(&[rec(0)], FREQ, 500.0), None);
    }

    #[test]
    fn rejects_zero_frequency() {
        let rs = [rec(0), rec(16_000_000)];
        assert_eq!(trailing(&rs, 0, 500.0), None);
    }

    #[test]
    fn computes_60fps_over_the_full_window() {
        // Three presents, 16.667 ms apart.
        let rs = [rec(0), rec(16_666_667), rec(33_333_334)];
        let t = trailing(&rs, FREQ, 1000.0).expect("two+ frames");
        assert_eq!(t.frames, 3);
        assert!((t.frametime_ms - 16.666_667).abs() < 1e-3, "{t:?}");
        assert!((t.fps - 60.0).abs() < 0.01, "{t:?}");
        assert!((t.span_ms - 33.333_334).abs() < 1e-3, "{t:?}");
    }

    #[test]
    fn window_excludes_frames_older_than_the_lookback() {
        let rs = [rec(0), rec(16_666_667), rec(33_333_334)];
        // 20 ms look-back from the newest: the first frame (33.3 ms old) drops.
        let t = trailing(&rs, FREQ, 20.0).expect("two+ frames");
        assert_eq!(t.frames, 2);
        assert!((t.frametime_ms - 16.666_667).abs() < 1e-3, "{t:?}");
    }

    #[test]
    fn zero_span_is_none() {
        let rs = [rec(5), rec(5), rec(5)];
        assert_eq!(trailing(&rs, FREQ, 500.0), None);
    }

    #[test]
    fn negative_zero_and_nan_windows_are_rejected_without_panicking() {
        // Catches: a non-positive/NaN `window_ms` slipping past the guard and
        // producing garbage (a NaN cutoff, a divide-by-zero frametime, or a
        // panicking cast) instead of `None`.
        let rs = [rec(0), rec(16_000_000)];
        assert_eq!(trailing(&rs, FREQ, -1.0), None);
        assert_eq!(trailing(&rs, FREQ, 0.0), None);
        assert_eq!(
            trailing(&rs, FREQ, f64::NAN),
            None,
            "NaN window must be None"
        );
    }

    #[test]
    fn a_duplicate_or_identical_timestamp_delta_is_none() {
        // Catches: a divide-by-zero / negative frametime when samples share a
        // timestamp (a stalled or coalesced present, or a corrupt record).
        assert_eq!(trailing(&[rec(7), rec(7)], FREQ, 500.0), None);
        assert_eq!(
            trailing(&[rec(9), rec(9), rec(9), rec(9)], FREQ, 500.0),
            None
        );
    }

    #[test]
    fn a_wrapped_or_decreasing_timestamp_sequence_is_none_not_a_panic() {
        // Catches: an unsigned subtraction underflow (a panic in debug, a huge
        // bogus span in release) when the newest timestamp is smaller than an
        // older one — a QPC wrap, or a foreign/corrupt block. The math must fail
        // open.
        let backwards = [rec(u64::MAX - 10), rec(5)];
        assert_eq!(trailing(&backwards, FREQ, 500.0), None, "backwards span");

        // A lone low value after a huge one, and vice versa, must never panic.
        for rs in [
            &[rec(5), rec(u64::MAX - 10)][..],
            &[rec(u64::MAX - 10), rec(9), rec(3)][..],
            &[rec(0), rec(u64::MAX), rec(u64::MAX - 1)][..],
        ] {
            if let Some(t) = trailing(rs, FREQ, 500.0) {
                assert!(
                    t.frametime_ms.is_finite() && t.frametime_ms > 0.0,
                    "{rs:?} -> {t:?}"
                );
                assert!(t.fps.is_finite() && t.fps > 0.0, "{rs:?} -> {t:?}");
                assert!(t.span_ms.is_finite(), "{rs:?} -> {t:?}");
            }
        }
    }

    #[test]
    fn a_huge_window_or_frequency_stays_finite() {
        // Catches: a `window_ms`/`qpc_freq` large enough to overflow the tick
        // math into `inf`/`NaN` (the host would print `inf fps`). Every returned
        // value must stay finite.
        let rs = [rec(0), rec(16_666_667), rec(33_333_334)];
        for window in [f64::INFINITY, f64::MAX, 1e300] {
            if let Some(t) = trailing(&rs, FREQ, window) {
                assert!(t.frametime_ms.is_finite(), "window={window}");
                assert!(t.fps.is_finite() && !t.fps.is_nan(), "window={window}");
                assert!(t.span_ms.is_finite(), "window={window}");
            }
        }
        for freq in [1u64, u64::MAX] {
            if let Some(t) = trailing(&rs, freq, 1_000.0) {
                assert!(
                    t.frametime_ms.is_finite() && t.frametime_ms > 0.0,
                    "freq={freq}"
                );
                assert!(t.fps.is_finite(), "freq={freq}");
            }
        }
    }

    #[test]
    fn every_returned_metric_is_finite_and_positive() {
        // The invariant the host's printed numbers rely on: whenever `trailing`
        // returns `Some`, the frametime, fps and span are finite and positive.
        for n in 2..40u64 {
            let rs: Vec<FrameRecord> = (0..n).map(|i| rec(i * 7_000_000)).collect();
            if let Some(t) = trailing(&rs, FREQ, 100.0) {
                assert!(t.frames >= 2, "n={n} {t:?}");
                assert!(
                    t.frametime_ms > 0.0 && t.frametime_ms.is_finite(),
                    "n={n} {t:?}"
                );
                assert!(t.fps > 0.0 && t.fps.is_finite(), "n={n} {t:?}");
                assert!(t.span_ms.is_finite(), "n={n} {t:?}");
            }
        }
    }

    #[test]
    fn a_single_timestamp_sample_is_none() {
        assert_eq!(trailing(&[rec(123)], FREQ, 500.0), None);
        assert_eq!(trailing(&[], FREQ, 500.0), None);
    }

    #[test]
    fn trailing_never_panics_or_returns_nonfinite_on_hostile_input() {
        // Catches: any panic (subtraction overflow, divide-by-zero, a bad cast)
        // or a `NaN`/`inf` result from hostile timestamps / window / frequency —
        // exactly what a foreign or corrupt shared block can feed the host.
        // Deterministic xorshift so a failure is reproducible.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..50_000 {
            let n = (next() % 6) as usize;
            let rs: Vec<FrameRecord> = (0..n).map(|_| rec(next())).collect();
            let freq = if next() % 4 == 0 { 0 } else { next() };
            let window = match next() % 6 {
                0 => f64::NAN,
                1 => f64::INFINITY,
                2 => f64::NEG_INFINITY,
                3 => -1.0,
                4 => f64::from_bits(next()),
                _ => (next() % 100_000) as f64,
            };
            if let Some(t) = trailing(&rs, freq, window) {
                assert!(t.frametime_ms.is_finite() && t.frametime_ms > 0.0, "{t:?}");
                assert!(t.fps.is_finite() && t.fps > 0.0, "{t:?}");
                assert!(t.span_ms.is_finite() && t.span_ms >= 0.0, "{t:?}");
                assert!(t.frames >= 2, "{t:?}");
            }
        }
    }
}
