//! Run lifecycle records: `process.start` (the first JSONL event of every run)
//! and `process.stop` (the last).
//!
//! Both are written directly to the sink rather than through the tracing
//! layer, so `process.start` lands before any tracing event can, and
//! `process.stop` still lands on the elevation-relaunch path, where `Drop`
//! never runs because `std::process::exit` skips it.

use std::path::Path;

use super::record::{EventKind, Record};
use super::sink::Diagnostics;
use super::time::iso_timestamp_now;

/// The `process.start` payload keys, persisted for post-run correlation.
const VERSION_KEY: &str = "version";

/// Write the run's first record: what this process is and how it was invoked.
/// ASCII-only payload (a non-ASCII argv entry is escaped as JSON, and the
/// message itself is literal ASCII).
pub fn record_start(sink: &Diagnostics, argv: &[String], exe: &Path, elevated: bool) {
    let mut payload = serde_json::Map::new();
    payload.insert(
        VERSION_KEY.to_string(),
        serde_json::Value::String(env!("CARGO_PKG_VERSION").to_string()),
    );
    payload.insert(
        "argv".to_string(),
        serde_json::Value::Array(
            argv.iter()
                .map(|a| serde_json::Value::String(a.clone()))
                .collect(),
        ),
    );
    payload.insert(
        "exe".to_string(),
        serde_json::Value::String(exe.to_string_lossy().to_string()),
    );
    payload.insert(
        "cwd".to_string(),
        serde_json::Value::String(
            std::env::current_dir()
                .map(|d| d.to_string_lossy().to_string())
                .unwrap_or_default(),
        ),
    );
    payload.insert(
        "pid".to_string(),
        serde_json::Value::Number(std::process::id().into()),
    );
    payload.insert("elevated".to_string(), serde_json::Value::Bool(elevated));
    write(sink, "process.start", payload);
}

/// Write the run's last record: why it ended and how long it ran.
pub fn record_stop(sink: &Diagnostics, reason: &str) {
    let mut payload = serde_json::Map::new();
    payload.insert(
        "reason".to_string(),
        serde_json::Value::String(reason.to_string()),
    );
    payload.insert(
        "uptime_ms".to_string(),
        serde_json::Value::Number(sink.uptime_ms().into()),
    );
    write(sink, "process.stop", payload);
}

/// Writes `process.stop` on drop unless already written. `Drop` does not run
/// on `std::process::exit`, so the elevation-relaunch path calls
/// [`record_stop`] explicitly before exiting.
pub struct StopGuard<'a> {
    sink: &'a Diagnostics,
    written: bool,
}

impl<'a> StopGuard<'a> {
    pub fn new(sink: &'a Diagnostics) -> Self {
        Self {
            sink,
            written: false,
        }
    }

    /// Write the stop record now (the exit path) instead of on drop.
    pub fn stop(&mut self, reason: &str) {
        if !self.written {
            record_stop(self.sink, reason);
            self.written = true;
        }
    }
}

impl Drop for StopGuard<'_> {
    fn drop(&mut self) {
        self.stop("exit");
    }
}

fn write(sink: &Diagnostics, message: &str, payload: serde_json::Map<String, serde_json::Value>) {
    let rec = Record {
        run_id: sink.run_id().to_string(),
        timestamp: iso_timestamp_now(),
        event_kind: EventKind::Subsystem,
        subsystem: "minihud::main".to_string(),
        level: "info".to_string(),
        span_name: String::new(),
        duration_us: 0,
        sensor_values: serde_json::json!({}),
        render_stats: serde_json::json!({}),
        payload,
        message: message.to_string(),
    };
    let _ = sink.record(&rec);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diag::sink::{JSONL_NAME, TEXT_NAME};
    use serde_json::{json, Value};
    use std::fs;

    fn root(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("minihud-lifecycle-{tag}-{nanos}"));
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Catches: a tracing event landing before the lifecycle record, so the
    /// JSONL no longer starts with `process.start` and a run cannot be tied
    /// to its invocation (version, argv, elevation).
    #[test]
    fn the_first_record_of_a_run_is_process_start() {
        let base = root("start");
        let sink = Diagnostics::create(&base, "run-14_10_2026-18-30-00".to_string());
        record_start(
            &sink,
            &["minihud".to_string(), "--stats".to_string()],
            Path::new(r"C:\Users\helmet\dev\minihud\target\debug\minihud.exe"),
            true,
        );

        let jsonl = fs::read_to_string(sink.dir().join(JSONL_NAME)).unwrap();
        let first: Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
        assert_eq!(first["message"], json!("process.start"));
        assert_eq!(
            first["payload"]["version"],
            json!(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(first["payload"]["elevated"], json!(true));
        assert!(first["payload"]["pid"].is_number(), "{first}");
        let text = fs::read_to_string(sink.dir().join(TEXT_NAME)).unwrap();
        assert!(text.contains("process.start"), "{text}");
        let _ = fs::remove_dir_all(&base);
    }

    /// Catches: a run that ends without a stop record, so uptime and the exit
    /// reason are unknowable from the artifacts.
    #[test]
    fn a_stop_record_carries_the_reason_and_uptime() {
        let base = root("stop");
        let sink = Diagnostics::create(&base, "run-14_10_2026-18-30-00".to_string());
        record_start(&sink, &["minihud".to_string()], Path::new("x"), false);
        let mut guard = StopGuard::new(&sink);
        guard.stop("elevation-relaunch");

        let jsonl = fs::read_to_string(sink.dir().join(JSONL_NAME)).unwrap();
        let stop: Value = serde_json::from_str(jsonl.lines().last().unwrap()).unwrap();
        assert_eq!(stop["message"], json!("process.stop"));
        assert_eq!(stop["payload"]["reason"], json!("elevation-relaunch"));
        assert!(stop["payload"]["uptime_ms"].is_number(), "{stop}");
        let _ = fs::remove_dir_all(&base);
    }

    /// Catches: the guard writing stop twice (explicit stop then Drop), which
    /// would put two stop records at the end of the file.
    #[test]
    fn the_guard_writes_stop_once() {
        let base = root("once");
        let sink = Diagnostics::create(&base, "run-14_10_2026-18-30-00".to_string());
        {
            let mut guard = StopGuard::new(&sink);
            guard.stop("explicit");
        }
        let jsonl = fs::read_to_string(sink.dir().join(JSONL_NAME)).unwrap();
        assert_eq!(
            jsonl.lines().count(),
            1,
            "only the explicit stop record: {jsonl}"
        );
        let _ = fs::remove_dir_all(&base);
    }
}
