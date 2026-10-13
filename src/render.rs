//! Text rendering of one `HwStats` sample.
//!
//! Kept separate from the entry point so the formatting is a pure,
//! unit-tested unit. Missing values print as `--`.

use crate::hw::HwStats;

/// One hardware sample, formatted as a single text line.
pub fn line(s: &HwStats) -> String {
    format!(
        "CPU {:.0}% {} {} {} | RAM {} | GPU {} {} {} {}x{}MHz | {}{}",
        s.cpu_percent,
        f32u(s.cpu_temp_c, "C"),
        f32u(s.cpu_power_w, "W"),
        u32u(s.cpu_clock_mhz, "MHz"),
        ram(s.ram_used_mb, s.ram_total_mb),
        f32u(s.gpu_percent, "%"),
        f32u(s.gpu_temp_c, "C"),
        f32u(s.gpu_power_w, "W"),
        u32u(s.gpu_core_mhz, ""),
        u32u(s.gpu_mem_mhz, ""),
        vram(s),
        names(s),
    )
}

fn f32u(v: Option<f32>, unit: &str) -> String {
    v.map(|v| format!("{v:.0}{unit}"))
        .unwrap_or_else(|| "--".to_string())
}

fn u32u(v: Option<u32>, unit: &str) -> String {
    v.map(|v| format!("{v}{unit}"))
        .unwrap_or_else(|| "--".to_string())
}

fn ram(used: Option<u64>, total: Option<u64>) -> String {
    match (used, total) {
        (Some(u), Some(t)) => format!("{u}/{t}MB"),
        _ => "--".to_string(),
    }
}

fn vram(s: &HwStats) -> String {
    match (s.gpu_vram_used_mb, s.gpu_vram_total_mb) {
        (Some(u), Some(t)) => format!("VRAM {u}/{t}MB"),
        _ => "VRAM --".to_string(),
    }
}

fn names(s: &HwStats) -> String {
    match (&s.cpu_name, &s.gpu_name) {
        (Some(c), Some(g)) => format!("[{c} / {g}]"),
        (Some(c), None) => format!("[{c}]"),
        (None, Some(g)) => format!("[{g}]"),
        (None, None) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_optional_numbers() {
        assert_eq!(f32u(Some(61.4), "C"), "61C");
        assert_eq!(f32u(None, "C"), "--");
        assert_eq!(u32u(Some(4200), "MHz"), "4200MHz");
        assert_eq!(u32u(None, ""), "--");
    }

    #[test]
    fn formats_ram_and_vram() {
        assert_eq!(ram(Some(100), Some(200)), "100/200MB");
        assert_eq!(ram(Some(100), None), "--");
        let s = HwStats {
            gpu_vram_used_mb: Some(3000),
            gpu_vram_total_mb: Some(8192),
            ..HwStats::default()
        };
        assert_eq!(vram(&s), "VRAM 3000/8192MB");
        assert_eq!(vram(&HwStats::default()), "VRAM --");
    }

    #[test]
    fn formats_names() {
        let mut s = HwStats::default();
        assert_eq!(names(&s), "");
        s.cpu_name = Some("CPU".into());
        assert_eq!(names(&s), "[CPU]");
        s.gpu_name = Some("GPU".into());
        assert_eq!(names(&s), "[CPU / GPU]");
        s.cpu_name = None;
        assert_eq!(names(&s), "[GPU]");
    }

    #[test]
    fn line_prints_dashes_for_missing_values() {
        let l = line(&HwStats::default());
        assert!(l.contains("--"), "missing values must render as --: {l}");
    }
}
