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
        std::fs::read_to_string(p)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let p = Self::path();
        if let Ok(s) = toml::to_string_pretty(self) {
            let _ = std::fs::write(p, s);
        }
    }
}
