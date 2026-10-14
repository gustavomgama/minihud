//! One diagnostics record, in both output shapes: a JSONL line and the matching
//! verbose text-log line.
//!
//! The JSONL schema is the ontology in [`crate::diag`]: every record carries the
//! same keys in the same order, so a run's file can be diffed against another
//! run's file line by line.

use serde::Serialize;

/// The event taxonomy. Every recorded event is exactly one of these, so the
/// JSONL never grows an undocumented kind a consumer would have to special-case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A minihud subsystem event (startup, dump, console, apply, stats).
    Subsystem,
    /// One hardware poll's complete sensor dump.
    SensorPoll,
    /// The LHM bridge session lifecycle (spawn, handshake, serving, close).
    BridgeLifecycle,
    /// A failed bridge session and its retry backoff.
    Retry,
    /// A span timing (enter/exit/close with a duration).
    Timing,
    /// One rendered stats line and the values behind it.
    RenderStat,
}

impl EventKind {
    /// The snake_case taxonomy string persisted in the JSONL.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Subsystem => "subsystem",
            Self::SensorPoll => "sensor_poll",
            Self::BridgeLifecycle => "bridge_lifecycle",
            Self::Retry => "retry",
            Self::Timing => "timing",
            Self::RenderStat => "render_stat",
        }
    }

    /// Parse a taxonomy string back (the JSONL is read back by tooling).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "subsystem" => Some(Self::Subsystem),
            "sensor_poll" => Some(Self::SensorPoll),
            "bridge_lifecycle" => Some(Self::BridgeLifecycle),
            "retry" => Some(Self::Retry),
            "timing" => Some(Self::Timing),
            "render_stat" => Some(Self::RenderStat),
            _ => None,
        }
    }
}

/// One persisted record. Keys serialize in declaration order.
#[derive(Clone, Debug, Serialize)]
pub struct Record {
    /// The run this record belongs to: `run-DD_MM_YYYY-HH-MM-SS`.
    pub run_id: String,
    /// RFC 3339 UTC instant, correlating with the live stream.
    pub timestamp: String,
    pub event_kind: EventKind,
    /// Emitting subsystem: the tracing target.
    pub subsystem: String,
    /// Severity/verbosity: trace, debug, info, warn, error.
    pub level: String,
    /// The span this record is attributed to (empty outside a span).
    pub span_name: String,
    /// Span/operation duration in microseconds (0 when not a timing).
    pub duration_us: u64,
    /// The complete sensor reading dump of a poll event (`{}` otherwise).
    pub sensor_values: serde_json::Value,
    /// The per-render stats of a render event (`{}` otherwise).
    pub render_stats: serde_json::Value,
    /// The event's structured body: every field of the tracing event.
    pub payload: serde_json::Map<String, serde_json::Value>,
    /// The human-readable line written to the verbose text log.
    pub message: String,
}

impl Record {
    /// The JSONL line: one compact JSON object plus a newline, so the file is
    /// newline-delimited and streamable.
    pub fn jsonl_line(&self) -> String {
        let mut line = serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string());
        line.push('\n');
        line
    }

    /// The verbose text-log line: clock, level, kind, subsystem, span, the
    /// event's fields, the message, then the duration when the record is a
    /// timing. The kind is padded so a developer skimming the file can read the
    /// taxonomy; the duration appears only on timings, so a plain line keeps
    /// its shape.
    pub fn text_line(&self) -> String {
        let clock = clock_of(&self.timestamp);
        let span = if self.span_name.is_empty() {
            String::new()
        } else {
            format!("[{}] ", self.span_name)
        };
        let fields = payload_fields(&self.payload);
        let fields = if fields.is_empty() {
            String::new()
        } else {
            format!("{fields} ")
        };
        let duration = if self.duration_us > 0 {
            format!(" {}us", self.duration_us)
        } else {
            String::new()
        };
        format!(
            "{clock} {:<5} {:<16} {subsystem} {span}{fields}{message}{duration}\n",
            self.level.to_uppercase(),
            self.event_kind.as_str(),
            subsystem = self.subsystem,
            span = span,
            fields = fields,
            message = self.message,
            duration = duration,
        )
    }
}

/// `k=v` for every payload entry, in the payload's (sorted, stable) order, so
/// the verbose log shows the event's structured values without the reader
/// having to parse the JSONL.
fn payload_fields(payload: &serde_json::Map<String, serde_json::Value>) -> String {
    payload
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `HH:MM:SS.mmm` from the ISO timestamp: the date is already on every record
/// via `run_id`, so the text log keeps only the time.
fn clock_of(timestamp: &str) -> String {
    timestamp.get(11..23).unwrap_or(timestamp).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Catches: a record shape that omits a key when a record does not use it
    /// (no `sensor_values` on a stats line, no `duration_us` on a poll), so a
    /// consumer's fixed-key lookup breaks and two runs no longer diff cleanly.
    /// Also catches an undocumented key creeping in, which no consumer knows.
    #[test]
    fn every_jsonl_record_carries_exactly_the_schema_keys() {
        let mut want = [
            "run_id",
            "timestamp",
            "event_kind",
            "subsystem",
            "level",
            "span_name",
            "duration_us",
            "sensor_values",
            "render_stats",
            "payload",
            "message",
        ];
        let v: serde_json::Value =
            serde_json::from_str(record_fixture().jsonl_line().trim()).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        want.sort_unstable();
        assert_eq!(keys, want, "the record must carry exactly the schema keys");
    }

    /// Catches: a record whose message carries a newline (a multi-line banner,
    /// or a bridge error with an embedded break): a JSONL record would then
    /// span lines and corrupt the stream for every consumer.
    #[test]
    fn jsonl_line_stays_one_line_even_with_a_multiline_message() {
        let rec = Record {
            message: "first\nsecond".to_string(),
            ..record_fixture()
        };
        let line = rec.jsonl_line();
        assert_eq!(
            line.matches('\n').count(),
            1,
            "one record, one line: {line}"
        );
        assert!(
            line.contains("first\\nsecond"),
            "the newline is escaped: {line}"
        );
    }

    /// Catches: a taxonomy string that does not match the agreed names (a typo
    /// such as `sensorpoll` or `bridge-lifecycle`), which would silently break a
    /// consumer filtering the JSONL by `event_kind`.
    #[test]
    fn event_kind_strings_round_trip_through_the_taxonomy() {
        for kind in [
            EventKind::Subsystem,
            EventKind::SensorPoll,
            EventKind::BridgeLifecycle,
            EventKind::Retry,
            EventKind::Timing,
            EventKind::RenderStat,
        ] {
            let s = kind.as_str();
            assert_eq!(EventKind::parse(s), Some(kind), "{s} must round-trip");
            assert_eq!(s, s.to_lowercase(), "{s} must be lowercase");
        }
        assert_eq!(EventKind::parse("render_stat"), Some(EventKind::RenderStat));
        assert_eq!(EventKind::parse("bogus"), None);
    }

    /// Catches: the verbose text log dropping the span or the level, so a
    /// developer reading the file cannot tell a trace from an info, or which
    /// span an event happened inside.
    #[test]
    fn text_line_carries_clock_level_subsystem_span_and_message() {
        let mut rec = record_fixture();
        rec.level = "trace".to_string();
        rec.event_kind = EventKind::SensorPoll;
        rec.span_name = "poll".to_string();
        rec.message = "render stats: CPU 26%".to_string();
        // The kind sits between the level and the subsystem, padded so the
        // columns line up for a reader skimming the file.
        assert_eq!(
            rec.text_line(),
            "18:30:00.123 TRACE sensor_poll      minihud::hw [poll] render stats: CPU 26%\n"
        );
        rec.span_name.clear();
        assert_eq!(
            rec.text_line(),
            "18:30:00.123 TRACE sensor_poll      minihud::hw render stats: CPU 26%\n",
            "no span means no empty brackets"
        );
    }

    /// Catches: the verbose log showing no duration on a timing, so the one
    /// thing a developer opens the file for (how long each step took) is
    /// invisible outside the JSONL.
    #[test]
    fn a_timing_line_shows_its_duration_and_kind() {
        let mut rec = record_fixture();
        rec.event_kind = EventKind::Timing;
        rec.span_name = "render".to_string();
        rec.message = "render".to_string();
        rec.duration_us = 1337;
        assert_eq!(
            rec.text_line(),
            "18:30:00.123 INFO  timing           minihud::hw [render] render 1337us\n"
        );
    }

    /// Catches: an event kind with no dedicated dump collapsing
    /// `sensor_values`/`render_stats` to null (a consumer would have to branch
    /// on the record kind just to read its numbers).
    #[test]
    fn a_record_without_a_dump_still_carries_empty_objects() {
        let v: serde_json::Value =
            serde_json::from_str(record_fixture().jsonl_line().trim()).unwrap();
        assert_eq!(v["sensor_values"], json!({}));
        assert_eq!(v["render_stats"], json!({}));
        assert_eq!(v["duration_us"], json!(0));
    }

    fn record_fixture() -> Record {
        Record {
            run_id: "run-14_10_2026-18-30-00".to_string(),
            timestamp: "2026-10-14T18:30:00.123456Z".to_string(),
            event_kind: EventKind::Subsystem,
            subsystem: "minihud::hw".to_string(),
            level: "info".to_string(),
            span_name: String::new(),
            duration_us: 0,
            sensor_values: json!({}),
            render_stats: json!({}),
            payload: serde_json::Map::new(),
            message: "hello".to_string(),
        }
    }
}
