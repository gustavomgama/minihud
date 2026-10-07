use smallvec::SmallVec;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct FrameSample {
    pub dt_ms: f32,
}

#[derive(Clone)]
pub struct FrameStats {
    pub fps: f32,
    pub avg_ms: f32,
    pub min_ms: f32,
    pub max_ms: f32,
}

pub struct PresentTimer {
    last: Option<Instant>,
    samples: SmallVec<[f32; 256]>,
    max_samples: usize,
}

impl PresentTimer {
    pub fn new(max_samples: usize) -> Self {
        Self {
            last: None,
            samples: SmallVec::with_capacity(max_samples),
            max_samples,
        }
    }

    pub fn tick(&mut self) -> Option<FrameSample> {
        let now = Instant::now();
        let dt = match self.last {
            Some(l) => now.duration_since(l),
            None => {
                self.last = Some(now);
                return None;
            }
        };
        self.last = Some(now);
        let dt_ms = dt.as_secs_f32() * 1000.0;
        if self.samples.len() >= self.max_samples {
            self.samples.remove(0);
        }
        self.samples.push(dt_ms);
        Some(FrameSample { dt_ms })
    }

    pub fn stats(&self) -> FrameStats {
        if self.samples.is_empty() {
            return FrameStats {
                fps: 0.0,
                avg_ms: 0.0,
                min_ms: 0.0,
                max_ms: 0.0,
            };
        }
        let mut sum = 0.0f32;
        let mut minv = self.samples[0];
        let mut maxv = self.samples[0];
        for &v in &self.samples {
            sum += v;
            if v < minv {
                minv = v;
            }
            if v > maxv {
                maxv = v;
            }
        }
        let n = self.samples.len() as f32;
        let avg_ms = sum / n;
        let fps = if avg_ms <= 0.0 { 0.0 } else { 1000.0 / avg_ms };
        FrameStats {
            fps,
            avg_ms,
            min_ms: minv,
            max_ms: maxv,
        }
    }

    pub fn samples_ms(&self) -> &[f32] {
        &self.samples
    }
}
