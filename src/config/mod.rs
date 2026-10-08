use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub x: i32,
    pub y: i32,
    pub opacity: f32,
    pub text_size: f32,
    pub show_frametime_graph: bool,
    pub update_hw_ms: u64,
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
            update_hw_ms: 100,
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
}
