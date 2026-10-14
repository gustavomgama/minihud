//! The console's event formatter.
//!
//! The app prints one line per event (`HH:MM:SS LEVEL message`). Diagnostics
//! adds tracing spans, and the stock `tracing-subscriber` formatter would print
//! each event's span scope in front of it — a visible change for a user who
//! never asked for diagnostics. This formatter is the stock line without the
//! span scope, and only includes the scope when `--diagnostics` asks for the
//! live trace surface.

use std::fmt;

use tracing::Metadata;
use tracing::Subscriber;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

/// The stock console line, with the span scope shown only when asked.
pub struct ConsoleFormat<T> {
    timer: T,
    /// Whether to print each event's span scope (`--diagnostics`).
    spans: bool,
}

impl<T: FormatTime> ConsoleFormat<T> {
    /// A formatter over `timer`; `spans` decides whether the live stream shows
    /// the span scope of each event.
    pub fn new(timer: T, spans: bool) -> Self {
        Self { timer, spans }
    }
}

impl<S, N, T> FormatEvent<S, N> for ConsoleFormat<T>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    T: FormatTime,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> fmt::Result {
        let meta = event.metadata();
        self.timestamp(&mut writer);
        self.level(&mut writer, meta);
        if self.spans {
            self.scope(ctx, &mut writer)?;
        }
        // The message and the event's fields, exactly as the stock formatter
        // renders them (message last).
        ctx.format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

impl<T: FormatTime> ConsoleFormat<T> {
    /// The dimmed `HH:MM:SS` the console line starts with.
    fn timestamp(&self, writer: &mut Writer<'_>) {
        if writer.has_ansi_escapes() {
            let _ = write!(writer, "\x1b[2m");
        }
        if self.timer.format_time(writer).is_err() {
            let _ = writer.write_str("<unknown time>");
        }
        if writer.has_ansi_escapes() {
            let _ = write!(writer, "\x1b[0m ");
        } else {
            let _ = writer.write_char(' ');
        }
    }

    /// The level, padded to five columns and coloured as the stock formatter
    /// colours it.
    fn level(&self, writer: &mut Writer<'_>, meta: &Metadata<'_>) {
        let level = match *meta.level() {
            tracing::Level::TRACE => ("TRACE", 35),
            tracing::Level::DEBUG => ("DEBUG", 34),
            tracing::Level::INFO => (" INFO", 32),
            tracing::Level::WARN => (" WARN", 33),
            tracing::Level::ERROR => ("ERROR", 31),
        };
        if writer.has_ansi_escapes() {
            let _ = write!(writer, "\x1b[{}m{}\x1b[0m ", level.1, level.0);
        } else {
            let _ = write!(writer, "{:<5} ", level.0);
        }
    }

    /// The entered spans, outermost first, in front of the event.
    fn scope<S, N>(&self, ctx: &FmtContext<'_, S, N>, writer: &mut Writer<'_>) -> fmt::Result
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
        N: for<'a> FormatFields<'a> + 'static,
    {
        let Some(scope) = ctx.event_scope() else {
            return Ok(());
        };
        let mut seen = false;
        for span in scope.from_root() {
            seen = true;
            if writer.has_ansi_escapes() {
                let _ = write!(
                    writer,
                    "\x1b[1m{}\x1b[0m\x1b[2m:\x1b[0m ",
                    span.metadata().name()
                );
            } else {
                let _ = write!(writer, "{}: ", span.metadata().name());
            }
        }
        if seen {
            writer.write_char(' ')?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::prelude::*;
    use tracing_subscriber::registry::Registry;

    use super::*;

    /// A clock frozen at `00:00:00`, so the captured line is deterministic.
    struct StubClock;

    impl FormatTime for StubClock {
        fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
            write!(w, "00:00:00")
        }
    }

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Capture what the console layer prints for `body`.
    fn captured(spans: bool, body: impl FnOnce()) -> String {
        let sink = Sink::default();
        let writer = sink.clone();
        let format = ConsoleFormat::new(StubClock, spans);
        let layer = tracing_subscriber::fmt::layer::<Registry>()
            .event_format(format)
            .with_writer(move || writer.clone());
        let sub = Registry::default().with(layer);
        tracing::subscriber::with_default(sub, body);
        let bytes = sink.0.lock().unwrap().clone();
        drop(sink);
        String::from_utf8(bytes).unwrap()
    }

    /// Catches: the default console line drifting from the pre-diagnostics
    /// format (a level with the wrong width or colour, a timestamp rendered
    /// differently), which is what a user reads on a bare run. The expected
    /// bytes are the stock `tracing-subscriber` output for a level `INFO`
    /// event with no spans.
    #[test]
    fn the_default_line_is_the_stock_console_line() {
        let out = captured(false, || tracing::info!("minihud v1.0.0"));
        assert_eq!(
            out,
            "\u{1b}[2m00:00:00\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m minihud v1.0.0\n"
        );
    }

    /// Catches: a span changing what a user's console shows — the whole reason
    /// this formatter exists. Entering the poll span must not prefix the
    /// bridge's info line on a run without `--diagnostics`.
    #[test]
    fn a_span_is_invisible_without_the_diagnostics_flag() {
        let out = captured(false, || {
            let span = tracing::info_span!("poll", poll_ms = 500_u64);
            let _g = span.enter();
            tracing::info!("lhm bridge: serving");
        });
        assert!(
            !out.contains("poll"),
            "the span scope must not reach a user's console: {out:?}"
        );
        assert!(
            out.contains(" INFO\u{1b}[0m lhm bridge: serving\n"),
            "the line itself is unchanged: {out:?}"
        );
    }

    /// Catches: the live trace surface (`--diagnostics`) not showing the span
    /// an event happened inside, which is the point of the flag.
    #[test]
    fn a_span_is_shown_with_the_diagnostics_flag() {
        let out = captured(true, || {
            let span = tracing::info_span!("poll");
            let _g = span.enter();
            tracing::info!("apply: 43 sensors");
        });
        assert!(
            out.contains("poll\u{1b}[0m\u{1b}[2m:\u{1b}[0m"),
            "the span scope must be shown live: {out:?}"
        );
        assert!(out.contains("apply: 43 sensors\n"), "{out:?}");
    }

    /// Catches: an event's fields being dropped from the console line, so a
    /// developer's `--diagnostics` stream loses the structured values. The
    /// field styling itself belongs to `tracing-subscriber`; what matters here
    /// is that the values reach the line at all.
    #[test]
    fn an_events_fields_are_printed_after_the_message() {
        let out = captured(
            true,
            || tracing::info!(target: "minihud::hw", kind = "sensor_poll", count = 43_u64, "poll"),
        );
        assert!(out.ends_with("poll \u{1b}[3mkind\u{1b}[0m\u{1b}[2m=\u{1b}[0m\"sensor_poll\" \u{1b}[3mcount\u{1b}[0m\u{1b}[2m=\u{1b}[0m43\n"), "the fields must be printed: {out:?}");
    }
}
