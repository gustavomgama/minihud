//! Overlay colour system: accents and threshold tones.
//!
//! Pure integer maths, no GUI dependency, so it is unit-tested in the default
//! suite. Colours are `[u8; 3]` RGB; the egui layer converts them.
//!
//! Two ideas drive the palette:
//!
//! - **Thermal accent** — one hue that drifts cool→warm with temperature, so
//!   the whole overlay reads "hot" or "cool" without a legend.
//! - **Threshold tones** — load bars go calm → amber → alarm, so the eye finds
//!   the problem without reading the number.

use super::config::Accent;

/// An 8-bit sRGB colour.
pub type Rgb = [u8; 3];

/// A neutral cool tone for a thermal accent with no temperature reading.
const NEUTRAL: Rgb = [150, 180, 200];

/// The fixed colour for a non-thermal accent.
pub fn accent_color(a: Accent) -> Rgb {
    match a {
        Accent::Thermal => NEUTRAL,
        Accent::Cyan => [90, 200, 255],
        Accent::Amber => [240, 180, 60],
        Accent::Lime => [150, 230, 90],
        Accent::Magenta => [230, 110, 220],
    }
}

/// The thermal accent at `temp_c`: cool (blue) when cold, warm (red) when hot.
pub fn thermal(temp_c: f32) -> Rgb {
    let t = ((temp_c - 40.0) / 45.0).clamp(0.0, 1.0);
    lerp([80, 180, 255], [255, 90, 60], t)
}

/// The bar tone for a `0..=1` load: green, then amber past 70%, then red past
/// 85%. The bands are deliberately wide, so a normal load stays calm and only a
/// genuinely pegged one alarms — and a value that rounds to "90%" already reads
/// alarm, so the number and the bar never disagree.
pub fn load_color(fraction: f32) -> Rgb {
    if fraction < 0.7 {
        [60, 200, 120]
    } else if fraction < 0.85 {
        [235, 175, 45]
    } else {
        [235, 70, 70]
    }
}

/// A temperature tone: green below 65 °C, amber to 80 °C, red above, so a
/// temperature reads at a glance like a load bar does.
pub fn temp_color(temp_c: f32) -> Rgb {
    if temp_c < 65.0 {
        [60, 200, 120]
    } else if temp_c < 80.0 {
        [235, 175, 45]
    } else {
        [235, 70, 70]
    }
}

/// The accent to draw with: thermal follows the temperature, the rest are
/// fixed. A thermal accent with no reading falls back to [`NEUTRAL`].
pub fn resolve_accent(a: Accent, temp_c: Option<f32>) -> Rgb {
    match a {
        Accent::Thermal => temp_c.map(thermal).unwrap_or(NEUTRAL),
        fixed => accent_color(fixed),
    }
}

/// Linear blend `a..b` at `t` (clamped), rounding to the nearest channel.
fn lerp(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    [mix(a[0], b[0]), mix(a[1], b[1]), mix(a[2], b[2])]
}

/// Parse a `#rgb` or `#rrggbb` colour (the `#` optional) into RGB. `None` for
/// anything else, so a bad config value falls back rather than panics.
pub fn parse_hex(s: &str) -> Option<Rgb> {
    let h = s.strip_prefix('#').unwrap_or(s);
    let pair = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
    let nibble = |i: usize| {
        u8::from_str_radix(h.get(i..i + 1)?, 16)
            .ok()
            .map(|v| v * 17)
    };
    match h.len() {
        3 => Some([nibble(0)?, nibble(1)?, nibble(2)?]),
        6 => Some([pair(0)?, pair(2)?, pair(4)?]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_greenish(c: Rgb) -> bool {
        c[1] > c[0] && c[1] > c[2]
    }
    fn is_reddish(c: Rgb) -> bool {
        c[0] > c[1] && c[0] > c[2]
    }
    fn is_bluish(c: Rgb) -> bool {
        c[2] > c[0]
    }
    /// A true alarm tone (red), distinct from amber: red carries little green.
    fn is_alarm(c: Rgb) -> bool {
        c[0] > 200 && c[1] < 120
    }

    #[test]
    fn load_color_goes_calm_amber_alarm() {
        // Catches: a load bar that never changes tone, so a pegged GPU looks
        // identical to an idle one.
        let low = load_color(0.2);
        assert!(is_greenish(low), "low load must read calm: {low:?}");
        let mid = load_color(0.7);
        assert!(
            mid[0] > mid[2] && mid[1] > mid[2],
            "mid load must read amber: {mid:?}"
        );
        let high = load_color(0.95);
        assert!(is_reddish(high), "high load must read alarm: {high:?}");
        // A value that rounds to "90%" must already read alarm, or the number
        // and the bar disagree.
        assert!(is_alarm(load_color(0.88)), "88% must read alarm");
    }

    #[test]
    fn a_moderate_load_stays_calm() {
        // Catches: amber firing too early, so a normal ~65% load looks alarming.
        let moderate = load_color(0.65);
        assert!(
            is_greenish(moderate),
            "65% must still read calm: {moderate:?}"
        );
    }

    #[test]
    fn thermal_reads_cool_when_cold_and_warm_when_hot() {
        // Catches: a thermal accent with no gradient (cool==hot) — the whole
        // point of the accent would be lost.
        let cold = thermal(35.0);
        let hot = thermal(90.0);
        assert!(is_bluish(cold), "cold must skew blue: {cold:?}");
        assert!(is_reddish(hot), "hot must skew red: {hot:?}");
        assert!(hot[0] > cold[0], "hot must carry more red than cold");
    }

    #[test]
    fn thermal_clamps_beyond_its_range() {
        // Catches: out-of-range temperatures extrapolating the lerp past 0/255
        // (wrapping or saturating into nonsense) instead of clamping.
        assert_eq!(thermal(-20.0), thermal(0.0), "below range clamps");
        assert_eq!(thermal(200.0), thermal(120.0), "above range clamps");
    }

    #[test]
    fn fixed_accents_keep_their_hue() {
        // Catches: all named accents collapsing to one colour (the picker would
        // do nothing).
        assert!(is_bluish(accent_color(Accent::Cyan)));
        let amber = accent_color(Accent::Amber);
        assert!(amber[0] > amber[2], "amber must skew warm: {amber:?}");
        assert_eq!(accent_color(Accent::Thermal), NEUTRAL);
    }

    #[test]
    fn resolve_accent_follows_temperature_only_for_thermal() {
        // Catches: a fixed accent drifting with temperature (it must not), or a
        // thermal accent ignoring the reading.
        assert_eq!(resolve_accent(Accent::Thermal, Some(90.0)), thermal(90.0));
        assert_eq!(resolve_accent(Accent::Thermal, None), NEUTRAL);
        assert_eq!(
            resolve_accent(Accent::Cyan, Some(90.0)),
            accent_color(Accent::Cyan)
        );
    }

    #[test]
    fn temp_color_reads_cool_then_hot() {
        // Catches: a temp tint that never changes (the "reads hot at a glance"
        // promise lost) or one inverted (a hot reading showing green).
        assert!(is_greenish(temp_color(45.0)));
        let warm = temp_color(72.0);
        assert!(
            warm[0] > warm[2] && warm[1] > warm[2],
            "amber at 72: {warm:?}"
        );
        assert!(is_reddish(temp_color(88.0)));
    }

    #[test]
    fn parse_hex_reads_six_and_three_digit_colours() {
        // Catches: a custom accent being ignored, or the short form mis-expanded
        // (`#f80` must become `#ff8800`, not `#0f0800`).
        assert_eq!(parse_hex("#ff8800"), Some([255, 136, 0]));
        assert_eq!(parse_hex("ff8800"), Some([255, 136, 0]));
        assert_eq!(parse_hex("#f80"), Some([255, 136, 0]));
        assert_eq!(parse_hex("#FFFFFF"), Some([255, 255, 255]));
    }

    #[test]
    fn parse_hex_rejects_garbage() {
        // Catches: a malformed value silently becoming a colour (e.g. black) —
        // it must be `None` so the caller falls back.
        assert_eq!(parse_hex("nope"), None);
        assert_eq!(parse_hex("#ff88"), None, "four digits is not a colour");
        assert_eq!(parse_hex("#gggggg"), None, "non-hex digits");
        assert_eq!(parse_hex(""), None);
    }
}
