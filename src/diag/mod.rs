//! minihud's developer diagnostics: the per-run persisted record (a verbose
//! text log plus a structured JSONL, written next to the exe on every run) and
//! the `--diagnostics` live trace surface.
//!
//! Diagnostics are developer-only: nothing here reaches the end user. The
//! persisted files are written silently on every run, and the live stream only
//! gains trace-level output when `--diagnostics` is passed.
//!
//! The two halves share one tracing layer ([`layer`]): the console layer
//! decides what a user sees (info by default, trace with `--diagnostics`),
//! while the diagnostics layer takes every event at every depth, so the
//! persisted record is complete whatever the console filter does.

pub mod console;
pub mod layer;
pub mod lifecycle;
mod record;
mod sink;
pub mod time;

use std::sync::{Arc, OnceLock};

use crate::hw::{HwStats, LhmSensor};

pub use sink::Diagnostics;

/// The folder (next to the exe) that holds every run's artifacts, so `cargo
/// clean` reclaims the whole tree by removing `target/`.
pub const DIAGNOSTICS_DIR: &str = "diagnostics";

/// The process-wide sink behind the tracing layer. Pending until [`begin`]
/// opens the run, so a process that hands off to its elevated copy writes
/// nothing of its own.
static SINK: OnceLock<Arc<Diagnostics>> = OnceLock::new();

/// The tracing layer that persists every event and span.
pub fn layer() -> layer::DiagLayer {
    layer::DiagLayer::new(sink().clone())
}

/// The shared sink. Opening a run is [`begin`]'s job.
pub fn sink() -> &'static Arc<Diagnostics> {
    SINK.get_or_init(|| Arc::new(Diagnostics::pending()))
}

/// Start this run's persisted record: `diagnostics/run-DD_MM_YYYY-HH-MM-SS/`
/// next to the exe, so `cargo clean` reclaims it together with `target/`.
///
/// Called once, after the elevation-relaunch decision, so only the process
/// that actually runs the app leaves a record. Best effort: an unwritable
/// folder degrades this run and never stops the status loop.
pub fn begin(argv: &[String], elevated: bool) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent().map(|p| p.to_path_buf()) else {
        return;
    };
    let (secs, _) = time::now_parts();
    if sink().open(&run_root(&dir), time::run_id(secs)) {
        lifecycle::record_start(sink(), argv, &exe, elevated);
    }
}

/// Where a run's artifacts live under `exe_dir`: a `diagnostics` folder beside
/// the exe, one timestamped folder per run.
fn run_root(exe_dir: &std::path::Path) -> std::path::PathBuf {
    exe_dir.join(DIAGNOSTICS_DIR)
}

/// The complete sensor dump of one poll, at trace depth in both the live stream
/// and the persisted record.
pub fn sensor_poll(sink: &Diagnostics, index: u64, rows: &[LhmSensor]) {
    if !sink.opened() {
        return;
    }
    tracing::trace!(
        target: "minihud::poll",
        kind = "sensor_poll",
        sensor_poll.index = index,
        sensor_poll.count = rows.len() as u64,
        sensor_values = %serde_json::to_value(rows).unwrap_or_default(),
        "sensor poll #{index} count={}",
        rows.len()
    );
}

/// The per-render stats: the rendered line and every value behind it, at trace
/// depth in both the live stream and the persisted record.
pub fn render_stat(sink: &Diagnostics, line: &str, stats: &HwStats) {
    if !sink.opened() {
        return;
    }
    tracing::trace!(
        target: "minihud::render",
        kind = "render_stat",
        render_stats = %render_stats(line, stats),
        line = line,
        "render stats"
    );
}

/// The complete stat set behind one rendered line: the line itself plus every
/// field of the sample it was drawn from, so the persisted record carries the
/// same numbers the developer saw on screen.
fn render_stats(line: &str, stats: &HwStats) -> serde_json::Value {
    let mut v = serde_json::to_value(stats).unwrap_or(serde_json::Value::Null);
    if let serde_json::Value::Object(ref mut map) = v {
        map.insert("line".to_string(), serde_json::json!(line));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::sink::{JSONL_NAME, TEXT_NAME};
    use super::*;
    use std::path::PathBuf;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::registry::Registry;

    fn run(tag: &str) -> (PathBuf, Arc<Diagnostics>) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("minihud-events-{tag}-{nanos}"));
        std::fs::create_dir_all(&base).unwrap();
        let d = Diagnostics::create(&base, "run-14_10_2026-18-30-00".to_string());
        (base, d)
    }

    fn records(d: &Diagnostics) -> Vec<serde_json::Value> {
        std::fs::read_to_string(d.dir().join(JSONL_NAME))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn sensor(hw: &str, ty: &str, name: &str, value: f64) -> LhmSensor {
        LhmSensor {
            hw: hw.to_string(),
            sensor_type: ty.to_string(),
            name: name.to_string(),
            value,
        }
    }

    /// Catches: the artifacts being written next to the exe without the
    /// `diagnostics` container (they would mix with the bridge assets and the
    /// exe itself), or into a folder that `cargo clean` does not reclaim.
    #[test]
    fn the_run_artifacts_live_in_a_diagnostics_folder_next_to_the_exe() {
        assert_eq!(
            run_root(std::path::Path::new(r"C:pp	arget\debug")),
            std::path::Path::new(r"C:pp	arget\debug\diagnostics")
        );
    }

    /// Catches: the per-poll dump going nowhere (or landing as text in
    /// `payload` instead of `sensor_values`), so the "complete sensor dump per
    /// poll" the flag promises cannot be read back out of the record.
    #[test]
    fn sensor_poll_persists_the_complete_dump() {
        let (base, d) = run("sensor");
        let sub = Registry::default().with(layer::DiagLayer::new(d.clone()));
        tracing::subscriber::with_default(sub, || {
            sensor_poll(
                &d,
                3,
                &[
                    sensor("Cpu:AMD", "Load", "CPU Total", 42.0),
                    sensor("GpuNvidia:RTX", "Load", "GPU Core", 80.0),
                ],
            )
        });

        let recs = records(&d);
        let poll = recs
            .iter()
            .find(|r| r["event_kind"] == "sensor_poll")
            .unwrap_or_else(|| panic!("no sensor_poll record in {recs:?}"));
        assert_eq!(poll["payload"]["sensor_poll.count"], 2);
        assert_eq!(poll["payload"]["sensor_poll.index"], 3);
        assert_eq!(poll["sensor_values"][0]["name"], "CPU Total");
        assert_eq!(poll["sensor_values"][1]["hw"], "GpuNvidia:RTX");
        let text = std::fs::read_to_string(d.dir().join(TEXT_NAME)).unwrap();
        assert!(text.contains("sensor poll #3 count=2"), "{text}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Catches: the rendered stats line not reaching the record (or dropping
    /// the line the developer saw), so the trace-level per-render stats the
    /// flag promises exist only on the console and not in the files.
    #[test]
    fn render_stat_persists_the_line_and_every_value() {
        let (base, d) = run("render");
        let stats = HwStats {
            cpu_percent: Some(26.0),
            gpu_temp_c: Some(38.0),
            ..HwStats::default()
        };
        let sub = Registry::default().with(layer::DiagLayer::new(d.clone()));
        tracing::subscriber::with_default(sub, || render_stat(&d, "CPU 26%", &stats));

        let recs = records(&d);
        let render = recs
            .iter()
            .find(|r| r["event_kind"] == "render_stat")
            .unwrap_or_else(|| panic!("no render_stat record in {recs:?}"));
        assert_eq!(render["render_stats"]["cpu_percent"], 26.0);
        assert_eq!(
            render["render_stats"]["line"], "CPU 26%",
            "the rendered line belongs in the record"
        );
        assert_eq!(render["payload"]["line"], "CPU 26%");
        let text = std::fs::read_to_string(d.dir().join(TEXT_NAME)).unwrap();
        assert!(text.contains("CPU 26%"), "{text}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Catches: instrumentation that keeps serializing every poll after the
    /// sink has gone away (a failed run), turning a diagnostics failure into a
    /// slowdown of the status loop.
    #[test]
    fn instrumentation_is_a_noop_before_the_run_opens() {
        let pending = Diagnostics::pending();
        let stats = HwStats {
            cpu_percent: Some(1.0),
            ..HwStats::default()
        };
        sensor_poll(&pending, 1, &[sensor("Cpu:AMD", "Load", "CPU Total", 1.0)]);
        render_stat(&pending, "CPU 1%", &stats);
        assert!(pending.dir().as_os_str().is_empty(), "no folder");
    }
}
