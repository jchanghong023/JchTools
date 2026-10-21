use super::sanitize::{render, safe_error, sensitive_name};
use super::*;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::field::{Field, Visit};
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, Writer};
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::{FmtContext, FormattedFields};
use tracing_subscriber::registry::LookupSpan;

fn quoted(writer: &mut Writer<'_>, value: &str) -> fmt::Result {
    writer.write_char('"')?;
    for character in value.chars() {
        match character {
            '"' => writer.write_str("\\\"")?,
            '\\' => writer.write_str("\\\\")?,
            '\n' => writer.write_str("\\n")?,
            '\r' => writer.write_str("\\r")?,
            '\t' => writer.write_str("\\t")?,
            character if character.is_control() => {
                write!(writer, "\\u{{{:x}}}", u32::from(character))?;
            }
            character => writer.write_char(character)?,
        }
    }
    writer.write_char('"')
}

fn atom(writer: &mut Writer<'_>, value: &str) -> fmt::Result {
    if !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | ':' | '.')
        })
    {
        writer.write_str(value)
    } else {
        quoted(writer, value)
    }
}

#[derive(Clone, Copy)]
pub(super) struct DiagnosticFields;

struct FieldVisitor<'a, 'writer> {
    writer: &'a mut Writer<'writer>,
    result: fmt::Result,
    skip_header: bool,
}

impl FieldVisitor<'_, '_> {
    fn field(&mut self, field: &Field, value: impl fmt::Display) {
        if self.skip_header && matches!(field.name(), "component" | "event" | "pid") {
            return;
        }
        if self.result.is_ok() {
            self.result = if sensitive_name(field.name()) {
                write!(self.writer, " {}=\"[redacted]\"", field.name())
            } else {
                write!(self.writer, " {}={value}", field.name())
            };
        }
    }

    fn text(&mut self, field: &Field, value: &str) {
        if self.skip_header && matches!(field.name(), "component" | "event" | "pid") {
            return;
        }
        if self.result.is_ok() {
            let safe = if sensitive_name(field.name()) {
                "[redacted]".into()
            } else {
                safe_error(value)
            };
            self.result =
                write!(self.writer, " {}=", field.name()).and_then(|()| quoted(self.writer, &safe));
        }
    }
}

impl Visit for FieldVisitor<'_, '_> {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.field(field, value);
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.field(field, value);
    }
    fn record_i128(&mut self, field: &Field, value: i128) {
        self.field(field, value);
    }
    fn record_u128(&mut self, field: &Field, value: u128) {
        self.field(field, value);
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.field(field, value);
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.field(field, value);
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.text(field, value);
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.text(field, &render(value));
    }
}

impl<'writer> FormatFields<'writer> for DiagnosticFields {
    fn format_fields<R: RecordFields>(
        &self,
        mut writer: Writer<'writer>,
        fields: R,
    ) -> fmt::Result {
        let mut visitor = FieldVisitor {
            writer: &mut writer,
            result: Ok(()),
            skip_header: false,
        };
        fields.record(&mut visitor);
        visitor.result
    }
}

#[derive(Default)]
struct HeaderVisitor {
    component: Option<String>,
    event: Option<String>,
}

impl Visit for HeaderVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "component" => self.component = Some(safe_error(value)),
            "event" => self.event = Some(safe_error(value)),
            _ => {}
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        match field.name() {
            "component" => self.component = Some(safe_error(&render(value))),
            "event" => self.event = Some(safe_error(&render(value))),
            _ => {}
        }
    }
}

pub(super) struct DiagnosticEvent {
    pub(super) dropped: tracing_appender::non_blocking::ErrorCounter,
    pub(super) reported_drops: AtomicUsize,
}

fn header(
    writer: &mut Writer<'_>,
    level: tracing::Level,
    component: &str,
    event: &str,
) -> fmt::Result {
    writer.write_str("timestamp=")?;
    tracing_subscriber::fmt::time::SystemTime.format_time(writer)?;
    write!(
        writer,
        " level={level} pid={} thread_id={:?} component=",
        std::process::id(),
        std::thread::current().id()
    )?;
    atom(writer, component)?;
    writer.write_str(" event=")?;
    atom(writer, event)
}

impl<S> FormatEvent<S, DiagnosticFields> for DiagnosticEvent
where
    S: tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn format_event(
        &self,
        context: &FmtContext<'_, S, DiagnosticFields>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> fmt::Result {
        let dropped = self.dropped.dropped_lines();
        let previous = self.reported_drops.swap(dropped, Ordering::Relaxed);
        if dropped > previous {
            // Queue overload must remain visible even when the warning itself cannot enqueue.
            report_stderr(format_args!(
                "event=diagnostic_queue_dropped error_type=QueueFull dropped_lines={dropped}"
            ));
            header(
                &mut writer,
                tracing::Level::WARN,
                "logging",
                "diagnostic_queue_dropped",
            )?;
            writeln!(
                writer,
                " dropped_lines={dropped} message=\"诊断日志队列记录丢失\""
            )?;
        }
        let mut fields = HeaderVisitor::default();
        event.record(&mut fields);
        header(
            &mut writer,
            *event.metadata().level(),
            fields
                .component
                .as_deref()
                .unwrap_or(event.metadata().target()),
            fields.event.as_deref().unwrap_or("diagnostic"),
        )?;
        if let Some(scope) = context.event_scope() {
            for span in scope.from_root() {
                write!(writer, " span_id={} span=", span.id().into_u64())?;
                quoted(&mut writer, span.name())?;
                if let Some(fields) = span.extensions().get::<FormattedFields<DiagnosticFields>>() {
                    writer.write_str(&fields.fields)?;
                }
            }
        }
        let mut visitor = FieldVisitor {
            writer: &mut writer,
            result: Ok(()),
            skip_header: true,
        };
        event.record(&mut visitor);
        visitor.result?;
        writeln!(writer)
    }
}
