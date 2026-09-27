//! Structured logging.
//!
//! JSON is the standard format (specification §27.1). Every record carries the
//! always-on fields of `observability-and-config.md` §2.2 — `timestamp`,
//! `level`, `service`, `version`, `event` — and any correlation fields the call
//! site adds (`request_id`, `connection_id`, `duration_ms`, `error_code`, …).
//!
//! Secret hygiene is a call site responsibility: passwords, tokens, cookies,
//! authorization headers and full payloads must never be passed as fields
//! (§2.3).

use std::io;
#[cfg(test)]
use std::sync::{Arc, Mutex};

use orbisync_config::{LogFormat, ObservabilityConfig};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Registry, fmt};

/// Failure while installing the telemetry backend.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ObservabilityError {
    /// A global subscriber was already installed.
    #[error("a global tracing subscriber is already installed")]
    AlreadyInitialised,
}

/// Installs the global logging subscriber.
///
/// `service` and `version` are emitted on every record; `version` is the build
/// version reported by `/version` as well.
///
/// # Errors
///
/// Returns [`ObservabilityError::AlreadyInitialised`] when a global subscriber
/// is already installed, which happens if the composition root is run twice in
/// one process.
pub fn init_logging(
    config: &ObservabilityConfig,
    service: &'static str,
    version: &'static str,
) -> Result<(), ObservabilityError> {
    let subscriber = build_subscriber(config, service, version, io::stdout);
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|_| ObservabilityError::AlreadyInitialised)
}

fn build_subscriber<W>(
    config: &ObservabilityConfig,
    service: &'static str,
    version: &'static str,
    writer: W,
) -> Box<dyn Subscriber + Send + Sync>
where
    W: for<'writer> fmt::MakeWriter<'writer> + Send + Sync + 'static,
{
    let filter = EnvFilter::new(config.log_level.as_str());
    match config.log_format {
        LogFormat::Json => Box::new(
            Registry::default().with(filter).with(
                fmt::layer()
                    .event_format(OrbisyncJson { service, version })
                    .with_writer(writer),
            ),
        ),
        LogFormat::Pretty => Box::new(
            Registry::default()
                .with(filter)
                .with(fmt::layer().pretty().with_writer(writer)),
        ),
    }
}

/// Event formatter emitting one JSON object per record.
struct OrbisyncJson {
    service: &'static str,
    version: &'static str,
}

impl<S, N> FormatEvent<S, N> for OrbisyncJson
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn format_event(
        &self,
        _context: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> core::fmt::Result {
        let metadata = event.metadata();
        let mut record = serde_json::Map::new();
        record.insert(
            "timestamp".to_owned(),
            serde_json::Value::String(
                OffsetDateTime::now_utc()
                    .format(&Rfc3339)
                    .unwrap_or_else(|_| String::from("1970-01-01T00:00:00Z")),
            ),
        );
        record.insert(
            "level".to_owned(),
            serde_json::Value::String(metadata.level().as_str().to_lowercase()),
        );
        record.insert(
            "service".to_owned(),
            serde_json::Value::String(self.service.to_owned()),
        );
        record.insert(
            "version".to_owned(),
            serde_json::Value::String(self.version.to_owned()),
        );
        record.insert(
            "target".to_owned(),
            serde_json::Value::String(metadata.target().to_owned()),
        );

        let mut visitor = JsonVisitor {
            fields: &mut record,
        };
        event.record(&mut visitor);

        // `event` is an always-on field. When the call site does not name the
        // event explicitly, the callsite target is the stable fallback.
        if !record.contains_key("event") {
            let fallback = serde_json::Value::String(metadata.target().to_owned());
            record.insert("event".to_owned(), fallback);
        }

        let line = serde_json::Value::Object(record).to_string();
        writeln!(writer, "{line}")
    }
}

struct JsonVisitor<'fields> {
    fields: &'fields mut serde_json::Map<String, serde_json::Value>,
}

impl JsonVisitor<'_> {
    fn insert(&mut self, field: &Field, value: serde_json::Value) {
        self.fields.insert(field.name().to_owned(), value);
    }
}

impl Visit for JsonVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.insert(field, serde_json::Value::String(value.to_owned()));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.insert(field, serde_json::Value::Bool(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.insert(field, serde_json::Value::Number(value.into()));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.insert(field, serde_json::Value::Number(value.into()));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        match serde_json::Number::from_f64(value) {
            Some(number) => self.insert(field, serde_json::Value::Number(number)),
            None => self.insert(field, serde_json::Value::Null),
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn core::fmt::Debug) {
        self.insert(field, serde_json::Value::String(format!("{value:?}")));
    }
}

/// Writer that appends to a shared buffer, used by the logging tests.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

#[cfg(test)]
impl io::Write for SharedBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.0.lock() {
            Ok(mut guard) => {
                guard.extend_from_slice(buf);
                Ok(buf.len())
            }
            Err(_) => Err(io::Error::other("log buffer poisoned")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
impl<'writer> fmt::MakeWriter<'writer> for SharedBuffer {
    type Writer = Self;

    fn make_writer(&'writer self) -> Self::Writer {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{OrbisyncJson, SharedBuffer, build_subscriber};
    use orbisync_config::{LogFormat, LogLevel, ObservabilityConfig};

    fn capture(config: &ObservabilityConfig, emit: impl FnOnce()) -> String {
        let buffer = SharedBuffer::default();
        let subscriber = build_subscriber(config, "orbisync", "0.1.0-test", buffer.clone());
        tracing::subscriber::with_default(subscriber, emit);
        let bytes = buffer.0.lock().expect("buffer is not poisoned").clone();
        String::from_utf8(bytes).expect("log output is UTF-8")
    }

    fn json_config(level: LogLevel) -> ObservabilityConfig {
        ObservabilityConfig {
            log_format: LogFormat::Json,
            log_level: level,
            audit_retention_days: 365,
        }
    }

    #[test]
    fn test_json_record_contains_the_always_on_fields() {
        let output = capture(&json_config(LogLevel::Info), || {
            tracing::info!(event = "connection.established", connection_id = "abc");
        });
        let line = output.lines().next().expect("one record was written");
        let value: serde_json::Value = serde_json::from_str(line).expect("record is valid JSON");

        assert_eq!(value["service"], "orbisync");
        assert_eq!(value["version"], "0.1.0-test");
        assert_eq!(value["level"], "info");
        assert_eq!(value["event"], "connection.established");
        assert_eq!(value["connection_id"], "abc");
        assert!(
            value["timestamp"]
                .as_str()
                .is_some_and(|stamp| stamp.ends_with('Z'))
        );
    }

    #[test]
    fn test_event_field_falls_back_to_the_callsite_target() {
        let output = capture(&json_config(LogLevel::Info), || {
            tracing::info!(request_id = "r-1", "started");
        });
        let value: serde_json::Value =
            serde_json::from_str(output.lines().next().expect("record")).expect("valid JSON");
        assert_eq!(value["event"], value["target"]);
        assert_eq!(value["request_id"], "r-1");
    }

    #[test]
    fn test_level_filter_suppresses_lower_severities() {
        let output = capture(&json_config(LogLevel::Warn), || {
            tracing::info!(event = "ignored");
            tracing::warn!(event = "kept");
        });
        assert!(!output.contains("ignored"));
        assert!(output.contains("kept"));
    }

    #[test]
    fn test_formatter_is_constructible_for_both_formats() {
        let mut config = json_config(LogLevel::Info);
        config.log_format = LogFormat::Pretty;
        let pretty = capture(&config, || tracing::info!(event = "pretty.mode"));
        assert!(pretty.contains("pretty.mode"));

        let formatter = OrbisyncJson {
            service: "orbisync",
            version: "0.0.0",
        };
        assert_eq!(formatter.service, "orbisync");
    }
}
