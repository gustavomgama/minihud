//! PresentMon sidecar feed: the tier-0 (ETW, no-injection) FPS data source.
//!
//! `PresentMon.exe` — staged next to the exe — opens an ETW session for one
//! process and writes one CSV line per present to stdout. A reader thread parses
//! each line leniently and pushes the frametime (ms between presents) into a
//! shared rolling [`FpsWindow`]. Nothing is injected.
//!
//! Graceful degradation (FpsOverlayer's lesson): if the child exits or errors —
//! most importantly when an anti-cheat blocks the ETW session — we log once,
//! serve `--`, and retry with a backoff. `PresentMonFeed::latest` never blocks
//! and never panics.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::window::{FpsSource, FpsWindow};

/// Seconds before retrying a failed PresentMon session.
const RETRY_SECS: u64 = 5;
/// A frame sample older than this reads as stale (`--`): no present for this
/// long means the target is idle or gone.
const SAMPLE_MAX_AGE: Duration = Duration::from_secs(2);
/// `CREATE_NO_WINDOW` — spawn PresentMon without a console window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// How long the retry backoff sleeps before re-checking the stop flag.
const BACKOFF_POLL_MS: u64 = 100;

/// Locate the frame-time column in a PresentMon CSV header line.
///
/// Prefers the exact `MsBetweenPresents`/`MsBetweenDisplayChange` columns
/// (case-insensitively, so v1 and v2 spellings both match), else the first
/// `msbetween*` column. `None` when the header has no timing field.
pub fn find_timing_column(header: &str) -> Option<usize> {
    let cols: Vec<String> = header
        .split(',')
        .map(|c| c.trim().to_ascii_lowercase())
        .collect();
    for candidate in ["msbetweenpresents", "msbetweendisplaychange"] {
        if let Some(i) = cols.iter().position(|c| c == candidate) {
            return Some(i);
        }
    }
    cols.iter().position(|c| c.starts_with("msbetween"))
}

/// Parse the frame-time field at `idx` from a CSV data line. Lenient: a short
/// line, an unparseable field, or surrounding whitespace yields `None`/the
/// number rather than an error.
pub fn parse_frame_ms(line: &str, idx: usize) -> Option<f64> {
    line.split(',').nth(idx)?.trim().parse::<f64>().ok()
}

/// The PresentMon arguments for capturing one process to stdout, tolerating a
/// pre-existing session (`-stop_existing_session`).
pub fn spawn_args(pid: u32) -> Vec<String> {
    vec![
        "-process_id".to_string(),
        pid.to_string(),
        "-output_stdout".to_string(),
        "-stop_existing_session".to_string(),
    ]
}

/// Resolve `PresentMon.exe` next to the running exe (staged by `cargo xtask`),
/// like `hw::lhm::paths`. A missing binary is a clear error; the feed then
/// serves `--` and retries.
fn presentmon_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let dir = exe.parent().ok_or("exe has no parent directory")?;
    let p = dir.join("PresentMon.exe");
    if p.exists() {
        Ok(p)
    } else {
        Err(format!(
            "PresentMon.exe not found next to the exe ({})",
            dir.display()
        ))
    }
}

/// Shared state: the rolling window plus the time of the newest sample (for the
/// staleness gate).
#[derive(Default)]
struct Shared {
    window: FpsWindow,
    updated: Option<Instant>,
}

/// The running child plus the stop flag, so `Drop` can kill PresentMon and
/// unblock the reader.
#[derive(Default)]
struct Control {
    stop: bool,
    child: Option<Child>,
}

/// An owned PresentMon capture for one pid. Dropping it stops the child (and the
/// supervisor thread).
pub struct PresentMonFeed {
    shared: Arc<Mutex<Shared>>,
    control: Arc<Mutex<Control>>,
}

impl PresentMonFeed {
    /// Start capturing `pid`. Spawns a supervisor thread that runs PresentMon,
    /// parses its stdout, and restarts it with a backoff on any failure. Never
    /// fails: a missing binary or a blocked ETW session just leaves the window
    /// empty (`--`).
    pub fn start(pid: u32) -> Self {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let control = Arc::new(Mutex::new(Control::default()));
        let (s, c) = (shared.clone(), control.clone());
        std::thread::spawn(move || supervise(&s, &c, pid));
        Self { shared, control }
    }

    /// Refresh the staleness gate: true when a sample arrived within
    /// [`SAMPLE_MAX_AGE`].
    fn fresh(&self) -> bool {
        self.shared
            .lock()
            .map(|s| s.updated.is_some_and(|t| t.elapsed() < SAMPLE_MAX_AGE))
            .unwrap_or(false)
    }
}

impl FpsSource for PresentMonFeed {
    fn fps(&self, window_ms: f64) -> Option<f64> {
        if !self.fresh() {
            return None;
        }
        self.shared.lock().ok()?.window.fps(window_ms)
    }

    fn one_percent_low(&self) -> Option<f64> {
        if !self.fresh() {
            return None;
        }
        self.shared.lock().ok()?.window.one_percent_low()
    }
}

impl Drop for PresentMonFeed {
    fn drop(&mut self) {
        // Stop the supervisor and kill the child so the blocked reader unblocks.
        let mut c = match self.control.lock() {
            Ok(c) => c,
            Err(e) => e.into_inner(),
        };
        c.stop = true;
        if let Some(mut child) = c.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Run sessions for `pid` until stopped, backing off between failures.
fn supervise(shared: &Arc<Mutex<Shared>>, control: &Arc<Mutex<Control>>, pid: u32) {
    let mut logged = false;
    while !is_stopped(control) {
        if let Err(e) = session(shared, control, pid) {
            // A dropped feed kills the child, which surfaces here as an EOF
            // error; that is a clean stop, not a failure to report.
            if is_stopped(control) {
                return;
            }
            if !logged {
                // Log the first failure loudly; anti-cheat blocking the ETW
                // session is the common cause, and it must stay visible.
                tracing::warn!("fps: presentmon: {e}; retry in {RETRY_SECS}s");
                logged = true;
            } else {
                tracing::debug!("fps: presentmon: {e}; retry in {RETRY_SECS}s");
            }
        }
        if !backoff(control) {
            return;
        }
    }
}

/// Sleep [`RETRY_SECS`], waking early when stopped. Returns false when stopped.
fn backoff(control: &Arc<Mutex<Control>>) -> bool {
    let deadline = Instant::now() + Duration::from_secs(RETRY_SECS);
    while Instant::now() < deadline {
        if is_stopped(control) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(BACKOFF_POLL_MS));
    }
    !is_stopped(control)
}

/// True when the feed has been dropped.
fn is_stopped(control: &Arc<Mutex<Control>>) -> bool {
    control.lock().map(|c| c.stop).unwrap_or(true)
}

/// One PresentMon session: spawn, store the child, read/parse until it exits.
fn session(
    shared: &Arc<Mutex<Shared>>,
    control: &Arc<Mutex<Control>>,
    pid: u32,
) -> Result<(), String> {
    let exe = presentmon_path()?;
    let mut child = spawn(&exe, pid)?;
    let stdout = child.stdout.take().ok_or("PresentMon has no stdout")?;
    {
        let mut c = control.lock().map_err(|_| "control poisoned".to_string())?;
        if c.stop {
            // Raced with Drop: kill this child and stop.
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
        c.child = Some(child);
    }
    let result = read_frames(&mut BufReader::new(stdout), shared);
    // Reap and clear the child slot (it may already be gone if Drop took it).
    if let Ok(mut c) = control.lock() {
        if let Some(mut ch) = c.child.take() {
            let _ = ch.wait();
        }
    }
    result
}

/// Spawn PresentMon for `pid` with stdout piped and no console window.
fn spawn(exe: &PathBuf, pid: u32) -> Result<Child, String> {
    let mut cmd = Command::new(exe);
    cmd.args(spawn_args(pid))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn()
        .map_err(|e| format!("spawn {}: {e}", exe.display()))
}

/// Read PresentMon's CSV: find the timing column from the header, then push one
/// frametime per data line into the shared window. Returns `Err` on EOF/read
/// error so the supervisor retries (or stops, if the feed was dropped).
fn read_frames(reader: &mut impl BufRead, shared: &Arc<Mutex<Shared>>) -> Result<(), String> {
    let mut line = String::new();
    // Header: the first line that names a timing column.
    let idx = loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("PresentMon exited before a header".to_string());
        }
        if let Some(i) = find_timing_column(&line) {
            break i;
        }
    };
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("PresentMon exited".to_string());
        }
        if let Some(ms) = parse_frame_ms(&line, idx) {
            if let Ok(mut s) = shared.lock() {
                s.window.record(ms);
                s.updated = Some(Instant::now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_timing_column_case_insensitively() {
        // Catches: PresentMon's v1 (`MsBetweenPresents`) vs v2
        // (`msBetweenPresents`) casing difference silently breaking the parse —
        // the feed would then never publish a frametime.
        let v1 = "Application,ProcessID,SwapChainAddress,MsBetweenPresents,MsInPresentAPI";
        assert_eq!(find_timing_column(v1), Some(3));
        let lower = "application,processId,msBetweenPresents";
        assert_eq!(find_timing_column(lower), Some(2));
        let display = "Application,ProcessID,MsBetweenDisplayChange";
        assert_eq!(find_timing_column(display), Some(2));
    }

    #[test]
    fn falls_back_to_any_msbetween_column() {
        // Catches: a PresentMon build whose timing column is not one of the two
        // known names (e.g. `MsBetweenSimulationStart`) yielding no FPS at all.
        let header = "Application,ProcessID,MsBetweenSimulationStart,MsInPresentAPI";
        assert_eq!(find_timing_column(header), Some(2));
    }

    #[test]
    fn a_header_without_a_timing_column_is_none() {
        assert_eq!(find_timing_column("Application,ProcessID,Runtime"), None);
    }

    #[test]
    fn parses_a_frametime_field_leniently() {
        // Catches: a stray space or a short/garbled line aborting the feed
        // instead of being skipped.
        let line = "game,1234,0xdead,16.667,0.42";
        assert_eq!(parse_frame_ms(line, 3), Some(16.667));
        assert_eq!(parse_frame_ms("a, 60.0 ", 1), Some(60.0));
        assert_eq!(parse_frame_ms("a,b", 5), None, "missing column is None");
        assert_eq!(parse_frame_ms("a,notnum", 1), None, "unparseable is None");
    }

    #[test]
    fn spawn_args_request_a_stdout_dump_for_the_pid() {
        // Catches: dropping `-output_stdout` (no CSV to parse) or
        // `-stop_existing_session` (a stale session blocks the capture), or
        // passing the wrong pid.
        assert_eq!(
            spawn_args(4321),
            vec![
                "-process_id",
                "4321",
                "-output_stdout",
                "-stop_existing_session"
            ]
        );
    }
}
