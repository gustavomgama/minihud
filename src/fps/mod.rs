//! Independent per-app FPS / frametime capture.
//! Uses hook-rt phases; never blocks minihud; no external uninstall needed.
use std::time::{Duration, Instant};

pub struct Capture {
    last: Instant,
    frames: u32,
}
impl Capture {
    pub fn new() -> Self {
        Self {
            last: Instant::now(),
            frames: 0,
        }
    }
    pub fn tick(&mut self) -> Option<f32> {
        self.frames += 1;
        let now = Instant::now();
        if now.duration_since(self.last) >= Duration::from_millis(100) {
            let fps = self.frames as f32 / now.duration_since(self.last).as_secs_f32();
            self.frames = 0;
            self.last = now;
            Some(fps)
        } else {
            None
        }
    }
}

/// Per-app frametime (ms) from tick delta.
pub fn frametime_ms(&self, delta: std::time::Duration) -> f32 {
    delta.as_secs_f32() * 1000.0
}
