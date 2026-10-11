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

/// The top-left position of a `window`-sized panel anchored in a corner of the
/// monitor work area `work` (`[x, y, w, h]`, virtual-desktop points), inset by
/// `margin`. Never returns a position off the top-left edge of the area.
pub fn anchor_pos(anchor: Anchor, work: [f32; 4], window: [f32; 2], margin: f32) -> [f32; 2] {
    let (x0, y0, w, h) = (work[0], work[1], work[2], work[3]);
    let left = x0 + margin;
    let top = y0 + margin;
    let right = x0 + w - window[0] - margin;
    let bottom = y0 + h - window[1] - margin;
    let x = match anchor {
        Anchor::TopLeft | Anchor::BottomLeft => left,
        Anchor::TopRight | Anchor::BottomRight => right,
    };
    let y = match anchor {
        Anchor::TopLeft | Anchor::TopRight => top,
        Anchor::BottomLeft | Anchor::BottomRight => bottom,
    };
    [x.max(left), y.max(top)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_places_the_panel_in_each_corner() {
        // Catches: an anchor mapped to the wrong corner (a "bottom-right"
        // setting landing top-left), so the config silently does nothing.
        let work = [0.0, 0.0, 1920.0, 1080.0];
        let w = [260.0, 174.0];
        assert_eq!(anchor_pos(Anchor::TopLeft, work, w, 16.0), [16.0, 16.0]);
        assert_eq!(anchor_pos(Anchor::TopRight, work, w, 16.0), [1644.0, 16.0]);
        assert_eq!(anchor_pos(Anchor::BottomLeft, work, w, 16.0), [16.0, 890.0]);
        assert_eq!(
            anchor_pos(Anchor::BottomRight, work, w, 16.0),
            [1644.0, 890.0]
        );
    }

    #[test]
    fn anchor_respects_the_work_area_origin() {
        // Catches: a secondary monitor (origin != 0,0) anchoring the panel at
        // the primary's corner, or a taskbar-inset work area ignored (the panel
        // landing under the taskbar).
        let secondary = [1920.0, 0.0, 1920.0, 1080.0];
        assert_eq!(
            anchor_pos(Anchor::TopLeft, secondary, [260.0, 174.0], 16.0),
            [1936.0, 16.0],
            "a secondary monitor offsets the panel by its origin"
        );
        // A 40px taskbar shortens the work area: bottom-right sits above it.
        let with_taskbar = [0.0, 0.0, 1920.0, 1040.0];
        assert_eq!(
            anchor_pos(Anchor::BottomRight, with_taskbar, [260.0, 174.0], 16.0),
            [1644.0, 850.0],
            "the panel must clear the taskbar"
        );
    }

    #[test]
    fn anchor_keeps_the_panel_on_screen_when_larger_than_the_monitor() {
        // Catches: a right/bottom anchor pushing an oversized panel into
        // negative coordinates (off the top-left edge, unreachable).
        let p = anchor_pos(
            Anchor::BottomRight,
            [0.0, 0.0, 200.0, 200.0],
            [400.0, 400.0],
            16.0,
        );
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
