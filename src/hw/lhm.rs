//! LibreHardwareMonitor sidecar feed: the SOLE hardware data source.
//!
//! LHM is .NET-only, so a persistent `powershell` host runs
//! `tools/lhm/lhm-bridge.ps1` and emits one JSON sensor dump per blank
//! line on stdin. This module owns that child on a dedicated thread:
//! the main loop NEVER blocks on it — `latest()` just reads the last
//! good sample, and rows read "--" until the first one lands.
//!
//! Dropping the feed stops the supervisor and kills the child, which closes its
//! stdout pipe and unblocks the reader (the same discipline as
//! `fps::presentmon::PresentMonFeed`), so no orphan PowerShell survives.
//!
//! Mapping a sample onto [`super::HwStats`] lives in [`super::apply`].

use serde::Deserialize;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::retry;

/// Fixed backoff before retrying a failed bridge session.
const BRIDGE_RETRY_SECS: u64 = 5;
/// Largest jitter applied to the backoff (so feeds do not retry in lockstep).
const MAX_JITTER: Duration = Duration::from_millis(retry::MAX_JITTER_MS);
/// How long the backoff sleeps before re-checking the stop flag.
const BACKOFF_POLL_MS: u64 = 100;

/// One sensor reading from the bridge.
#[derive(Clone, Debug, Deserialize)]
pub struct LhmSensor {
    /// e.g. "GpuNvidia:NVIDIA GeForce RTX 3070".
    pub hw: String,
    /// e.g. "Temperature", "Load", "Power", "Clock", "SmallData".
    #[serde(rename = "type")]
    pub sensor_type: String,
    /// e.g. "GPU Core", "CPU Total", "Package".
    pub name: String,
    pub value: f64,
}

/// Shared latest-sample slot: `(timestamp, sensors)` behind a mutex.
type LhmLatest = Arc<Mutex<Option<(Instant, Vec<LhmSensor>)>>>;

/// The running child plus the stop flag, so `Drop` can kill PowerShell and
/// unblock the reader.
#[derive(Default)]
struct Control {
    stop: bool,
    child: Option<Child>,
}

pub struct LhmFeed {
    latest: LhmLatest,
    control: Arc<Mutex<Control>>,
}

impl LhmFeed {
    /// Start the bridge ticking every `poll_ms` (clamped to ≥1 ms). A faster
    /// cadence lowers the latency from a sensor change to the status line; the
    /// caller is responsible for not pegging the machine.
    pub fn start_with_poll(poll_ms: u64) -> Self {
        let latest = Arc::new(Mutex::new(None));
        let control = Arc::new(Mutex::new(Control::default()));
        let (slot, ctl) = (latest.clone(), control.clone());
        std::thread::spawn(move || Self::run(slot, ctl, poll_ms));
        Self { latest, control }
    }

    /// Last sample if younger than `max_age`, else None (rows read
    /// "--"). Never blocks, never fails.
    pub fn latest(&self, max_age: Duration) -> Option<Vec<LhmSensor>> {
        let g = self.latest.lock().ok()?;
        let (t, v) = g.as_ref()?;
        if t.elapsed() < max_age {
            Some(v.clone())
        } else {
            None
        }
    }

    /// Run sessions until stopped, backing off (with jitter) between failures.
    fn run(slot: LhmLatest, control: Arc<Mutex<Control>>, poll_ms: u64) {
        let mut failures: u64 = 0;
        let mut jitter = retry::Jitter::new(retry::seed_now(std::process::id()));
        while !is_stopped(&control) {
            let mut wait = Duration::from_secs(BRIDGE_RETRY_SECS);
            if let Err(e) = Self::session(&slot, &control, poll_ms) {
                // Drop kills the child, which surfaces here as a pipe EOF; that
                // is a clean stop, not a failure to report.
                if is_stopped(&control) {
                    return;
                }
                failures += 1;
                let total = retry::HW_RETRIES.bump();
                wait = jitter.next(Duration::from_secs(BRIDGE_RETRY_SECS), MAX_JITTER);
                retry::emit_failure("lhm bridge", &e, wait, failures, total);
            }
            if !backoff(&control, wait) {
                return;
            }
        }
    }

    fn session(
        slot: &LhmLatest,
        control: &Arc<Mutex<Control>>,
        poll_ms: u64,
    ) -> Result<(), String> {
        let (mut out, mut stdin, child) = start_bridge()?;
        if !install_child(control, child)? {
            return Ok(()); // raced with Drop
        }
        let result = pump_session(&mut out, &mut stdin, slot, poll_ms);
        reap_child(control);
        result
    }
}

impl Drop for LhmFeed {
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

/// True when the feed has been dropped.
fn is_stopped(control: &Arc<Mutex<Control>>) -> bool {
    control.lock().map(|c| c.stop).unwrap_or(true)
}

/// Sleep `duration`, waking early when stopped. Returns false when stopped.
fn backoff(control: &Arc<Mutex<Control>>, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if is_stopped(control) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(BACKOFF_POLL_MS));
    }
    !is_stopped(control)
}

/// Spawn the PowerShell bridge host with the DLL path as an argument.
fn spawn_bridge() -> Result<Child, String> {
    let (dll, script) = paths().ok_or_else(|| {
        "LibreHardwareMonitorLib.dll / lhm-bridge.ps1 not found (see README)".to_string()
    })?;
    Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            &script.to_string_lossy(),
            "-DllPath",
            &dll.to_string_lossy(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("spawn powershell: {e}"))
}

/// Take the bridge child's stdout / stdin pipes.
fn bridge_streams(child: &mut Child) -> Result<(BufReader<ChildStdout>, ChildStdin), String> {
    let out = BufReader::new(child.stdout.take().ok_or("no stdout")?);
    let stdin = child.stdin.take().ok_or("no stdin")?;
    Ok((out, stdin))
}

/// Spawn the bridge and take its pipes: `(stdout reader, stdin writer, child)`.
fn start_bridge() -> Result<(BufReader<ChildStdout>, ChildStdin, Child), String> {
    let mut child = spawn_bridge()?;
    let (out, stdin) = bridge_streams(&mut child)?;
    Ok((out, stdin, child))
}

/// Store the spawned child on the control, unless the feed was stopped meanwhile
/// (then kill it and return false so the session ends without serving).
fn install_child(control: &Arc<Mutex<Control>>, mut child: Child) -> Result<bool, String> {
    let mut c = control.lock().map_err(|_| "control poisoned".to_string())?;
    if c.stop {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(false);
    }
    c.child = Some(child);
    Ok(true)
}

/// Handshake, then serve sensor dumps until the child exits.
fn pump_session(
    out: &mut impl BufRead,
    stdin: &mut impl Write,
    slot: &LhmLatest,
    poll_ms: u64,
) -> Result<(), String> {
    read_handshake(out)?;
    tracing::info!("lhm bridge: serving");
    pump(out, stdin, slot, poll_ms)
}

/// Reap and clear the child slot (it may already be gone if Drop took it).
fn reap_child(control: &Arc<Mutex<Control>>) {
    if let Ok(mut c) = control.lock() {
        if let Some(mut ch) = c.child.take() {
            let _ = ch.wait();
        }
    }
}

/// Read the bridge's two hello lines: a version banner, then `READY`.
fn read_handshake(out: &mut impl BufRead) -> Result<(), String> {
    let mut line = String::new();
    out.read_line(&mut line)
        .map_err(|e| format!("bridge hello: {e}"))?;
    tracing::info!("lhm bridge: {}", line.trim());
    line.clear();
    out.read_line(&mut line)
        .map_err(|e| format!("bridge ready: {e}"))?;
    if line.trim() != "READY" {
        return Err(format!("bridge unexpected hello: {}", line.trim()));
    }
    Ok(())
}

/// Tick the bridge: send a newline, read one JSON line, store it. Returns the
/// bytes read so the caller can detect a closed stdout (0 = EOF).
fn pump(
    out: &mut impl BufRead,
    stdin: &mut impl Write,
    slot: &LhmLatest,
    poll_ms: u64,
) -> Result<(), String> {
    let mut line = String::new();
    loop {
        let n = tick(out, stdin, &mut line, slot)?;
        if n == 0 {
            return Err("bridge closed stdout".to_string());
        }
        std::thread::sleep(bridge_delay(poll_ms));
    }
}

/// The sleep between bridge ticks (ms clamped to ≥1 so a `--hardware-poll 0`
/// cannot busy-spin the sidecar). The tick comes from the hardware poll value.
/// Pure, so the clamp is unit-tested.
pub(crate) fn bridge_delay(poll_ms: u64) -> Duration {
    Duration::from_millis(poll_ms.max(1))
}

fn tick(
    out: &mut impl BufRead,
    stdin: &mut impl Write,
    line: &mut String,
    slot: &LhmLatest,
) -> Result<usize, String> {
    stdin
        .write_all(b"\n")
        .map_err(|e| format!("bridge write: {e}"))?;
    stdin.flush().map_err(|e| format!("bridge flush: {e}"))?;
    line.clear();
    let n = out
        .read_line(line)
        .map_err(|e| format!("bridge read: {e}"))?;
    if n > 0 {
        store_sample(line, slot);
    }
    Ok(n)
}

/// Parse one JSON sensor dump and publish it; a bad line is logged, not fatal.
fn store_sample(line: &str, slot: &LhmLatest) {
    match serde_json::from_str::<Vec<LhmSensor>>(line) {
        Ok(sensors) => {
            tracing::trace!("lhm sensors: count={}", sensors.len());
            if let Ok(mut g) = slot.lock() {
                *g = Some((Instant::now(), sensors));
            }
        }
        Err(e) => tracing::debug!("lhm bridge: bad JSON line ({e})"),
    }
}

/// DLL + script live next to the exe in every profile. `cargo xtask build`
/// stages them there; there is no build-time / manifest-dir fallback, so
/// debug and release resolve assets identically.
fn paths() -> Option<(PathBuf, PathBuf)> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.to_path_buf();
    let find = |name: &str| -> Option<PathBuf> {
        let p = dir.join(name);
        p.exists().then_some(p)
    };
    Some((
        find("LibreHardwareMonitorLib.dll")?,
        find("lhm-bridge.ps1")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_slot() -> LhmLatest {
        Arc::new(Mutex::new(None))
    }

    /// A feed handle over `latest` with a fresh, empty control.
    fn feed(latest: LhmLatest) -> LhmFeed {
        LhmFeed {
            latest,
            control: Arc::new(Mutex::new(Control::default())),
        }
    }

    /// A real, silent, long-lived child: it writes no output and keeps its
    /// stdout pipe open, so a reader blocks on it — the "hung child" case.
    fn silent_long_lived_child() -> Child {
        Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 30",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a silent powershell")
    }

    const ONE_SENSOR: &str = r#"[{"hw":"Cpu:X","type":"Load","name":"CPU Total","value":42.0}]"#;

    #[test]
    fn read_handshake_requires_a_ready_second_line() {
        // Catches: accepting a bridge whose second hello line is not READY — the
        // host would then parse protocol noise as sensor JSON.
        let mut good = std::io::Cursor::new(b"lhm-bridge 0.9.4\nREADY\n".to_vec());
        assert!(read_handshake(&mut good).is_ok());

        let mut bad = std::io::Cursor::new(b"lhm-bridge 0.9.4\nNOPE\n".to_vec());
        assert!(
            read_handshake(&mut bad).is_err(),
            "a non-READY hello must be rejected"
        );
    }

    #[test]
    fn tick_writes_a_tick_and_publishes_the_parsed_sample() {
        // Catches: a bridge tick that reads a line but never publishes it, so
        // the poller always sees a stale/absent feed.
        let slot = empty_slot();
        let mut out = std::io::Cursor::new(format!("{ONE_SENSOR}\n").into_bytes());
        let mut stdin: Vec<u8> = Vec::new();
        let mut line = String::new();

        let n = tick(&mut out, &mut stdin, &mut line, &slot).expect("tick");
        assert!(n > 0);
        assert_eq!(stdin, b"\n", "each tick must send a newline to the bridge");
        let g = slot.lock().expect("slot");
        let (_, sensors) = g.as_ref().expect("a stored sample");
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].value, 42.0);
    }

    #[test]
    fn store_sample_ignores_bad_json_without_clobbering_a_good_sample() {
        // Catches: a single malformed bridge line wiping the last good sample,
        // flipping every row to `--` on a transient parse error.
        let slot = empty_slot();
        store_sample(ONE_SENSOR, &slot);
        assert!(slot.lock().expect("slot").is_some(), "good line stored");
        store_sample("not json at all", &slot);
        assert!(
            slot.lock().expect("slot").is_some(),
            "a bad line must not clear the slot"
        );
    }

    #[test]
    fn latest_returns_none_once_the_sample_goes_stale() {
        // Catches: a feed that ignores `max_age` and serves an old sample
        // forever, so a dead bridge would never show `--`.
        let slot = empty_slot();
        store_sample(ONE_SENSOR, &slot);
        let feed = feed(slot);
        assert!(
            feed.latest(Duration::from_secs(60)).is_some(),
            "a fresh sample is returned"
        );
        assert!(
            feed.latest(Duration::from_secs(0)).is_none(),
            "a zero-age window means the sample is already stale"
        );
    }

    #[test]
    fn bridge_delay_clamps_zero_and_keeps_a_normal_cadence() {
        // Catches: a `--hardware-poll 0` busy-spinning the PowerShell sidecar
        // (and the LHM hardware reads) at full tilt, or a valid value being
        // ignored.
        assert_eq!(bridge_delay(0), Duration::from_millis(1));
        assert_eq!(bridge_delay(100), Duration::from_millis(100));
        assert_eq!(bridge_delay(500), Duration::from_millis(500));
    }

    #[test]
    fn dropping_the_feed_kills_the_child_and_unblocks_the_reader() {
        // Catches: a persistent PowerShell child surviving minihud (an orphan)
        // and a reader parked forever on its pipe — the exact resource-leak and
        // infinite-wait failure this hardening fixes.
        let mut child = silent_long_lived_child();
        let mut stdout = child.stdout.take().expect("stdout");
        let control = Arc::new(Mutex::new(Control {
            stop: false,
            child: Some(child),
        }));
        let feed = LhmFeed {
            latest: empty_slot(),
            control: control.clone(),
        };

        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 1];
            let n = std::io::Read::read(&mut stdout, &mut buf).unwrap_or(0);
            let _ = tx.send(n);
        });

        drop(feed);

        let n = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the kill must close the pipe and unblock the reader");
        assert_eq!(n, 0, "a closed stdout must read as EOF");
        reader.join().expect("reader thread");
        assert!(is_stopped(&control), "drop must set the stop flag");
        assert!(
            control.lock().expect("control").child.is_none(),
            "drop must reap the child out of the slot"
        );
    }
}
