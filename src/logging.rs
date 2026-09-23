//! Compact, coloured output for the operational log.
//!
//! `INFO` is intended for people reading `debug.log`; structured spans and
//! event fields remain available at `DEBUG` and `TRACE` for diagnosis.

use std::fmt;

use chrono::{Local, Offset as _};
use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{
    fmt::{FmtContext, FormatEvent, FormatFields, FormattedFields, format::Writer},
    registry::LookupSpan,
};

pub struct HumanEvent;

/// A coloured, compact line for a command issued in a guild.
pub fn interaction(guild: &str, player: &str, track: Option<&str>, action: &str) -> String {
    let prefix = format!("\x1b[36m[{guild}]\x1b[0m \x1b[35m[{player}]\x1b[0m");
    match track {
        Some(track) => format!("{prefix} \x1b[32m{{{track}}}\x1b[0m {}", past_tense(action)),
        None => format!("{prefix} \x1b[33m{{{action}}}\x1b[0m completed"),
    }
}

/// A coloured, compact playback transition.
pub fn track(title: &str, action: &str) -> String {
    format!("\x1b[32m{{{title}}}\x1b[0m {action}")
}

fn past_tense(action: &str) -> &str {
    match action {
        "skip" => "skipped",
        "pause" => "paused",
        "resume" => "resumed",
        "stop" => "stopped",
        _ => "updated",
    }
}

#[derive(Default)]
struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ = fmt::write(&mut self.0, format_args!("{value:?}"));
        }
    }
}

impl<S, N> FormatEvent<S, N> for HumanEvent
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let level = *event.metadata().level();
        // Match tracing-subscriber's standard level palette and spacing.
        let (colour, label) = match level {
            tracing::Level::ERROR => ("31", "ERROR"),
            tracing::Level::WARN => ("33", " WARN"),
            tracing::Level::INFO => ("32", " INFO"),
            tracing::Level::DEBUG => ("34", "DEBUG"),
            tracing::Level::TRACE => ("35", "TRACE"),
        };

        write!(writer, "\x1b[2m{}\x1b[0m", local_timestamp())?;
        write!(writer, " \x1b[{colour}m{label}\x1b[0m ")?;

        if level == tracing::Level::INFO {
            let mut message = MessageVisitor::default();
            event.record(&mut message);
            write!(writer, "{}", message.0)?;
            return writeln!(writer);
        }

        ctx.field_format().format_fields(writer.by_ref(), event)?;

        // Keep identifying fields out of the everyday INFO view, while making
        // the full causal context available when debugging is enabled.
        if level <= tracing::Level::DEBUG
            && let Some(scope) = ctx.event_scope()
        {
            for span in scope.from_root() {
                let extensions = span.extensions();
                if let Some(fields) = extensions.get::<FormattedFields<N>>()
                    && !fields.is_empty()
                {
                    write!(writer, " [{fields}]")?;
                }
            }
        }

        writeln!(writer)
    }
}

fn local_timestamp() -> String {
    let now = Local::now();
    let seconds = now.offset().fix().local_minus_utc();
    let sign = if seconds < 0 { '-' } else { '+' };
    let minutes = seconds.unsigned_abs() / 60;
    let hours = minutes / 60;

    let offset = if minutes.is_multiple_of(60) {
        format!("UTC{sign}{hours}")
    } else {
        format!("UTC{sign}{hours}:{:02}", minutes % 60)
    };
    format!("{} ({offset})", now.format("%Y-%m-%d %H:%M:%S"))
}
