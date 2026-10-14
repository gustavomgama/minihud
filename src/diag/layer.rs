//! The tracing layer that persists every event and span.
//!
//! Two subsytems share this layer: the live console stream, which is filtered
//! to `info` (or `trace` with `--diagnostics`), and the persisted record, which
//! takes EVERY event at EVERY level. The split is what makes the `--diagnostics`
//! flag safe to ship: it only changes how loud the console is.
//!
//! Two details make that split work against `tracing`'s callsite interest cache:
//!
//! * [`Layer::enabled`] always returns true, so an event one layer's filter
//!   rejects is still offered to this one (a `Filtered` layer that disables an
//!   event reports enabled so the rest of the stack still sees it).
//! * [`Layer::max_level_hint`] reports `TRACE`, so a `trace_span!` is created
//!   at all — without it the interest cache would never construct trace spans
//!   and every persisted timing would silently vanish on a default run.

use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Map, Value};
use tracing::field::Field;
use tracing::{span, Event, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use super::record::{EventKind, Record};
use super::sink::Diagnostics;
use super::time;

/// The target the layer's own live mirror emits under. The console layer
/// prints it (at `trace`, so only with `--diagnostics`), and the diagnostics
/// layer skips it, so a mirrored timing is never recorded twice.
pub const LIVE_TARGET: &str = "minihud::diag";

/// The default event kind for a tracing target, so an event a subsytem emits
/// without naming its kind still lands in the bucket a reader queries.
pub fn classify_target(target: &str) -> EventKind {
    match target.rsplit("::").next().unwrap_or(target) {
        "lhm" | "bridge" => EventKind::BridgeLifecycle,
        "poll" | "sensors" | "apply" | "inventory" => EventKind::SensorPoll,
        "render" | "console" => EventKind::RenderStat,
        "retry" => EventKind::Retry,
        _ => EventKind::Subsystem,
    }
}

/// Collects one event's (or span's) fields into a JSON object.
#[derive(Default)]
pub struct FieldVisitor(pub Map<String, Value>);

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // `tracing`'s formatted message reaches here as `Arguments`, whose
        // Debug is its text — so a literal message records unquoted.
        self.0.insert(
            field.name().to_string(),
            Value::String(format!("{value:?}")),
        );
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), json!(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_string(), json!(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_string(), json!(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_string(), json!(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        if value.is_finite() {
            self.0.insert(field.name().to_string(), json!(value));
        } else {
            // A non-finite reading is not valid JSON, and a sensor that reports
            // one must not corrupt the stream for every later record.
            self.0
                .insert(field.name().to_string(), json!(value.to_string()));
        }
    }
}

/// A span's start instant and the fields it was created with, read back when
/// it closes so the persisted timing carries both.
struct SpanStart {
    at: Instant,
    fields: Map<String, Value>,
}

/// The persisted-record layer: writes one record per event and one per span.
pub struct DiagLayer {
    sink: Arc<Diagnostics>,
}

impl DiagLayer {
    pub fn new(sink: Arc<Diagnostics>) -> Self {
        Self { sink }
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for DiagLayer {
    /// Always interested: this layer takes every event whatever the live
    /// stream does with it.
    fn enabled(&self, _metadata: &Metadata<'_>, _cx: Context<'_, S>) -> bool {
        true
    }

    /// Report `TRACE` so trace-level events and spans are constructed even
    /// when the live console stream is filtered to `info`.
    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::TRACE)
    }

    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, cx: Context<'_, S>) {
        let Some(span) = cx.span(id) else {
            return;
        };
        let mut fields = FieldVisitor::default();
        attrs.record(&mut fields);
        let mut ext = span.extensions_mut();
        if ext.get_mut::<SpanStart>().is_none() {
            ext.insert(SpanStart {
                at: Instant::now(),
                fields: fields.0,
            });
        }
    }

    fn on_close(&self, id: span::Id, cx: Context<'_, S>) {
        let Some(span) = cx.span(&id) else {
            return;
        };
        let meta = span.metadata();
        let name = meta.name().to_string();
        let mut ext = span.extensions_mut();
        // A span closes once; a second notification carries nothing to time.
        let Some(start) = ext.remove::<SpanStart>() else {
            return;
        };
        drop(ext);
        let duration_us = start.at.elapsed().as_micros() as u64;
        let mut fields = start.fields;
        let message = take_message(&mut fields).unwrap_or_else(|| name.clone());
        let record = Record {
            run_id: self.sink.run_id().to_string(),
            timestamp: time::iso_timestamp_now(),
            event_kind: EventKind::Timing,
            subsystem: meta.target().to_string(),
            level: meta.level().as_str().to_lowercase(),
            span_name: name,
            duration_us,
            sensor_values: lift(&mut fields, "sensor_values"),
            render_stats: lift(&mut fields, "render_stats"),
            payload: fields,
            message,
        };
        let _ = self.sink.record(&record);
        // The live trace surface gets the timing too. Emitted as an event so
        // the console layer prints it — at `trace`, i.e. only under
        // `--diagnostics` — and skipped above so the files keep one record.
        tracing::trace!(target: LIVE_TARGET, "{}", record.text_line().trim_end());
    }

    fn on_event(&self, event: &Event<'_>, cx: Context<'_, S>) {
        let mut fields = FieldVisitor::default();
        event.record(&mut fields);
        let span_name = cx
            .lookup_current()
            .map(|s| s.name().to_string())
            .unwrap_or_default();
        let meta = event.metadata();
        if meta.target() == LIVE_TARGET {
            return;
        }
        let message = take_message(&mut fields.0).unwrap_or_default();
        let mut fields = fields.0;
        let kind = fields
            .get("kind")
            .and_then(|v| v.as_str())
            .and_then(EventKind::parse)
            .unwrap_or_else(|| classify_target(meta.target()));
        fields.remove("kind");
        let record = Record {
            run_id: self.sink.run_id().to_string(),
            timestamp: time::iso_timestamp_now(),
            event_kind: kind,
            subsystem: meta.target().to_string(),
            level: meta.level().as_str().to_lowercase(),
            span_name,
            duration_us: 0,
            sensor_values: lift(&mut fields, "sensor_values"),
            render_stats: lift(&mut fields, "render_stats"),
            payload: fields,
            message,
        };
        let _ = self.sink.record(&record);
    }
}

/// The event's human-readable line, lifted out of its fields so the payload
/// carries the structured data only.
pub(crate) fn take_message(fields: &mut Map<String, Value>) -> Option<String> {
    fields
        .remove("message")
        .map(|v| match v {
            Value::String(s) => s,
            other => other.to_string(),
        })
        .filter(|m| !m.is_empty())
}

/// Lift a structured field out of the payload into the record's own object
/// field. A `serde_json` value reaches here as its compact JSON text (tracing
/// records it through `Display`/`Debug`), so it is parsed back.
fn lift(fields: &mut Map<String, Value>, key: &str) -> Value {
    let Some(value) = fields.remove(key) else {
        return Value::Object(Map::new());
    };
    let text = match value {
        Value::String(s) => s,
        other => return other,
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(parsed) => parsed,
        Err(_) => Value::Object(Map::from_iter([("raw".to_string(), Value::String(text))])),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use serde::Serialize;
    use serde_json::Value;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::registry::Registry;
    use tracing_subscriber::EnvFilter;

    use super::super::sink::{JSONL_NAME, TEXT_NAME};
    use super::*;

    /// One bridge sensor row: the same shape the sidecar emits.
    #[derive(Serialize)]
    struct Sensor {
        hw: &'static str,
        #[serde(rename = "type")]
        sensor_type: &'static str,
        name: &'static str,
        value: f64,
    }

    fn run(tag: &str) -> (PathBuf, Arc<Diagnostics>) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("minihud-layer-{tag}-{nanos}"));
        std::fs::create_dir_all(&base).unwrap();
        let d = Diagnostics::create(&base, "run-14_10_2026-18-30-00".to_string());
        (base, d)
    }

    fn read(d: &Diagnostics, name: &str) -> String {
        std::fs::read_to_string(d.dir().join(name)).unwrap_or_default()
    }

    fn records(d: &Diagnostics) -> Vec<Value> {
        read(d, JSONL_NAME)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// A stand-in for the live console stream: nothing is ever written.
    struct SilentLayer;

    impl<S: Subscriber> Layer<S> for SilentLayer {}

    /// Catches: an event whose target does not sort into the taxonomy bucket a
    /// reader queries (a retry recorded as a plain subsystem event), so the
    /// bridge lifecycle and retries are not addressable in the JSONL.
    #[test]
    fn an_event_is_recorded_in_the_bucket_its_kind_or_target_names() {
        let (base, d) = run("kind");
        let sub = Registry::default().with(DiagLayer::new(d.clone()));
        tracing::subscriber::with_default(sub, || {
            tracing::info!(target: "minihud::hw::lhm", "lhm bridge: serving");
            tracing::warn!(target: "minihud::retry", kind = "retry", "bridge failed; retry");
            tracing::trace!(target: "minihud::render", "render stats");
        });

        let recs = records(&d);
        let kinds: Vec<&str> = recs
            .iter()
            .map(|r| r["event_kind"].as_str().unwrap_or("?"))
            .collect();
        assert!(
            kinds.contains(&"bridge_lifecycle"),
            "the bridge line must be a lifecycle event: {kinds:?}"
        );
        assert!(
            kinds.contains(&"retry"),
            "a retry must be a retry: {kinds:?}"
        );
        assert!(
            kinds.contains(&"render_stat"),
            "a rendered line must be a render stat: {kinds:?}"
        );
        assert_eq!(recs[0]["level"], "info");
        assert_eq!(recs[0]["message"], "lhm bridge: serving");
        assert_eq!(recs[0]["subsystem"], "minihud::hw::lhm");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Catches: the persisted record being gated by the live filter — the
    /// default run (console at `info`, or a `RUST_LOG` that silences trace)
    /// would then write no JSONL at all, and every timing would vanish.
    #[test]
    fn the_persisted_layer_takes_events_and_spans_the_live_filter_blocks() {
        let (base, d) = run("gate");
        let sub = Registry::default()
            // A live stream whose filter is OFF: exactly what `RUST_LOG=off`
            // or the default `info` level does to a trace line.
            .with(SilentLayer.with_filter(EnvFilter::new("off")))
            .with(DiagLayer::new(d.clone()));
        tracing::subscriber::with_default(sub, || {
            tracing::trace!(target: "minihud::poll", sensors = 43_u64, "poll: 43 sensors");
            let span = tracing::trace_span!(target: "minihud::render", "render", line = "CPU 42%");
            span.in_scope(|| tracing::trace!("inside the span"));
        });

        let jsonl = read(&d, JSONL_NAME);
        assert!(
            jsonl.contains("poll: 43 sensors"),
            "a blocked trace event must still be persisted: {jsonl}"
        );
        assert!(
            jsonl.contains("\"sensors\":43"),
            "numeric fields must persist as numbers: {jsonl}"
        );
        assert!(
            jsonl.contains("\"event_kind\":\"timing\"") && jsonl.contains("render"),
            "a trace span must still be timed and persisted: {jsonl}"
        );
        let text = read(&d, TEXT_NAME);
        assert!(
            text.contains("poll: 43 sensors"),
            "the verbose log carries the same event: {text}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Catches: a span's timing never reaching the file (so every `duration_us`
    /// is 0 and the timings the flag promises do not exist), or a duration
    /// measured in the wrong unit.
    #[test]
    fn a_closed_span_is_recorded_as_a_timing_with_its_duration() {
        let (base, d) = run("timing");
        let sub = Registry::default().with(DiagLayer::new(d.clone()));
        tracing::subscriber::with_default(sub, || {
            let span = tracing::trace_span!(target: "minihud::render", "render", steps = 4096_u64);
            span.in_scope(|| std::thread::sleep(Duration::from_millis(150)));
        });

        let recs = records(&d);
        let timing = recs
            .iter()
            .find(|r| r["event_kind"] == "timing")
            .unwrap_or_else(|| panic!("no timing record in {recs:?}"));
        assert_eq!(timing["span_name"], "render");
        assert_eq!(timing["payload"]["steps"], 4096);
        assert_eq!(timing["subsystem"], "minihud::render");
        let micros = timing["duration_us"].as_u64().unwrap();
        assert!(
            micros >= 100_000,
            "a 150ms span must record at least 100ms: {micros}us"
        );
        let text = read(&d, TEXT_NAME);
        assert!(
            text.contains("us") && text.contains("render"),
            "the verbose log must show the duration: {text}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Catches: the per-poll sensor dump landing in `payload` instead of
    /// `sensor_values`, so the complete reading set is not addressable.
    #[test]
    fn a_poll_records_the_complete_sensor_dump_as_sensor_values() {
        let (base, d) = run("dump");
        let sub = Registry::default().with(DiagLayer::new(d.clone()));
        tracing::subscriber::with_default(sub, || {
            let sensors = vec![
                Sensor {
                    hw: "Cpu:Intel",
                    sensor_type: "Load",
                    name: "CPU Total",
                    value: 42.0,
                },
                Sensor {
                    hw: "Memory:",
                    sensor_type: "Data",
                    name: "Memory Used",
                    value: 8.0,
                },
            ];
            tracing::trace!(
                target: "minihud::poll",
                kind = "sensor_poll",
                sensor_values = %serde_json::to_value(&sensors).unwrap(),
                "poll: 2 sensors"
            );
        });

        let recs = records(&d);
        let poll = recs
            .iter()
            .find(|r| r["event_kind"] == "sensor_poll")
            .unwrap_or_else(|| panic!("no sensor_poll record in {recs:?}"));
        assert_eq!(
            poll["sensor_values"][0]["value"], 42.0,
            "the whole dump must land in sensor_values"
        );
        assert_eq!(poll["sensor_values"][1]["hw"], "Memory:");
        assert_eq!(
            poll["payload"].get("sensor_values"),
            None,
            "the dump must not be duplicated in the payload"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Catches: the per-render stats collapsing into a string or being dropped
    /// from `render_stats`, so the flag's trace-level stats are unreadable.
    #[test]
    fn a_render_records_its_stats_as_render_stats() {
        let (base, d) = run("render");
        let sub = Registry::default().with(DiagLayer::new(d.clone()));
        tracing::subscriber::with_default(sub, || {
            let stats = serde_json::json!({"cpu_percent": 42.0, "cpu_temp_c": null});
            tracing::trace!(
                target: "minihud::render",
                kind = "render_stat",
                render_stats = %stats,
                line = "CPU 42%",
                "render stats"
            );
        });

        let recs = records(&d);
        let render = recs
            .iter()
            .find(|r| r["event_kind"] == "render_stat")
            .unwrap_or_else(|| panic!("no render_stat record in {recs:?}"));
        assert_eq!(render["render_stats"]["cpu_percent"], 42.0);
        assert_eq!(render["payload"]["line"], "CPU 42%");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Catches: an event inside a span being attributed to no span (so the
    /// file cannot reconstruct nesting), or to the wrong span.
    #[test]
    fn an_event_recorded_inside_a_span_carries_the_span_name() {
        let (base, d) = run("nested");
        let sub = Registry::default().with(DiagLayer::new(d.clone()));
        tracing::subscriber::with_default(sub, || {
            let span = tracing::trace_span!(target: "minihud::loop", "loop");
            span.in_scope(|| {
                tracing::trace!(target: "minihud::apply", kind = "sensor_poll", "apply: 43 sensors")
            });
        });

        let recs = records(&d);
        let event = recs
            .iter()
            .find(|r| r["message"] == "apply: 43 sensors")
            .unwrap_or_else(|| panic!("no nested record in {recs:?}"));
        assert_eq!(
            event["span_name"], "loop",
            "the event must be attributed to its span"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Catches: a bridge raw line with an embedded newline splitting one JSONL
    /// record across two lines, which corrupts the stream for every consumer.
    #[test]
    fn a_multiline_payload_stays_on_one_jsonl_line() {
        let (base, d) = run("multiline");
        let sub = Registry::default().with(DiagLayer::new(d.clone()));
        tracing::subscriber::with_default(sub, || {
            tracing::warn!(target: "minihud::bridge", raw = "line one\nline two", "bridge stderr");
        });

        let jsonl = read(&d, JSONL_NAME);
        assert_eq!(jsonl.trim_end().lines().count(), 1, "{jsonl}");
        assert!(jsonl.contains("line one\\nline two"), "{jsonl}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Catches: a target needing no explicit `kind` (a bridge tick, a stats
    /// line) falling back to `subsystem`, which would leave the taxonomy empty.
    #[test]
    fn classify_target_keeps_every_bucket_reachable() {
        assert_eq!(
            classify_target("minihud::hw::lhm"),
            EventKind::BridgeLifecycle
        );
        assert_eq!(classify_target("minihud::poll"), EventKind::SensorPoll);
        assert_eq!(classify_target("minihud::apply"), EventKind::SensorPoll);
        assert_eq!(classify_target("minihud::render"), EventKind::RenderStat);
        assert_eq!(classify_target("minihud::retry"), EventKind::Retry);
        assert_eq!(classify_target("minihud"), EventKind::Subsystem);
        assert_eq!(classify_target("minihud::loop"), EventKind::Subsystem);
    }

    /// Catches: the message reaching the payload as well as its own field, so
    /// the text log reads the message twice.
    #[test]
    fn the_message_is_lifted_out_of_the_payload() {
        let (base, d) = run("message");
        let sub = Registry::default().with(DiagLayer::new(d.clone()));
        tracing::subscriber::with_default(sub, || {
            tracing::info!(target: "minihud::process", count = 7_u64, "process.start");
        });

        let recs = records(&d);
        assert_eq!(recs[0]["message"], "process.start");
        assert_eq!(recs[0]["payload"]["count"], 7);
        assert_eq!(
            recs[0]["payload"].get("message"),
            None,
            "the message must not be duplicated in the payload"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Catches: an unreadable level or a level that is not one of the five, so
    /// a consumer cannot filter the stream by severity.
    #[test]
    fn every_level_is_reported_by_name() {
        let (base, d) = run("levels");
        let sub = Registry::default().with(DiagLayer::new(d.clone()));
        tracing::subscriber::with_default(sub, || {
            tracing::error!(target: "minihud::bridge", "bridge died");
            tracing::warn!(target: "minihud::bridge", "bridge refused");
            tracing::info!(target: "minihud::process", "serving");
            tracing::debug!(target: "minihud::apply", "apply");
            tracing::trace!(target: "minihud::poll", "poll");
        });

        let levels: Vec<String> = records(&d)
            .iter()
            .map(|r| r["level"].as_str().unwrap().to_string())
            .collect();
        for want in ["error", "warn", "info", "debug", "trace"] {
            assert!(
                levels.iter().any(|l| l == want),
                "{want} missing from {levels:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Catches: a `Record`'s text line dropping the duration, so the verbose
    /// log cannot show a timing at all.
    #[test]
    fn a_timing_text_line_shows_its_duration() {
        let mut payload = Map::new();
        payload.insert("steps".into(), json!(4096));
        let rec = Record {
            run_id: "run-14_10_2026-18-30-00".to_string(),
            timestamp: "2026-10-14T18:30:00.123456Z".to_string(),
            event_kind: EventKind::Timing,
            subsystem: "minihud::render".to_string(),
            level: "trace".to_string(),
            span_name: "render".to_string(),
            duration_us: 1337,
            sensor_values: Value::Object(Map::new()),
            render_stats: Value::Object(Map::new()),
            payload,
            message: "render".to_string(),
        };
        let text = rec.text_line();
        assert!(text.contains("1337us"), "{text}");
        assert!(text.contains("timing"), "{text}");
        assert!(text.contains("steps=4096"), "{text}");
    }

    /// Catches: the JSONL record's `timestamp` being empty, or regaining the
    /// UTC `Z` (a UTC claim on a local clock reads hours off the developer's
    /// wall), so the live stream and the file cannot be correlated.
    #[test]
    fn a_record_timestamp_comes_from_the_wall_clock() {
        let (base, d) = run("clock");
        let sub = Registry::default().with(DiagLayer::new(d.clone()));
        tracing::subscriber::with_default(
            sub,
            || tracing::info!(target: "minihud::process", "hello"),
        );

        let ts = records(&d)[0]["timestamp"].as_str().unwrap().to_string();
        assert_eq!(ts.len(), 26, "{ts}");
        assert!(!ts.ends_with('Z'), "local time carries no UTC marker: {ts}");
        let _ = std::fs::remove_dir_all(&base);
    }
}
