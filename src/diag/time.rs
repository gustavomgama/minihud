//! Run identity and event timestamps for the diagnostics record.
//!
//! A run is named `run-DD_MM_YYYY-HH-MM-SS` (the machine's LOCAL clock), so two
//! runs never collide by name unless they start in the same second. Both the
//! folder layout and the JSONL `timestamp` come from here, so the naming rule
//! lives in one place and is unit-tested against hand-computed instants.
//! Local time is a hard requirement: every artifact a human reads — the
//! console's compact clock, the run folder in Explorer, the log itself — shows
//! the machine's wall clock, and a UTC record would read hours off.

use std::time::{SystemTime, UNIX_EPOCH};

/// The `run-` prefix every diagnostics artifact carries.
pub const RUN_PREFIX: &str = "run-";

/// Name a run for the LOCAL instant `secs`: `run-DD_MM_YYYY-HH-MM-SS`.
///
/// DD/MM/YYYY is the mandated order — a US-style MM/DD/YYYY would mis-sort and
/// mis-name the run, and the day needs zero-padding so `05` and `5` do not both
/// appear.
pub fn run_id(secs: u64) -> String {
    let (y, m, d) = civil(secs);
    let day = secs % 86_400;
    format!(
        "{}{d:02}_{m:02}_{y:04}-{:02}-{:02}-{:02}",
        RUN_PREFIX,
        day / 3600,
        (day % 3600) / 60,
        day % 60
    )
}

/// Local wall-clock timestamp for `secs` + `micros`:
/// `YYYY-MM-DDTHH:MM:SS.ffffff` — fixed width and fixed precision, so two
/// records are diffable. No `Z`: the instant is local, and claiming UTC would
/// be a lie the developer reads as an offset error.
pub fn iso_timestamp(secs: u64, micros: u32) -> String {
    let (y, m, d) = civil(secs);
    let day = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{micros:06}",
        day / 3600,
        (day % 3600) / 60,
        day % 60
    )
}

/// The machine's local wall clock as `(whole seconds, microseconds)` since the
/// epoch. The epoch value is shifted by the local UTC offset, so feeding it to
/// [`iso_timestamp`] prints the same clock the developer sees.
pub fn now_parts() -> (u64, u32) {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let micros = d.subsec_micros();
    // Clamp at the epoch only: west-of-UTC zones (UTC-3) yield a SMALLER
    // local epoch than the UTC clock, and preferring UTC there would be the
    // exact bug this fixes.
    let local = local_epoch_secs().max(0);
    (local as u64, micros)
}

/// The current wall clock as one local ISO 8601 timestamp.
pub fn iso_timestamp_now() -> String {
    let (secs, micros) = now_parts();
    iso_timestamp(secs, micros)
}

/// The machine's local epoch seconds: the OS local time converted back to an
/// epoch instant. On Windows this is `GetLocalTime`, converted through the same
/// calendar math as [`civil`]; elsewhere it falls back to the UTC clock (the
/// shipped product is Windows-only, so the fallback only affects tests off-Win).
pub fn local_epoch_secs() -> i64 {
    #[cfg(windows)]
    {
        use windows::Win32::System::SystemInformation::GetLocalTime;
        // SAFETY: GetLocalTime takes no buffers; the SYSTEMTIME is returned.
        let t = unsafe { GetLocalTime() };
        let days = days_from_civil(i64::from(t.wYear), u32::from(t.wMonth), u32::from(t.wDay));
        days * 86_400 + i64::from(t.wHour) * 3600 + i64::from(t.wMinute) * 60 + i64::from(t.wSecond)
    }
    #[cfg(not(windows))]
    {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

/// Calendar date for `secs` since the epoch: `(year, month, day)`. Howard
/// Hinnant's `civil_from_days`, so the conversion is exact across leap years
/// without a calendar dependency.
fn civil(secs: u64) -> (i64, u32, u32) {
    // `secs` is unsigned, so days are non-negative and every step below stays
    // in unsigned arithmetic until the year is widened for display.
    let z = secs / 86_400 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era as i64 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (y + i64::from(m <= 2), m, d)
}

/// Days since the epoch for a proleptic-Gregorian date (the inverse of
/// [`civil`]), so tests and the local-clock conversion share one calendar.
pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: the run name drifting off the mandated `run-DD_MM_YYYY-HH-MM-SS`
    /// shape (US month-first order, or a missing `run-` prefix), which would make
    /// runs sort wrong and collide with another developer's naming expectation.
    #[test]
    fn run_id_names_a_run_with_the_mandated_timestamp_pattern() {
        let secs = (days_from_civil(2026, 10, 14) * 86_400 + 18 * 3600 + 30 * 60) as u64;
        assert_eq!(run_id(secs), "run-14_10_2026-18-30-00");
    }

    /// Catches: single-digit days/hours written unpadded, so `run-5_...` and
    /// `run-05_...` would name different strings for the same day and runs would
    /// no longer sort chronologically by name.
    #[test]
    fn run_id_zero_pads_every_component() {
        let secs = (days_from_civil(1970, 1, 5) * 86_400 + 9 * 3600 + 5 * 60 + 3) as u64;
        assert_eq!(run_id(secs), "run-05_01_1970-09-05-03");
    }

    /// Catches: a civil-date conversion that is not leap-year correct, so a run
    /// started on 29 February would be named with the wrong month entirely.
    #[test]
    fn run_id_handles_a_leap_day() {
        let secs = (days_from_civil(2024, 2, 29) * 86_400) as u64;
        assert_eq!(run_id(secs), "run-29_02_2024-00-00-00");
        // The next day is 1 March, not 30 February.
        assert_eq!(run_id(secs + 86_400), "run-01_03_2024-00-00-00");
    }

    /// Catches: the ISO timestamp drifting back to UTC (or regaining a `Z`),
    /// so the record's clock reads ~3h off the developer's wall clock on a
    /// UTC-3 machine and cannot be matched against the console line or the
    /// Explorer mtime.
    #[test]
    fn iso_timestamp_is_local_time_without_the_utc_marker() {
        let secs = (days_from_civil(2026, 10, 14) * 86_400 + 18 * 3600 + 30 * 60) as u64;
        assert_eq!(iso_timestamp(secs, 123_456), "2026-10-14T18:30:00.123456");
        assert_eq!(iso_timestamp(0, 0), "1970-01-01T00:00:00.000000");
    }

    /// Catches: the wall clock being read as UTC while every human artifact
    /// (the console line, the run folder in Explorer) shows local time. On a
    /// UTC-3 machine this test fails with the UTC clock by ~3 hours.
    #[test]
    fn now_parts_reads_the_machines_local_clock() {
        let (secs, micros) = now_parts();
        let expected = local_epoch_secs();
        assert!(
            (secs as i64 - expected).abs() < 2,
            "now_parts must be local: got {secs}, machine clock {expected}"
        );
        assert!(micros < 1_000_000, "micros must stay below a second");
        assert!(secs > 1_700_000_000, "the clock must be current: {secs}");
    }
}
