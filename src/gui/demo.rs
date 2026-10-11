//! Synthetic samples for previewing the overlay without hardware
//! (`MINIHUD_DEMO=1`).
//!
//! Pure and unit-tested: plausible, deterministic values that vary smoothly
//! with elapsed seconds, so a preview looks alive and two runs match.

use crate::hw::HwStats;

const GB: u64 = 1024;

/// A smooth oscillation in `[-amp, amp]`.
fn wave(t: f32, amp: f32, freq: f32, phase: f32) -> f32 {
    amp * (t * freq + phase).sin()
}

/// A plausible hardware sample at `t` seconds.
pub fn stats(t: f32) -> HwStats {
    HwStats {
        cpu_percent: 38.0 + wave(t, 22.0, 0.7, 0.0),
        cpu_temp_c: Some(54.0 + wave(t, 9.0, 0.5, 0.3)),
        cpu_power_w: Some(72.0 + wave(t, 24.0, 0.6, 0.5)),
        cpu_clock_mhz: Some((4150.0 + wave(t, 350.0, 0.9, 0.0)) as u32),
        ram_used_mb: Some(9 * GB + wave(t, 1.5 * GB as f32, 0.3, 0.0).max(0.0) as u64),
        ram_total_mb: Some(16 * GB),
        gpu_percent: Some(62.0 + wave(t, 28.0, 0.4, 1.0)),
        gpu_temp_c: Some(63.0 + wave(t, 11.0, 0.35, 0.0)),
        gpu_power_w: Some(185.0 + wave(t, 65.0, 0.5, 0.7)),
        gpu_core_mhz: Some((1950.0 + wave(t, 180.0, 0.8, 0.0)) as u32),
        gpu_mem_mhz: Some(7000),
        gpu_vram_used_mb: Some(3200 + wave(t, 1500.0, 0.25, 0.0).max(0.0) as u64),
        gpu_vram_total_mb: Some(8192),
        gpu_mem_used_mb: Some(3200),
        gpu_d3d_dedicated_mb: Some(3100),
        gpu_voltage_mv: Some(900),
        cpu_name: Some("AMD Ryzen 7 5800X".to_string()),
        gpu_name: Some("NVIDIA GeForce RTX 3070".to_string()),
    }
}

/// A plausible fps at `t` seconds.
pub fn fps(t: f32) -> f64 {
    118.0 + 18.0 * (t * 0.3).sin() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_stay_in_plausible_ranges() {
        // Catches: a demo sample that renders nonsense (negative load, a temp of
        // 300 C, over-100 % GPU) — the preview would look broken.
        for t in [0.0_f32, 3.0, 17.0, 100.0] {
            let s = stats(t);
            assert!((0.0..=100.0).contains(&s.cpu_percent), "cpu {t}: {s:?}");
            assert!((0.0..=100.0).contains(&s.gpu_percent.unwrap()), "gpu {t}");
            for temp in [s.cpu_temp_c.unwrap(), s.gpu_temp_c.unwrap()] {
                assert!((20.0..=110.0).contains(&temp), "temp {t}: {temp}");
            }
            assert!(
                s.ram_used_mb.unwrap() <= s.ram_total_mb.unwrap(),
                "ram used <= total at {t}"
            );
            assert!(
                s.gpu_vram_used_mb.unwrap() <= s.gpu_vram_total_mb.unwrap(),
                "vram used <= total at {t}"
            );
            assert!((30.0..=300.0).contains(&fps(t)), "fps {t}");
        }
    }

    #[test]
    fn it_varies_over_time() {
        // Catches: a static sample (the preview never moves; the sparkline and
        // the value roll never show).
        assert_ne!(stats(0.0).cpu_percent, stats(2.0).cpu_percent);
        assert_ne!(fps(0.0), fps(4.0));
    }

    #[test]
    fn it_is_deterministic() {
        // Catches: a random sample (two runs of a preview differ, and the value
        // roll would chase noise).
        assert_eq!(stats(5.0).cpu_percent, stats(5.0).cpu_percent);
        assert_eq!(fps(5.0), fps(5.0));
    }
}
