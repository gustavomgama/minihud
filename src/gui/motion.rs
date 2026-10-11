//! Overlay motion: the idle fade and change detection.
//!
//! Pure and unit-tested. The egui layer supplies the elapsed time and applies
//! the result. "Motion is a courtesy": the panel dims when nothing is
//! happening, and comes straight back when a value moves.

use super::model::Metric;

/// Seconds the panel takes to ramp from full to dim once it goes idle.
const RAMP_SECS: f32 = 0.8;
/// The dimmest the panel goes, as a fraction of the configured opacity.
const FLOOR: f32 = 0.15;

/// The panel opacity after being idle for `idle_for_secs`.
///
/// Full until `fade_idle_secs`, then a smooth ramp to [`FLOOR`] of `base`.
/// `fade_idle_secs == 0` disables the fade entirely (always full).
pub fn idle_opacity(base: f32, fade_idle_secs: u64, idle_for_secs: f32) -> f32 {
    if fade_idle_secs == 0 {
        return base;
    }
    let fade = fade_idle_secs as f32;
    if idle_for_secs <= fade {
        return base;
    }
    let t = ((idle_for_secs - fade) / RAMP_SECS).clamp(0.0, 1.0);
    base * (FLOOR + (1.0 - FLOOR) * (1.0 - t))
}

/// True while the fade is actually changing, so the overlay repaints fast only
/// then. A settled panel (fully faded or full) repaints slowly.
pub fn fade_animating(factor: f32, last_factor: f32) -> bool {
    (factor - last_factor).abs() > 0.001
}

/// A cheap signature of what is drawn, so the overlay can tell whether anything
/// changed since the last frame (and therefore is not idle).
pub fn signature(rows: &[Metric], fps: Option<f64>, low: Option<f64>) -> u64 {
    // FNV-1a over the rendered content.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for &b in bytes {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for m in rows {
        eat(m.label.as_bytes());
        eat(m.value.as_bytes());
        eat(&m
            .fraction
            .map(f32::to_bits)
            .unwrap_or(u32::MAX)
            .to_le_bytes());
    }
    eat(&fps.map(f64::to_bits).unwrap_or(u64::MAX).to_le_bytes());
    eat(&low.map(f64::to_bits).unwrap_or(u64::MAX).to_le_bytes());
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::model;
    use crate::hw::HwStats;

    #[test]
    fn zero_seconds_disables_the_fade() {
        // Catches: `fade_idle_secs = 0` ("never fade") still dimming the panel.
        assert_eq!(idle_opacity(0.8, 0, 0.0), 0.8);
        assert_eq!(idle_opacity(0.8, 0, 9999.0), 0.8);
    }

    #[test]
    fn stays_full_until_the_threshold_then_ramps_to_the_floor() {
        // Catches: fading before the configured delay, or never reaching the
        // dim floor (the fade does nothing).
        let base = 0.8;
        assert_eq!(idle_opacity(base, 5, 0.0), base);
        assert_eq!(idle_opacity(base, 5, 4.9), base, "still full before 5 s");
        let mid = idle_opacity(base, 5, 5.0 + RAMP_SECS / 2.0);
        assert!(mid < base && mid > base * FLOOR, "mid-ramp value: {mid}");
        assert_eq!(
            idle_opacity(base, 5, 5.0 + RAMP_SECS + 10.0),
            base * FLOOR,
            "settles at the floor"
        );
    }

    #[test]
    fn the_fade_never_rises() {
        // Catches: a non-monotonic curve (the panel flickering brighter as it
        // idles).
        let mut prev = f32::INFINITY;
        for i in 0..50 {
            let o = idle_opacity(1.0, 3, i as f32 * 0.1);
            assert!(o <= prev + 1e-6, "must not rise: {o} > {prev}");
            prev = o;
        }
    }

    #[test]
    fn identical_content_signs_the_same() {
        // Catches: a signature that varies per call (random/pointer-based), so
        // the overlay never looks idle.
        let rows = model::rows(&HwStats::default());
        assert_eq!(
            signature(&rows, Some(60.0), Some(45.0)),
            signature(&rows, Some(60.0), Some(45.0))
        );
    }

    #[test]
    fn any_change_alters_the_signature() {
        // Catches: a signature blind to fps, the 1% low, or a metric value, so
        // a real change would not wake the panel.
        let rows = model::rows(&HwStats::default());
        let base = signature(&rows, Some(60.0), Some(45.0));
        assert_ne!(base, signature(&rows, Some(61.0), Some(45.0)), "fps");
        assert_ne!(base, signature(&rows, Some(60.0), None), "1% low");
        let busy = HwStats {
            cpu_percent: 50.0,
            ..HwStats::default()
        };
        assert_ne!(
            base,
            signature(&model::rows(&busy), Some(60.0), Some(45.0)),
            "metric value"
        );
    }

    #[test]
    fn settled_fade_does_not_request_fast_repaint() {
        // Catches: a fully-faded panel repainting at 20 fps forever because the
        // check is `factor < 0.999` (a settled 0.15 factor never reaches 1.0).
        assert!(!fade_animating(0.15, 0.15), "settled faded must be slow");
        assert!(!fade_animating(1.0, 1.0), "settled full must be slow");
        assert!(fade_animating(0.5, 1.0), "changing fade must be fast");
    }
}
