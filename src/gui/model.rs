//! Pure overlay row model.
//!
//! Turns one hardware sample into the display rows the overlay draws. Kept
//! free of any GUI dependency so it compiles into the default headless build
//! and is exercised by the normal test suite. Missing sensors render as `--`,
//! never a guessed zero.

use crate::hw::HwStats;

use super::config::Density;
use super::theme::{self, Rgb};

/// One rendered metric row.
#[derive(Debug, Clone, PartialEq)]
pub struct Metric {
    /// Human label, e.g. `"CPU temp"`.
    pub label: &'static str,
    /// Formatted value, or `"--"` when the sensor is missing.
    pub value: String,
    /// A `0..=1` fill fraction for a bar, when the metric is a proportion.
    pub fraction: Option<f32>,
    /// An explicit text tint (temperature rows), else the theme default.
    pub tint: Option<Rgb>,
}

/// The display rows for one sample, in display order.
pub fn rows(s: &HwStats) -> Vec<Metric> {
    vec![
        Metric {
            label: "CPU",
            value: pct(s.cpu_percent),
            fraction: Some((s.cpu_percent / 100.0).clamp(0.0, 1.0)),
            tint: None,
        },
        Metric {
            label: "CPU temp",
            value: opt_f32(s.cpu_temp_c, "°C"),
            fraction: None,
            tint: s.cpu_temp_c.map(theme::temp_color),
        },
        Metric {
            label: "RAM",
            value: pair_mem(s.ram_used_mb, s.ram_total_mb),
            fraction: ratio(s.ram_used_mb, s.ram_total_mb),
            tint: None,
        },
        Metric {
            label: "GPU",
            value: s.gpu_percent.map(pct).unwrap_or_else(dashes),
            fraction: s.gpu_percent.map(|p| (p / 100.0).clamp(0.0, 1.0)),
            tint: None,
        },
        Metric {
            label: "GPU temp",
            value: opt_f32(s.gpu_temp_c, "°C"),
            fraction: None,
            tint: s.gpu_temp_c.map(theme::temp_color),
        },
        Metric {
            label: "VRAM",
            value: pair_mem(s.gpu_vram_used_mb, s.gpu_vram_total_mb),
            fraction: ratio(s.gpu_vram_used_mb, s.gpu_vram_total_mb),
            tint: None,
        },
        Metric {
            label: "GPU power",
            value: opt_f32(s.gpu_power_w, "W"),
            fraction: None,
            tint: None,
        },
    ]
}

/// The rows for a density. Zen keeps only the hero metric; Standard keeps the
/// three headline bars; Full keeps everything. FPS is not a row — it is drawn
/// by the overlay itself, in every density.
pub fn rows_for(density: Density, s: &HwStats) -> Vec<Metric> {
    let keep: &[&str] = match density {
        Density::Zen => &["GPU"],
        Density::Standard => &["CPU", "RAM", "GPU"],
        Density::Full => return rows(s),
    };
    rows(s)
        .into_iter()
        .filter(|m| keep.contains(&m.label))
        .collect()
}

/// The rows for an explicit, user-chosen list of labels (case-insensitive), in
/// the given order. Unknown names are skipped, so a typo drops one row rather
/// than failing the whole overlay.
pub fn rows_named(s: &HwStats, names: &[String]) -> Vec<Metric> {
    let all = rows(s);
    names
        .iter()
        .filter_map(|name| {
            let want = name.trim().to_ascii_lowercase();
            all.iter()
                .find(|m| m.label.to_ascii_lowercase() == want)
                .cloned()
        })
        .collect()
}

/// The missing-sensor placeholder. Every `--` in the overlay comes from here.
fn dashes() -> String {
    "--".to_string()
}

fn pct(v: f32) -> String {
    format!("{v:.0}%")
}

/// `Some` → rounded value with unit, `None` → `--`.
fn opt_f32(v: Option<f32>, unit: &str) -> String {
    v.map(|v| format!("{v:.0}{unit}")).unwrap_or_else(dashes)
}

/// `used/total` with a single unit — GB above a GiB, else MB — or `--` unless
/// both are known.
fn pair_mem(used: Option<u64>, total: Option<u64>) -> String {
    match (used, total) {
        (Some(u), Some(t)) if t >= 1024 => {
            format!("{:.1}/{:.1} GB", u as f32 / 1024.0, t as f32 / 1024.0)
        }
        (Some(u), Some(t)) => format!("{u}/{t} MB"),
        _ => dashes(),
    }
}

/// `used/total` as a `0..=1` fill, or `None` unless both are known.
fn ratio(used: Option<u64>, total: Option<u64>) -> Option<f32> {
    match (used, total) {
        (Some(u), Some(t)) if t > 0 => Some((u as f32 / t as f32).clamp(0.0, 1.0)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row<'a>(rows: &'a [Metric], label: &str) -> &'a Metric {
        rows.iter()
            .find(|m| m.label == label)
            .unwrap_or_else(|| panic!("no {label} row in {rows:?}"))
    }

    #[test]
    fn a_missing_gpu_temp_renders_as_dashes() {
        // Catches: printing a guessed 0°C when the sensor is absent, which is
        // indistinguishable from a real cold reading.
        let r = rows(&HwStats::default());
        assert_eq!(row(&r, "GPU temp").value, "--");
    }

    #[test]
    fn a_present_gpu_temp_rounds_and_units() {
        // Catches: dropping the unit, or not rounding the raw float.
        let s = HwStats {
            gpu_temp_c: Some(61.4),
            ..HwStats::default()
        };
        assert_eq!(row(&rows(&s), "GPU temp").value, "61°C");
    }

    #[test]
    fn cpu_load_exposes_a_bounded_fraction() {
        // Catches: a bar fraction outside 0..=1 (an over-100% load would draw
        // past the track) and a missing fraction where a proportion is expected.
        let s = HwStats {
            cpu_percent: 42.0,
            ..HwStats::default()
        };
        let cpu = row(&rows(&s), "CPU").clone();
        assert_eq!(cpu.value, "42%");
        assert_eq!(cpu.fraction, Some(0.42));

        let over = HwStats {
            cpu_percent: 150.0,
            ..HwStats::default()
        };
        assert_eq!(row(&rows(&over), "CPU").fraction, Some(1.0));
    }

    #[test]
    fn the_full_row_set_is_present_and_ordered() {
        // Catches: a missing metric (silently dropped from the overlay) or a
        // reshuffle that pushes the heroic CPU/GPU rows out of position.
        let labels: Vec<_> = rows(&HwStats::default()).iter().map(|m| m.label).collect();
        assert_eq!(
            labels,
            [
                "CPU",
                "CPU temp",
                "RAM",
                "GPU",
                "GPU temp",
                "VRAM",
                "GPU power"
            ]
        );
    }

    #[test]
    fn gpu_load_and_temp_render_the_sample() {
        // Catches: the GPU rows reading the CPU fields (or not bar-filling load).
        let s = HwStats {
            gpu_percent: Some(30.0),
            gpu_temp_c: Some(60.0),
            ..HwStats::default()
        };
        let r = rows(&s);
        assert_eq!(row(&r, "GPU").value, "30%");
        assert_eq!(row(&r, "GPU").fraction, Some(0.3));
        assert_eq!(row(&r, "GPU temp").value, "60°C");
    }

    #[test]
    fn ram_row_shows_used_over_total_and_fills() {
        // Catches: a RAM row that hides one of the two numbers, or a bar that
        // does not track the used/total ratio.
        let s = HwStats {
            ram_used_mb: Some(8192),
            ram_total_mb: Some(16384),
            ..HwStats::default()
        };
        let m = row(&rows(&s), "RAM").clone();
        assert_eq!(m.value, "8.0/16.0 GB");
        assert_eq!(m.fraction, Some(0.5));
    }

    #[test]
    fn ram_row_is_dashes_without_both_numbers() {
        // Catches: showing "8192/--" (or a bar) when the total is unknown.
        let s = HwStats {
            ram_used_mb: Some(8192),
            ..HwStats::default()
        };
        let m = row(&rows(&s), "RAM").clone();
        assert_eq!(m.value, "--");
        assert_eq!(m.fraction, None);
    }

    #[test]
    fn small_memory_stays_in_megabytes() {
        // Catches: forcing GB on small values (a confusing "0.5/0.9 GB").
        let s = HwStats {
            ram_used_mb: Some(512),
            ram_total_mb: Some(900),
            ..HwStats::default()
        };
        assert_eq!(row(&rows(&s), "RAM").value, "512/900 MB");
    }

    #[test]
    fn vram_row_reports_used_over_total() {
        // Catches: the VRAM row reading system RAM, or omitting the unit.
        let s = HwStats {
            gpu_vram_used_mb: Some(3000),
            gpu_vram_total_mb: Some(8192),
            ..HwStats::default()
        };
        let m = row(&rows(&s), "VRAM").clone();
        assert_eq!(m.value, "2.9/8.0 GB");
        assert_eq!(m.fraction, Some(3000.0 / 8192.0));
    }

    fn labels(metrics: &[Metric]) -> Vec<&'static str> {
        metrics.iter().map(|m| m.label).collect()
    }

    #[test]
    fn zen_shows_only_the_hero_metric() {
        // Catches: Zen treating the density as a no-op (the whole "one glance"
        // promise gone) or picking a metric that is often `--`.
        assert_eq!(
            labels(&rows_for(Density::Zen, &HwStats::default())),
            ["GPU"]
        );
    }

    #[test]
    fn standard_shows_the_three_headline_bars() {
        // Catches: Standard dropping one of the headline bars, or reordering
        // them so CPU/GPU move around.
        assert_eq!(
            labels(&rows_for(Density::Standard, &HwStats::default())),
            ["CPU", "RAM", "GPU"]
        );
    }

    #[test]
    fn full_shows_every_metric_in_display_order() {
        // Catches: Full filtering anything out (the "everything" option showing
        // less than everything) or reordering the rows.
        let all = rows(&HwStats::default());
        assert_eq!(
            labels(&rows_for(Density::Full, &HwStats::default())),
            labels(&all)
        );
    }

    #[test]
    fn temperature_rows_carry_a_thermal_tint() {
        // Catches: a temperature rendering in the default colour (the "reads hot
        // at a glance" promise lost) or a missing reading carrying a tint.
        let hot = HwStats {
            gpu_temp_c: Some(88.0),
            ..HwStats::default()
        };
        assert_eq!(
            row(&rows(&hot), "GPU temp").tint,
            Some(theme::temp_color(88.0)),
            "a hot temp must carry the thermal tint"
        );
        let cold_cpu = HwStats {
            cpu_temp_c: Some(45.0),
            ..HwStats::default()
        };
        assert_eq!(
            row(&rows(&cold_cpu), "CPU temp").tint,
            Some(theme::temp_color(45.0))
        );
        assert_eq!(row(&rows(&HwStats::default()), "GPU temp").tint, None);
        assert_eq!(row(&rows(&HwStats::default()), "CPU").tint, None);
    }

    #[test]
    fn named_rows_select_and_order_by_the_given_names() {
        // Catches: a custom `metrics` list ignored (the density wins) or
        // reordered against the user's choice.
        let names = vec!["GPU".to_string(), "CPU".to_string()];
        let got: Vec<_> = rows_named(&HwStats::default(), &names)
            .iter()
            .map(|m| m.label)
            .collect();
        assert_eq!(got, ["GPU", "CPU"]);
    }

    #[test]
    fn named_rows_match_case_insensitively_and_skip_unknown() {
        // Catches: a case difference dropping a row, or an unknown name
        // panicking instead of being skipped.
        let names = vec!["ram".to_string(), "Nope".to_string()];
        let got: Vec<_> = rows_named(&HwStats::default(), &names)
            .iter()
            .map(|m| m.label)
            .collect();
        assert_eq!(got, ["RAM"]);
    }
}
