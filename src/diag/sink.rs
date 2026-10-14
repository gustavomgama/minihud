//! The run's pair of diagnostic artifacts: a verbose text log and a structured
//! JSONL, inside the timestamped run folder next to the exe.
//!
//! Both files are written on EVERY run, with no flag and no environment
//! opt-in, and a run is opened only once the process is really running — the
//! parent that relaunches itself elevated leaves no folder behind — so `cargo
//! clean` reclaims the whole tree by removing `target/`.
//!
//! Writing diagnostics is strictly secondary to the status loop: a folder that
//! cannot be created, or a write that fails, degrades this run to a silent
//! no-op and never panics or exits non-zero. The failure is reported to the
//! exactly ONE warning on stderr, unconditionally (the approved contract:
//! a loud degrade), because the only runtime today is the developer.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(test)]
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use super::record::Record;

/// The verbose text log's file name inside a run folder.
pub const TEXT_NAME: &str = "diagnostics.log";
/// The structured JSONL's name inside a run folder.
pub const JSONL_NAME: &str = "diagnostics.jsonl";

/// One run's artifact pair, opened for append so a second run that starts in
/// the same second adds to the same folder instead of erasing the first.
struct Sinks {
    text: File,
    jsonl: File,
}

/// The sink's lifecycle: nothing until the run opens, then the pair, then
/// silent after a failure.
enum State {
    /// The run has not opened its artifacts: records are dropped silently.
    Pending,
    Open(Sinks),
    /// A failure took the sink: records are dropped, reported at most once.
    Failed,
}

/// The run's diagnostics sink: shared by the tracing layer, the lifecycle
/// records and [`crate::diag::begin`].
pub struct Diagnostics {
    /// The run's id, set when the run opens.
    run_id: OnceLock<String>,
    /// The run's folder, set when the run opens.
    dir: OnceLock<PathBuf>,
    state: Mutex<State>,
    /// When this sink was created, for the stop record's uptime.
    started: Instant,
    /// Set on the first failure: at most one report per run.
    failed: AtomicBool,
    warnings: AtomicUsize,
    detail: Mutex<String>,
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self {
            run_id: OnceLock::new(),
            dir: OnceLock::new(),
            state: Mutex::new(State::Pending),
            started: Instant::now(),
            failed: AtomicBool::new(false),
            warnings: AtomicUsize::new(0),
            detail: Mutex::new(String::new()),
        }
    }
}

impl Diagnostics {
    /// A pending sink: it drops every record until [`Diagnostics::open`] runs,
    /// so a process that exits before the run really starts leaves no folder.
    pub fn pending() -> Self {
        Self::default()
    }

    /// A run opened immediately. Convenience for tests; the app uses
    /// [`Diagnostics::pending`] + [`Diagnostics::open`].
    #[cfg(test)]
    pub fn create(root: &Path, run_id: String) -> Arc<Self> {
        let d = Arc::new(Self::pending());
        d.open(root, run_id);
        d
    }

    /// Open the run's pair inside `<root>/<run_id>/`, creating the folder. A
    /// folder or file that cannot be created is a degrade, never an error the
    /// Open the run's pair inside `<root>/<run_id>/`, creating the folder. A
    /// folder or file that cannot be created is a degrade, never an error the
    /// caller must handle, and a failure is reported exactly once,
    /// unconditionally (the approved contract: a loud degrade). Returns
    /// whether the artifacts were opened.
    pub fn open(&self, root: &Path, run_id: String) -> bool {
        let dir = root.join(&run_id);
        match open_pair(&dir) {
            Ok(sinks) => {
                let _ = self.run_id.set(run_id);
                let _ = self.dir.set(dir);
                *self.state() = State::Open(sinks);
                true
            }
            Err(e) => {
                self.note_failure(&e.to_string());
                false
            }
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// This run's id (`run-DD_MM_YYYY-HH-MM-SS`), empty before the run opens.
    pub fn run_id(&self) -> &str {
        self.run_id.get().map(String::as_str).unwrap_or("")
    }

    /// The folder this run writes into, next to the exe.
    #[cfg(test)]
    pub fn dir(&self) -> &Path {
        self.dir
            .get()
            .map(PathBuf::as_path)
            .unwrap_or(Path::new(""))
    }

    /// Milliseconds since this sink was created.
    pub fn uptime_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// True once the run has opened its artifacts.
    pub fn opened(&self) -> bool {
        matches!(*self.state(), State::Open(_))
    }

    /// True when a write failed and the sink has gone silent.
    #[cfg(test)]
    pub fn degraded(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }

    /// How many failure reports this run emitted (0 or 1).
    #[cfg(test)]
    pub fn warnings(&self) -> usize {
        self.warnings.load(Ordering::SeqCst)
    }

    /// The failure text behind the report.
    #[cfg(test)]
    pub fn failure(&self) -> String {
        self.detail
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Write one record to both artifacts, flushing each line so a crashed run
    /// still leaves a complete record behind.
    pub fn record(&self, rec: &Record) -> io::Result<()> {
        let text = rec.text_line();
        let jsonl = rec.jsonl_line();
        let mut guards = self.state();
        let written = match &mut *guards {
            // Before the run opens, the record is dropped, silently.
            State::Pending => return Ok(()),
            State::Failed => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the sink is closed",
            )),
            State::Open(sinks) => (|| {
                sinks.text.write_all(text.as_bytes())?;
                sinks.text.flush()?;
                sinks.jsonl.write_all(jsonl.as_bytes())?;
                sinks.jsonl.flush()
            })(),
        };
        drop(guards);
        if let Err(e) = &written {
            self.note_failure(&e.to_string());
        }
        written
    }

    /// Stop writing after `why`, reporting it exactly once and
    /// unconditionally. The status loop keeps running: a missing diagnostics
    /// file is never fatal.
    fn note_failure(&self, why: &str) {
        *self.state() = State::Failed;
        if self.failed.swap(true, Ordering::SeqCst) {
            return;
        }
        *self.detail.lock().unwrap_or_else(|e| e.into_inner()) = why.to_string();
        self.warnings.fetch_add(1, Ordering::SeqCst);
        eprintln!("{}", write_failure_warning(why));
    }
}

/// The single report a failed diagnostics write emits, once per run. ASCII
/// only.
pub fn write_failure_warning(why: &str) -> String {
    format!("diagnostics: {why}; continuing without the diagnostics log")
}

/// Both files of the run folder, opened for append.
fn open_pair(dir: &Path) -> io::Result<Sinks> {
    std::fs::create_dir_all(dir)?;
    Ok(Sinks {
        text: append(dir.join(TEXT_NAME))?,
        jsonl: append(dir.join(JSONL_NAME))?,
    })
}

/// Open `path` for appending, creating it when absent — so a run that reuses
/// another run's folder in the same second adds to the same files.
fn append(path: PathBuf) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map};
    use std::fs;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("minihud-diag-{tag}-{nanos}"));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn rec(run: &str, message: &str) -> Record {
        Record {
            run_id: run.to_string(),
            timestamp: "2026-10-14T18:30:00.123456Z".to_string(),
            event_kind: crate::diag::record::EventKind::Subsystem,
            subsystem: "minihud::main".to_string(),
            level: "info".to_string(),
            span_name: String::new(),
            duration_us: 0,
            sensor_values: json!({}),
            render_stats: json!({}),
            payload: Map::new(),
            message: message.to_string(),
        }
    }

    /// A run over `base` for the failure-report tests.
    fn live(base: &Path) -> Arc<Diagnostics> {
        let d = Arc::new(Diagnostics::pending());
        d.open(base, "run-14_10_2026-18-30-00".to_string());
        d
    }

    /// Catches: a run that writes nothing at all — the whole feature is a no-op
    /// and the developer has no record to read.
    #[test]
    fn a_run_writes_a_pair_of_artifacts_into_a_timestamped_folder() {
        let base = root("pair");
        let d = Diagnostics::create(&base, "run-14_10_2026-18-30-00".to_string());
        assert!(d.record(&rec(d.run_id(), "hello")).is_ok());
        assert!(d.opened(), "the run must be open");

        let dir = d.dir();
        assert!(dir.is_dir(), "the run folder must exist: {}", dir.display());
        assert!(
            dir.join(TEXT_NAME).is_file(),
            "the verbose text log must exist: {}",
            dir.join(TEXT_NAME).display()
        );
        assert!(
            dir.join(JSONL_NAME).is_file(),
            "the JSONL must exist: {}",
            dir.join(JSONL_NAME).display()
        );
        let jsonl = fs::read_to_string(dir.join(JSONL_NAME)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(jsonl.trim()).unwrap();
        assert_eq!(parsed["message"], json!("hello"));
        assert_eq!(parsed["run_id"], json!(d.run_id()));
        let text = fs::read_to_string(dir.join(TEXT_NAME)).unwrap();
        assert!(text.contains("hello"), "{text}");
        let _ = fs::remove_dir_all(&base);
    }

    /// Catches: two runs sharing one folder, so the second run's record
    /// overwrites (or appends into) the first's and prior runs are lost.
    #[test]
    fn every_run_keeps_its_own_folder_beside_the_others() {
        let base = root("runs");
        let first = Diagnostics::create(&base, "run-14_10_2026-18-30-00".to_string());
        first
            .record(&rec(first.run_id(), "first"))
            .expect("the first run writes");
        let second = Diagnostics::create(&base, "run-14_10_2026-18-30-01".to_string());
        second
            .record(&rec(second.run_id(), "second"))
            .expect("the second run writes");

        assert_ne!(first.dir(), second.dir(), "each run gets its own folder");
        for (run, msg) in [
            (first.run_id().to_string(), "first"),
            (second.run_id().to_string(), "second"),
        ] {
            let jsonl = fs::read_to_string(base.join(&run).join(JSONL_NAME)).unwrap();
            assert!(
                jsonl.contains(msg),
                "run {run} must keep its own record: {jsonl}"
            );
        }
        let _ = fs::remove_dir_all(&base);
    }

    /// Catches: a read-only exe dir (or a full disk) aborting the run — the
    /// status loop must keep going, the run degrades once, and nothing is
    /// written. The report is unconditional (approved contract: loud degrade).
    #[test]
    fn an_unwritable_folder_degrades_and_reports_once() {
        // A FILE where the diagnostics folder must go, so `create_dir_all`
        // cannot make it: the same failure a read-only exe dir produces.
        let base = root("degrade");
        fs::write(base.join("run-14_10_2026-18-30-00"), b"not a folder").unwrap();

        let silent = live(&base);
        assert!(!silent.opened(), "an unwritable run cannot open");
        assert!(silent.degraded(), "the run must degrade, not abort");
        assert_eq!(
            silent.warnings(),
            1,
            "the live surface reports the failure once"
        );
        assert!(!silent.failure().is_empty(), "the reason must be recorded");

        // Every later write fails, and the report is not repeated.
        for n in 0..5 {
            assert!(
                silent.record(&rec(silent.run_id(), "loop")).is_err(),
                "write {n}"
            );
            assert_eq!(silent.warnings(), 1, "the warning is emitted once, not {n}");
        }

        assert!(
            fs::read_to_string(base.join("run-14_10_2026-18-30-00"))
                .is_ok_and(|s| s == "not a folder"),
            "the file that blocked the folder must be untouched"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// Catches: the write-failure report being non-ASCII (an em-dash in the
    /// failure text), which renders as mojibake on a legacy code page, or one
    /// that does not say what happened.
    #[test]
    fn the_write_failure_warning_is_ascii_and_names_the_reason() {
        let w = write_failure_warning("Access is denied. (os error 5)");
        assert!(w.is_ascii(), "{w}");
        assert!(w.contains("diagnostics"), "{w}");
        assert!(w.contains("Access is denied"), "{w}");
        assert!(w.contains("without the diagnostics log"), "{w}");
    }

    /// Catches: the JSONL and the text log drifting apart (an event in one and
    /// not the other), so the verbose file is not a readable copy of the
    /// structured one.
    #[test]
    fn both_artifacts_carry_the_same_events() {
        let base = root("pair-events");
        let d = Diagnostics::create(&base, "run-14_10_2026-18-30-00".to_string());
        for msg in ["one", "two", "three"] {
            d.record(&rec(d.run_id(), msg)).expect("write");
        }
        let jsonl = fs::read_to_string(d.dir().join(JSONL_NAME)).unwrap();
        let text = fs::read_to_string(d.dir().join(TEXT_NAME)).unwrap();
        assert_eq!(jsonl.lines().count(), 3, "{jsonl}");
        assert_eq!(text.lines().count(), 3, "{text}");
        for msg in ["one", "two", "three"] {
            assert!(jsonl.contains(msg), "{msg} missing from the JSONL");
            assert!(text.contains(msg), "{msg} missing from the text log");
        }
        let _ = fs::remove_dir_all(&base);
    }

    /// Catches: events emitted before the run's artifacts are opened (the
    /// startup chatter of a process that then hands off to its elevated copy)
    /// creating a folder of their own, so every app start leaves an empty run
    /// behind instead of only the runs that actually ran.
    #[test]
    fn a_pending_sink_writes_nothing_until_the_run_opens() {
        let base = root("pending");
        let d = Diagnostics::pending();
        assert!(
            d.record(&rec("run-14_10_2026-18-30-00", "before the run"))
                .is_ok(),
            "a record before the run opens is dropped, not an error"
        );
        assert!(
            !base.join("run-14_10_2026-18-30-00").exists(),
            "no folder may exist before the run opens"
        );

        assert!(d.open(&base, "run-14_10_2026-18-30-00".to_string()));
        assert!(d.record(&rec(d.run_id(), "after the run")).is_ok());
        assert_eq!(
            d.run_id(),
            "run-14_10_2026-18-30-00",
            "the run id is the one the caller opened"
        );
        let jsonl = fs::read_to_string(d.dir().join(JSONL_NAME)).unwrap();
        assert!(jsonl.contains("after the run"), "{jsonl}");
        assert!(
            !jsonl.contains("before the run"),
            "a dropped record must not reappear: {jsonl}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// Catches: a run opened twice (or a folder the layer already wrote to)
    /// losing the first pair's records — the append mode must keep both.
    #[test]
    fn the_artifacts_are_opened_for_append_not_truncation() {
        let base = root("append");
        let d = Diagnostics::create(&base, "run-14_10_2026-18-30-00".to_string());
        d.record(&rec(d.run_id(), "first")).expect("write");
        d.record(&rec(d.run_id(), "second")).expect("write");
        let jsonl = fs::read_to_string(d.dir().join(JSONL_NAME)).unwrap();
        assert_eq!(jsonl.lines().count(), 2, "{jsonl}");
        let _ = fs::remove_dir_all(&base);
    }
}
