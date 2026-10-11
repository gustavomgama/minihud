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
    /// Optional `--match` glob to follow, so a set-and-forget overlay can pin a
    /// target without a CLI flag. A `--match` on the command line wins.
    pub target: Option<String>,
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
            target: None,
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

    /// Load a `minihud.toml`. A **missing** file is not an error — it yields the
    /// defaults, so the overlay works out of the box; a present-but-broken file
    /// is an error the caller can report.
    pub fn load(path: &std::path::Path) -> Result<Config, LoadError> {
        match std::fs::read_to_string(path) {
            Ok(s) => Config::from_toml(&s).map_err(LoadError::Parse),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(LoadError::Io(e)),
        }
    }
}

/// Resolve the effective config: the first existing `minihud.toml` next to the
/// executable or in the working directory, else the defaults. A present-but-
/// broken file logs and falls back to the defaults so the overlay still starts.
pub fn resolve() -> Config {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.join("minihud.toml")));
    let cwd = Some(std::path::PathBuf::from("minihud.toml"));
    for path in [exe, cwd].into_iter().flatten() {
        if path.exists() {
            return match Config::load(&path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("{path:?}: {e}; using default config");
                    Config::default()
                }
            };
        }
    }
    Config::default()
}

/// Why a config file could not be loaded.
#[derive(Debug)]
pub enum LoadError {
    /// The file exists but could not be read.
    Io(std::io::Error),
    /// The file was read but is not valid `minihud.toml`.
    Parse(toml::de::Error),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Io(e) => write!(f, "reading config: {e}"),
            LoadError::Parse(e) => write!(f, "parsing config: {e}"),
        }
    }
}

impl std::error::Error for LoadError {}

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
        assert_eq!(c.target, None);
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

    #[test]
    fn target_defaults_to_none_and_parses_from_toml() {
        // Catches: the set-and-forget `target` being ignored (a pinned game
        // never followed) or a stray default target.
        assert_eq!(Config::default().target, None);
        let c = Config::from_toml("target = \"deadlock*.exe\"\n").expect("parse");
        assert_eq!(c.target.as_deref(), Some("deadlock*.exe"));
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("minihud-cfg-{}-{n}-{tag}.toml", std::process::id()))
    }

    #[test]
    fn a_missing_config_file_loads_the_defaults() {
        // Catches: a missing minihud.toml refusing to start the overlay instead
        // of using the out-of-the-box defaults.
        let p = temp_path("missing");
        assert_eq!(
            Config::load(&p).expect("missing is not an error"),
            Config::default()
        );
    }

    #[test]
    fn an_existing_config_file_is_parsed() {
        // Catches: the loader ignoring the file (always defaults), so the user
        // settings never apply.
        let p = temp_path("present");
        std::fs::write(&p, "density = \"zen\"\nopacity = 0.4\n").expect("write");
        let c = Config::load(&p).expect("load");
        std::fs::remove_file(&p).ok();
        assert_eq!(c.density, Density::Zen);
        assert_eq!(c.opacity, 0.4);
    }

    #[test]
    fn a_broken_config_file_is_an_error() {
        // Catches: a syntax error silently swallowed, leaving the user thinking
        // their settings applied when the file was never parsed.
        let p = temp_path("broken");
        std::fs::write(&p, "density = [ oops\n").expect("write");
        let r = Config::load(&p);
        std::fs::remove_file(&p).ok();
        assert!(r.is_err(), "a broken file must surface an error");
    }
}
