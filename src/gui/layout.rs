//! Pure overlay geometry: how big the panel is and where it sits.
//!
//! No GUI dependency, so it is unit-tested in the default suite. The egui
//! layer feeds it the monitor size at runtime.

use super::config::{Anchor, Density};

/// Panel width at scale 1 (logical points).
const WIDTH_PT: f32 = 260.0;
/// Hero row plus the padding above the metric rows, at scale 1.
const BASE_PT: f32 = 76.0;
/// Points per metric row (label + bar + spacing) at scale 1.
const ROW_PT: f32 = 22.0;

/// How many metric rows a density draws.
fn row_count(density: Density) -> f32 {
    match density {
        Density::Zen => 1.0,
        Density::Standard => 3.0,
        Density::Full => 7.0,
    }
}

/// The panel size in logical points for a density at `scale`.
pub fn content_size(density: Density, scale: f32) -> [f32; 2] {
    [
        WIDTH_PT * scale,
        (BASE_PT + row_count(density) * ROW_PT) * scale,
    ]
}

/// The top-left position of a `window`-sized panel anchored in a corner of a
/// `monitor`-sized screen, inset by `margin`. Never returns a position that
/// puts the panel off the top-left edge.
pub fn anchor_pos(anchor: Anchor, monitor: [f32; 2], window: [f32; 2], margin: f32) -> [f32; 2] {
    let right = monitor[0] - window[0] - margin;
    let bottom = monitor[1] - window[1] - margin;
    let x = match anchor {
        Anchor::TopLeft | Anchor::BottomLeft => margin,
        Anchor::TopRight | Anchor::BottomRight => right,
    };
    let y = match anchor {
        Anchor::TopLeft | Anchor::TopRight => margin,
        Anchor::BottomLeft | Anchor::BottomRight => bottom,
    };
    [x.max(margin), y.max(margin)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_places_the_panel_in_each_corner() {
        // Catches: an anchor mapped to the wrong corner (a "bottom-right"
        // setting landing top-left), so the config silently does nothing.
        let m = [1920.0, 1080.0];
        let w = [260.0, 174.0];
        assert_eq!(anchor_pos(Anchor::TopLeft, m, w, 16.0), [16.0, 16.0]);
        assert_eq!(anchor_pos(Anchor::TopRight, m, w, 16.0), [1644.0, 16.0]);
        assert_eq!(anchor_pos(Anchor::BottomLeft, m, w, 16.0), [16.0, 890.0]);
        assert_eq!(anchor_pos(Anchor::BottomRight, m, w, 16.0), [1644.0, 890.0]);
    }

    #[test]
    fn anchor_keeps_the_panel_on_screen_when_larger_than_the_monitor() {
        // Catches: a right/bottom anchor pushing an oversized panel into
        // negative coordinates (off the top-left edge, unreachable).
        let p = anchor_pos(Anchor::BottomRight, [200.0, 200.0], [400.0, 400.0], 16.0);
        assert_eq!(p, [16.0, 16.0]);
    }

    #[test]
    fn content_size_grows_with_density_and_scale() {
        // Catches: a fixed window size (content clipped, or a huge empty panel)
        // and a `scale` that does nothing.
        let zen = content_size(Density::Zen, 1.0);
        let std = content_size(Density::Standard, 1.0);
        let full = content_size(Density::Full, 1.0);
        assert!(
            zen[1] < std[1] && std[1] < full[1],
            "taller with more rows: {zen:?} {std:?} {full:?}"
        );
        assert!(zen[0] > 0.0 && zen[1] > 0.0, "positive size: {zen:?}");
        assert_eq!(
            content_size(Density::Standard, 2.0),
            [std[0] * 2.0, std[1] * 2.0]
        );
    }
}
