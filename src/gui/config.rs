//! Overlay configuration: the `minihud.toml` schema.
//!
//! Defaults are the **set-and-forget** baseline — an empty (or absent) file
//! still yields a complete, sensible overlay. Every field is overridable for
//! the "full control" audience. Out-of-range numbers are clamped rather than
//! rejected, so a typo degrades gracefully instead of refusing to start.

use serde::Deserialize;

/// How much of the hardware detail the overlay shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Density {
    /// FPS + one hero metric.
    Zen,
    /// FPS + CPU / GPU / RAM bars.
    Standard,
    /// Every field, plus an FPS sparkline.
    Full,
}

/// Which screen corner the overlay sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Anchor {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// The accent tone. `Thermal` derives the hue from temperature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Accent {
    Thermal,
    Cyan,
    Amber,
    Lime,
    Magenta,
}

/// The overlay's full configuration.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub density: Density,
    pub anchor: Anchor,
    pub margin_px: u32,
    pub opacity: f32,
    pub scale: f32,
    pub accent: Accent,
    pub poll_ms: u64,
    pub fade_idle_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            density: Density::Standard,
            anchor: Anchor::TopLeft,
            margin_px: 16,
            opacity: 0.72,
            scale: 1.0,
            accent: Accent::Thermal,
            poll_ms: 500,
            fade_idle_secs: 0,
        }
    }
}

impl Config {
    /// Parse a `minihud.toml` body, filling missing fields from the defaults
    /// and clamping the numbers into range.
    pub fn from_toml(s: &str) -> Result<Config, toml::de::Error> {
        let cfg: Config = toml::from_str(s)?;
        Ok(cfg.normalized())
    }

    /// Clamp every numeric field into its supported range.
    fn normalized(self) -> Self {
        Self {
            margin_px: self.margin_px.min(200),
            opacity: self.opacity.clamp(0.1, 1.0),
            scale: self.scale.clamp(0.5, 3.0),
            poll_ms: self.poll_ms.clamp(100, 10_000),
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_set_and_forget_baseline() {
        // Catches: a default that ships a broken overlay (invisible opacity, a
        // zero poll that busy-spins, no margin flush to the screen edge).
        let c = Config::default();
        assert_eq!(c.density, Density::Standard);
        assert_eq!(c.anchor, Anchor::TopLeft);
        assert_eq!(c.margin_px, 16);
        assert_eq!(c.opacity, 0.72);
        assert_eq!(c.scale, 1.0);
        assert_eq!(c.accent, Accent::Thermal);
        assert_eq!(c.poll_ms, 500);
        assert_eq!(c.fade_idle_secs, 0);
    }

    #[test]
    fn an_empty_toml_yields_the_defaults() {
        // Catches: an empty/absent config failing to produce a usable overlay.
        assert_eq!(
            Config::from_toml("").expect("empty config"),
            Config::default()
        );
    }

    #[test]
    fn parses_overrides_and_keeps_other_defaults() {
        // Catches: an override clobbering unrelated fields (a provided value
        // must not zero the rest).
        let c = Config::from_toml("density = \"zen\"\nanchor = \"bottom-right\"\nopacity = 0.5\n")
            .expect("parse");
        assert_eq!(c.density, Density::Zen);
        assert_eq!(c.anchor, Anchor::BottomRight);
        assert_eq!(c.opacity, 0.5);
        assert_eq!(c.margin_px, 16, "unset fields keep the default");
        assert_eq!(c.accent, Accent::Thermal);
    }

    #[test]
    fn clamps_out_of_range_values_instead_of_rejecting() {
        // Catches: an absurd opacity/scale/poll making the overlay unusable or
        // the process spin, when a clamp would have kept it running.
        let c = Config::from_toml("opacity = 2.5\nscale = 99.0\npoll_ms = 0\nmargin_px = 9999\n")
            .expect("parse");
        assert_eq!(c.opacity, 1.0);
        assert_eq!(c.scale, 3.0);
        assert_eq!(c.poll_ms, 100);
        assert_eq!(c.margin_px, 200);

        let low = Config::from_toml("opacity = -1.0\nscale = 0.0\npoll_ms = 5\n").expect("parse");
        assert_eq!(low.opacity, 0.1);
        assert_eq!(low.scale, 0.5);
        assert_eq!(low.poll_ms, 100);
    }

    #[test]
    fn rejects_an_unknown_field() {
        // Catches: a typo'd key silently ignored, so the user thinks their
        // setting applied when it did not.
        let e = Config::from_toml("opacty = 0.5\n").expect_err("unknown key must error");
        assert!(e.to_string().contains("opacty"), "{e}");
    }

    #[test]
    fn rejects_an_unknown_enum_value() {
        // Catches: an invalid density/anchor/accent silently falling back
        // instead of telling the user the value is not one of the choices.
        assert!(Config::from_toml("density = \"turbo\"\n").is_err());
    }
}
