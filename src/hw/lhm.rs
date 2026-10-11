//! LibreHardwareMonitor sidecar feed: the SOLE hardware data source.
//!
//! LHM is .NET-only, so a persistent `powershell` host runs
//! `tools/lhm/lhm-bridge.ps1` and emits one JSON sensor dump per blank
//! line on stdin. This module owns that child on a dedicated thread:
//! the main loop NEVER blocks on it — `latest()` just reads the last
//! good sample, and rows read "--" until the first one lands.
//!
//! Mapping a sample onto [`super::HwStats`] lives in [`super::apply`].

use serde::Deserialize;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Seconds before retrying a failed bridge session.
const BRIDGE_RETRY_SECS: u64 = 5;
/// Default milliseconds between bridge polls (one JSON dump per tick). The
/// default of `1` reads continuously for the freshest possible values; the
/// bridge's own `Update()` read costs ~92 ms, so the sensor rate is ~10/s
/// regardless.
pub const BRIDGE_POLL_MS: u64 = 1;

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

#[derive(Clone)]
pub struct LhmFeed {
    latest: LhmLatest,
}

impl LhmFeed {
    /// Start the bridge ticking every `poll_ms` (clamped to ≥1 ms). A faster
    /// cadence lowers the latency from a sensor change to the status line; the
    /// caller is responsible for not pegging the machine.
    pub fn start_with_poll(poll_ms: u64) -> Self {
        let latest = Arc::new(Mutex::new(None));
        let slot = latest.clone();
        std::thread::spawn(move || Self::run(slot, poll_ms));
        Self { latest }
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

    fn run(slot: LhmLatest, poll_ms: u64) {
        loop {
            if let Err(e) = Self::session(&slot, poll_ms) {
                tracing::warn!("lhm bridge: {e}; retry in {BRIDGE_RETRY_SECS}s");
            }
            std::thread::sleep(Duration::from_secs(BRIDGE_RETRY_SECS));
        }
    }

    fn session(slot: &LhmLatest, poll_ms: u64) -> Result<(), String> {
        let mut child = spawn_bridge()?;
        let (mut out, mut stdin) = bridge_streams(&mut child)?;
        read_handshake(&mut out)?;
        tracing::info!("lhm bridge: serving");
        pump(&mut out, &mut stdin, slot, poll_ms)
    }
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

/// The sleep between bridge ticks (ms clamped to ≥1 so a `--bridge-ms 0` cannot
/// busy-spin the sidecar). Pure, so the clamp is unit-tested.
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
        let feed = LhmFeed { latest: slot };
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
        // Catches: a `--bridge-ms 0` busy-spinning the PowerShell sidecar (and
        // the LHM hardware reads) at full tilt, or a valid value being ignored.
        assert_eq!(bridge_delay(0), Duration::from_millis(1));
        assert_eq!(bridge_delay(100), Duration::from_millis(100));
        assert_eq!(bridge_delay(500), Duration::from_millis(500));
    }
}
