//! Console output coordinator.
//!
//! Keeps one **self-updating status line** at the bottom of the console — the
//! hardware stats, and the capture fps while capturing — with all event/log
//! output scrolling above it. Every writer goes through here: the hardware
//! poller, the capture loop, and `tracing` events (via [`LogMakeWriter`]). This
//! is what keeps a log line from corrupting the live status line, and it is why
//! the hardware line never repeats — it updates in place.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// The two status segments plus the redraw bookkeeping.
#[derive(Default)]
struct Out {
    /// Hardware-stats segment (left).
    hw: String,
    /// Capture segment (right), e.g. the current fps line.
    capture: String,
    /// Visible width of the last drawn status line, so a shorter line can be
    /// padded to erase the tail of the longer one.
    prev_len: usize,
    /// Whether a status line is currently on screen.
    drawn: bool,
    /// The exact text currently on screen. A `set()` whose composed line equals
    /// this writes nothing — the redraw is skipped.
    last: String,
    /// Actual hardware-segment changes (for the `--stats` rate readout).
    hw_updates: u64,
    /// Actual capture-segment changes (for the `--stats` rate readout).
    fps_updates: u64,
    /// Capture-segment redraw attempts (the fps tick), so the `--stats` readout
    /// can show the cadence even while the number is steady.
    fps_ticks: u64,
}

static OUT: OnceLock<Mutex<Out>> = OnceLock::new();

fn out() -> &'static Mutex<Out> {
    OUT.get_or_init(|| Mutex::new(Out::default()))
}

impl Out {
    /// Compose the current line and return the bytes that redraw it in place, or
    /// `None` when the composed line is identical to the one already on screen
    /// (nothing to do). Pure bookkeeping, so the "redraw only on change" rule is
    /// unit-tested without touching stdout.
    fn render(&mut self, max: usize) -> Option<String> {
        let line = compose(&self.hw, &self.capture, max);
        if line == self.last {
            return None;
        }
        let bytes = redraw_bytes(self.prev_len, &line);
        self.prev_len = line.chars().count();
        self.drawn = !line.is_empty();
        self.last = line;
        Some(bytes)
    }
}

/// Record a new segment value: true when it actually changed (so the caller can
/// bump the matching update counter). Pure, so the counter rule is testable.
fn note_change(seg: &mut String, new: String) -> bool {
    if *seg == new {
        return false;
    }
    *seg = new;
    true
}

fn lock() -> std::sync::MutexGuard<'static, Out> {
    out().lock().unwrap_or_else(|e| e.into_inner())
}

/// The composed status line: hardware stats and, while capturing, the fps line.
///
/// `max` is the visible console width (0 = unknown). The line is kept within it
/// so it never **wraps** — a wrapped status line breaks the in-place `\r`
/// redraw and garbles the console. The hardware segment is preserved; the
/// capture segment is truncated to fit.
fn compose(hw: &str, capture: &str, max: usize) -> String {
    const SEP: &str = "  |  ";
    let full = match (hw.is_empty(), capture.is_empty()) {
        (true, true) => return String::new(),
        (false, true) => hw.to_string(),
        (true, false) => capture.to_string(),
        (false, false) => format!("{hw}{SEP}{capture}"),
    };
    if max == 0 || full.chars().count() <= max {
        return full;
    }
    if hw.is_empty() {
        return truncate(capture, max);
    }
    let budget = max.saturating_sub(hw.chars().count() + SEP.chars().count());
    if capture.is_empty() || budget == 0 {
        return truncate(hw, max);
    }
    format!("{hw}{SEP}{}", truncate(capture, budget))
}

/// Truncate `s` to at most `max` characters.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// The visible console width in columns, or 0 when unknown (not a console).
fn console_width() -> usize {
    #[cfg(windows)]
    {
        use windows::Win32::System::Console::{
            GetConsoleScreenBufferInfo, GetStdHandle, CONSOLE_SCREEN_BUFFER_INFO, STD_OUTPUT_HANDLE,
        };
        // SAFETY: best-effort console query; any failure means "unknown".
        unsafe {
            let Ok(h) = GetStdHandle(STD_OUTPUT_HANDLE) else {
                return 0;
            };
            let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
            if GetConsoleScreenBufferInfo(h, &mut info).is_err() {
                return 0;
            }
            let w = info.srWindow.Right - info.srWindow.Left + 1;
            if w > 0 {
                // Leave the last column empty so the line cannot auto-wrap.
                (w as usize).saturating_sub(1)
            } else {
                0
            }
        }
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// Bytes that redraw the status line in place: `\r`, the line, then trailing
/// spaces to erase any longer previous line.
fn redraw_bytes(prev_len: usize, line: &str) -> String {
    let n = line.chars().count();
    let mut s = String::with_capacity(1 + line.len() + prev_len.saturating_sub(n));
    s.push('\r');
    s.push_str(line);
    for _ in n..prev_len {
        s.push(' ');
    }
    s
}

/// Bytes that scroll `line` above the status: erase the status line, print the
/// line, then redraw the status beneath it.
fn scroll_bytes(prev_len: usize, drawn: bool, line: &str, status: &str) -> String {
    let mut s = String::new();
    if drawn {
        s.push('\r');
        for _ in 0..prev_len {
            s.push(' ');
        }
        s.push('\r');
    }
    s.push_str(line);
    s.push('\n');
    if !status.is_empty() {
        s.push_str(status);
        let n = status.chars().count();
        for _ in n..prev_len {
            s.push(' ');
        }
    }
    s
}

/// Set the hardware-stats segment of the status line (redrawn in place).
pub fn status_hw(line: impl Into<String>) {
    let mut o = lock();
    if note_change(&mut o.hw, line.into()) {
        o.hw_updates += 1;
    }
    redraw(&mut o);
}

/// Set the capture segment of the status line (e.g. the fps line).
pub fn status_capture(line: impl Into<String>) {
    let mut o = lock();
    o.fps_ticks += 1;
    if note_change(&mut o.capture, line.into()) {
        o.fps_updates += 1;
    }
    redraw(&mut o);
}

/// Redraw the status line in place, but only when its composed content changed.
fn redraw(o: &mut Out) {
    if let Some(bytes) = o.render(console_width()) {
        write_stdout(&bytes);
    }
}

/// The status-update counts since start: `(hardware changes, fps changes, fps
/// ticks)`. Used by the `--stats` rate readout so "as fast as possible" is a
/// measured number.
pub fn counters() -> (u64, u64, u64) {
    let o = lock();
    (o.hw_updates, o.fps_updates, o.fps_ticks)
}

/// Emit a scrolling log line above the status line.
pub fn log(line: impl AsRef<str>) {
    let mut o = lock();
    let status = compose(&o.hw, &o.capture, console_width());
    write_stdout(&scroll_bytes(o.prev_len, o.drawn, line.as_ref(), &status));
    o.prev_len = status.chars().count();
    o.drawn = !status.is_empty();
    o.last = status;
}

/// Move off the status line so a shell prompt does not overwrite it.
pub fn finish() {
    let mut o = lock();
    if o.drawn {
        write_stdout("\n");
        o.drawn = false;
        o.prev_len = 0;
        o.last.clear();
    }
}

fn write_stdout(s: &str) {
    let mut h = std::io::stdout().lock();
    let _ = h.write_all(s.as_bytes());
    let _ = h.flush();
}

/// `HH:MM:SS` (UTC) for a Unix timestamp — a compact log timer.
pub fn hms(epoch_secs: u64) -> String {
    let s = epoch_secs % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// A `tracing` time formatter printing [`hms`].
pub struct CompactTime;

impl tracing_subscriber::fmt::time::FormatTime for CompactTime {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        write!(w, "{}", hms(secs))
    }
}

/// Enable ANSI/VT processing on the console; returns whether it is available
/// (false when output is redirected or the console is too old, so the caller
/// can disable colour).
pub fn enable_vt() -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE,
            ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
        };
        // SAFETY: standard console handle; every step is best-effort.
        unsafe {
            let Ok(h) = GetStdHandle(STD_OUTPUT_HANDLE) else {
                return false;
            };
            let mut mode = CONSOLE_MODE(0);
            if GetConsoleMode(h, &mut mode).is_err() {
                return false;
            }
            SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING).is_ok()
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// A `tracing` writer that routes each formatted event through [`log`].
#[derive(Clone, Copy, Default)]
pub struct LogMakeWriter;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogMakeWriter {
    type Writer = LogWriter;
    fn make_writer(&'a self) -> LogWriter {
        LogWriter::default()
    }
}

/// Buffers one `tracing` event and emits it as a single [`log`] line on drop.
#[derive(Default)]
pub struct LogWriter {
    buf: Vec<u8>,
}

impl Write for LogWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for LogWriter {
    fn drop(&mut self) {
        let s = String::from_utf8_lossy(&self.buf);
        let s = s.trim_end_matches(['\r', '\n']);
        if !s.is_empty() {
            log(s);
        }
    }
}

/// Cleared by the console Ctrl+C handler so the watch loops exit cleanly
/// (flushing a final status line) instead of being killed. A run with no
/// `--follow-secs` bound runs until this is cleared.
static RUNNING: AtomicBool = AtomicBool::new(true);

/// Console control handler: ask the running loops to stop.
#[cfg(windows)]
unsafe extern "system" fn ctrl_handler(_ctrl_type: u32) -> windows::core::BOOL {
    RUNNING.store(false, Ordering::SeqCst);
    windows::core::BOOL(1)
}

/// Register the Ctrl+C handler; failure is ignored (the loops still end on
/// their own bound or when the process is killed).
pub fn install_ctrl_handler() {
    #[cfg(windows)]
    {
        use windows::Win32::System::Console::SetConsoleCtrlHandler;
        // SAFETY: registering a process-global handler with a valid signature.
        let _ = unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), true) };
    }
}

/// True until Ctrl+C / window close was requested.
pub fn should_run() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_joins_the_two_segments() {
        assert_eq!(compose("", "", 0), "");
        assert_eq!(compose("CPU 1%", "", 0), "CPU 1%");
        assert_eq!(compose("", "60 fps", 0), "60 fps");
        assert_eq!(compose("CPU 1%", "60 fps", 0), "CPU 1%  |  60 fps");
    }

    #[test]
    fn compose_keeps_the_line_within_the_console_width() {
        // Catches: a status line wider than the console, which wraps and breaks
        // the in-place redraw (the whole console garbles). The hardware segment
        // is preserved; the capture segment is cut to fit.
        let hw = "CPU 1%";
        let cap = "1234567890";
        assert_eq!(compose(hw, cap, 100), "CPU 1%  |  1234567890");
        assert_eq!(compose(hw, cap, 12), "CPU 1%  |  1");
        assert_eq!(compose(hw, cap, 3), "CPU");
        assert_eq!(truncate("abcdef", 3), "abc");
        assert_eq!(truncate("ab", 3), "ab");
    }

    #[test]
    fn redraw_bytes_return_carriage_and_pad_a_shorter_line() {
        // Catches: a shorter status leaving the tail of the previous (longer)
        // line on screen.
        assert_eq!(redraw_bytes(0, "abc"), "\rabc");
        assert_eq!(redraw_bytes(5, "ab"), "\rab   ");
        assert_eq!(redraw_bytes(2, "abcd"), "\rabcd");
    }

    #[test]
    fn scroll_bytes_erase_the_status_then_restore_it_below() {
        // Catches: a log line overwriting the live status, or the status not
        // being restored under the log.
        assert_eq!(scroll_bytes(0, false, "hello", ""), "hello\n");
        assert_eq!(scroll_bytes(3, true, "hi", "abc"), "\r   \rhi\nabc");
        // The status is padded so a shorter one clears the old tail.
        assert_eq!(scroll_bytes(5, true, "x", "ab"), "\r     \rx\nab   ");
    }

    #[test]
    fn render_writes_once_and_skips_an_unchanged_line() {
        // Catches: the status line being rewritten and re-flushed on every
        // `set()` even when its content is identical — the display path's
        // dominant cost (one write+flush per hardware poll and per fps tick,
        // most of which change no character at all).
        let mut o = Out {
            hw: "CPU 1%".to_string(),
            ..Out::default()
        };
        let first = o.render(100).expect("the first render must write");
        assert!(first.contains("CPU 1%"), "{first}");
        assert_eq!(
            o.render(100),
            None,
            "an identical line must not be rewritten"
        );
        o.capture = "60 fps".to_string();
        let changed = o.render(100).expect("a changed line must write");
        assert!(changed.contains("60 fps"), "{changed}");
    }

    #[test]
    fn render_erases_the_line_when_both_segments_clear() {
        // Catches: clearing the status (target gone) leaving the old text on
        // screen because an empty line is treated as "unchanged".
        let mut o = Out {
            hw: "CPU 1%".to_string(),
            ..Out::default()
        };
        assert!(o.render(100).is_some(), "draw the first line");
        o.hw.clear();
        let clear = o.render(100).expect("clearing must redraw");
        assert_eq!(
            clear, "\r      ",
            "must erase the 6 columns of the old line"
        );
    }

    #[test]
    fn note_change_reports_only_a_real_segment_change() {
        // Catches: counting an unchanged segment as an update (inflating the
        // measured rate) or missing a real change (undercounting it).
        let mut seg = String::new();
        assert!(note_change(&mut seg, "a".to_string()));
        assert!(!note_change(&mut seg, "a".to_string()));
        assert!(note_change(&mut seg, "b".to_string()));
        assert_eq!(seg, "b");
    }

    #[test]
    fn hms_formats_seconds_within_a_day() {
        assert_eq!(hms(0), "00:00:00");
        assert_eq!(hms(3661), "01:01:01");
        assert_eq!(hms(86_399), "23:59:59");
        // Wraps at a day.
        assert_eq!(hms(86_400 + 5), "00:00:05");
    }
}
