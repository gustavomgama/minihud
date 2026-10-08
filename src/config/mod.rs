use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub x: i32,
    pub y: i32,
    pub opacity: f32,
    pub text_size: f32,
    pub show_frametime_graph: bool,
    pub update_hw_ms: u64,
    pub idle_hw_ms: u64,
    pub click_through: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            x: 16,
            y: 16,
            opacity: 0.95,
            text_size: 14.0,
            show_frametime_graph: true,
            update_hw_ms: 700,
            idle_hw_ms: 2000,
            click_through: true,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("minihud.toml")
    }

    pub fn load() -> Self {
        let p = Self::path();
        let raw = match std::fs::read_to_string(&p) {
            Ok(s) => s,
            Err(_) => return Self::default(),
        };
        let mut cfg: Self = match toml::from_str(&raw) {
            Ok(c) => c,
            Err(e) => {
                // Fail visibly, keep running on defaults: a typo must not
                // silently wedge the overlay into a weird state.
                tracing::warn!("config {:?} invalid ({e}); using defaults", p);
                return Self::default();
            }
        };
        // Clamp everything with a blast radius. update_hw_ms=0 would
        // busy-poll PDH every frame; absurd x/y parks the window
        // off-screen with no way to grab it back.
        cfg.update_hw_ms = cfg.update_hw_ms.clamp(50, 5000);
        cfg.idle_hw_ms = cfg.idle_hw_ms.clamp(cfg.update_hw_ms, 10000);
        cfg.opacity = cfg.opacity.clamp(0.2, 1.0);
        cfg.text_size = cfg.text_size.clamp(10.0, 28.0);
        cfg.x = cfg.x.clamp(-10000, 10000);
        cfg.y = cfg.y.clamp(-10000, 10000);
        cfg
    }

    pub fn save(&self) {
        let p = Self::path();
        if let Ok(s) = toml::to_string_pretty(self) {
            let _ = std::fs::write(p, s);
        }
    }

    /// Merge a toml snippet over defaults (what load() does with a file).
    #[cfg(test)]
    fn from_snippet(s: &str) -> Self {
        toml::from_str(s).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_config_overrides_only_listed_keys() {
        let cfg = Config::from_snippet("x = 100\nupdate_hw_ms = 123\n");
        assert_eq!(cfg.x, 100);
        assert_eq!(cfg.update_hw_ms, 123);
        // Everything else stays default.
        let d = Config::default();
        assert_eq!(cfg.y, d.y);
        assert_eq!(cfg.opacity, d.opacity);
        assert_eq!(cfg.click_through, d.click_through);
    }

    #[test]
    fn garbage_config_falls_back_to_defaults() {
        let cfg = Config::from_snippet("update_hw_ms = \"fast\"\n");
        assert_eq!(cfg, Config::default());
    }
}
