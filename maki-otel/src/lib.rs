//! Opt-in OpenTelemetry export for maki, on the official SDK.
//!
//! Off unless `MAKI_ENABLE_TELEMETRY` (or `telemetry.enabled`) says otherwise,
//! and when it is off every call site costs one relaxed atomic load. When it
//! is on, counters aggregate on the caller's thread and events go to the SDK's
//! batch processor, which drops rather than blocks when its queue is full.
//! Exports run on the SDK's own threads.

mod console;
pub mod emit;
mod export;
pub mod logs;
pub mod metrics;
mod overrides;
pub mod resource;
pub mod settings;

use std::convert::Infallible;
use std::env;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, OnceLock, RwLock};
use std::thread;
use std::time::{Duration, SystemTime};

use maki_config::TelemetryConfig;
use opentelemetry::logs::{LogRecord, Logger, LoggerProvider, Severity};
use opentelemetry::metrics::{Counter, MeterProvider};
use opentelemetry::{KeyValue, StringValue};
use opentelemetry_otlp::ExporterBuildError;
use opentelemetry_sdk::logs::{SdkLogRecord, SdkLogger, SdkLoggerProvider};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use thiserror::Error;

use crate::metrics::Instruments;
use crate::settings::InvalidSetting;

const KEY_SESSION_ID: &str = "session.id";
const KEY_APP_VERSION: &str = "app.version";
const KEY_TERMINAL_TYPE: &str = "terminal.type";
const KEY_EVENT_NAME: &str = "event.name";
const KEY_EVENT_SEQUENCE: &str = "event.sequence";
const TERMINAL_UNKNOWN: &str = "unknown";
const LUA_PREFIX: &str = "telemetry.";
/// Terminal type, session id and version.
const COMMON_METRIC_ATTRS: usize = 3;

static ENABLED: AtomicBool = AtomicBool::new(false);
static HANDLE: OnceLock<Handle> = OnceLock::new();

#[derive(Debug, Error)]
pub enum InitError {
    #[error(transparent)]
    Settings(#[from] InvalidSetting),
    #[error("could not build the OTLP exporter: {0}")]
    Exporter(#[from] ExporterBuildError),
    #[error("telemetry headers are not valid: {0}")]
    Headers(#[from] http::Error),
}

struct Metrics {
    provider: SdkMeterProvider,
    instruments: Instruments,
}

struct Events {
    provider: SdkLoggerProvider,
    logger: SdkLogger,
}

struct Handle {
    metrics: Option<Metrics>,
    events: Option<Events>,
    session_id: RwLock<Option<StringValue>>,
    sequence: AtomicU64,
    terminal_type: StringValue,
    metrics_include_session_id: bool,
    metrics_include_version: bool,
    log_user_prompts: bool,
    log_tool_details: bool,
    content_max_length: usize,
}

#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Returns without doing anything when telemetry is off or has nowhere to
/// send. Call after logging is set up so export errors are recorded.
pub fn init(config: &TelemetryConfig) -> Result<(), InitError> {
    if HANDLE.get().is_some() {
        return Ok(());
    }
    let (config, overridden) = overrides::env_wins(config, export::is_set);
    let Some(settings) = settings::resolve(&config, |key| env::var(key).ok())? else {
        return Ok(());
    };
    if settings.metrics_exporters.is_empty() && settings.logs_exporters.is_empty() {
        return Ok(());
    }

    let (meter_provider, logger_provider) = export::providers(&settings, &config)?;
    let scope = resource::scope();
    let handle = Handle {
        metrics: meter_provider.map(|provider| Metrics {
            instruments: Instruments::new(&provider.meter_with_scope(scope.clone())),
            provider,
        }),
        events: logger_provider.map(|provider| Events {
            logger: provider.logger_with_scope(scope),
            provider,
        }),
        session_id: RwLock::new(None),
        sequence: AtomicU64::new(0),
        terminal_type: shared(&terminal_type()),
        metrics_include_session_id: settings.metrics_include_session_id,
        metrics_include_version: settings.metrics_include_version,
        log_user_prompts: settings.log_user_prompts,
        log_tool_details: settings.log_tool_details,
        content_max_length: settings.content_max_length,
    };
    let (exports_metrics, exports_logs) = (handle.metrics.is_some(), handle.events.is_some());
    if HANDLE.set(handle).is_err() {
        return Ok(());
    }
    ENABLED.store(true, Ordering::Relaxed);
    tracing::info!(
        otel.metrics = exports_metrics,
        otel.logs = exports_logs,
        "telemetry enabled"
    );
    if !overridden.is_empty() {
        let overridden: Vec<_> = overridden
            .iter()
            .map(|over| format!("{LUA_PREFIX}{} by {}", over.key, over.env))
            .collect();
        tracing::info!(
            otel.overridden = ?overridden,
            "init.lua telemetry keys skipped, their environment variables are set"
        );
    }
    Ok(())
}

fn terminal_type() -> String {
    env::var("TERM_PROGRAM")
        .or_else(|_| env::var("TERM"))
        .unwrap_or_else(|_| TERMINAL_UNKNOWN.to_string())
}

/// Reference counted, so attaching it to every emit is a refcount bump.
fn shared(text: &str) -> StringValue {
    StringValue::from(Arc::<str>::from(text))
}

/// Whether tool input is wanted, so callers can skip serializing it.
pub fn logs_tool_details() -> bool {
    handle().is_some_and(|h| h.log_tool_details)
}

/// Attaches the session to everything emitted from now on.
pub fn set_session_id(id: &str) {
    let Some(handle) = HANDLE.get() else {
        return;
    };
    if let Ok(mut slot) = handle.session_id.write() {
        *slot = Some(shared(id));
    }
}

/// Final flush. Safe to call more than once and from a process that is about
/// to `exit`, which skips destructors.
///
/// The providers shut down in parallel on their own threads, because
/// `SdkMeterProvider` ignores the timeout it is given and waits up to 5s on
/// its reader. The caller never waits past `timeout`, and a thread stuck in
/// an export dies with the process.
pub fn shutdown(timeout: Duration) {
    let Some(handle) = HANDLE.get() else {
        return;
    };
    if !ENABLED.swap(false, Ordering::Relaxed) {
        return;
    }
    // Nothing is ever sent: the receiver disconnects once every thread has
    // dropped its sender.
    let (running, finished) = mpsc::channel::<Infallible>();
    if let Some(metrics) = &handle.metrics {
        let running = running.clone();
        thread::spawn(move || {
            let _ = metrics.provider.shutdown_with_timeout(timeout);
            drop(running);
        });
    }
    if let Some(events) = &handle.events {
        let running = running.clone();
        thread::spawn(move || {
            let _ = events.provider.shutdown_with_timeout(timeout);
            drop(running);
        });
    }
    drop(running);
    if let Err(RecvTimeoutError::Timeout) = finished.recv_timeout(timeout) {
        tracing::warn!(
            ?timeout,
            "otel shutdown timed out, the last export may be lost"
        );
    }
}

impl Handle {
    fn session_id(&self) -> Option<StringValue> {
        self.session_id.read().ok().and_then(|s| s.clone())
    }

    /// Session id and version multiply metric cardinality, so they are
    /// opt-in per the OTel env vars.
    fn count<T>(
        &self,
        counter: impl FnOnce(&Instruments) -> &Counter<T>,
        value: T,
        extra: &[KeyValue],
    ) {
        let Some(metrics) = &self.metrics else {
            return;
        };
        let mut attrs = Vec::with_capacity(extra.len() + COMMON_METRIC_ATTRS);
        attrs.push(KeyValue::new(KEY_TERMINAL_TYPE, self.terminal_type.clone()));
        if self.metrics_include_session_id
            && let Some(id) = self.session_id()
        {
            attrs.push(KeyValue::new(KEY_SESSION_ID, id));
        }
        if self.metrics_include_version {
            attrs.push(KeyValue::new(KEY_APP_VERSION, resource::VERSION));
        }
        attrs.extend_from_slice(extra);
        counter(&metrics.instruments).add(value, &attrs);
    }

    /// The name is repeated as an attribute because older collectors drop
    /// the newer OTLP `event_name` field, and the sequence orders events that
    /// share a timestamp. Both go last, and the SDK does not dedupe, so no
    /// caller may use these keys.
    fn event(&self, name: &'static str, fill: impl FnOnce(&mut SdkLogRecord)) {
        let Some(events) = &self.events else {
            return;
        };
        let now = SystemTime::now();
        let mut record = events.logger.create_log_record();
        record.set_event_name(name);
        record.set_timestamp(now);
        record.set_observed_timestamp(now);
        let severity = Severity::Info;
        record.set_severity_number(severity);
        record.set_severity_text(severity.name());
        record.add_attribute(KEY_TERMINAL_TYPE, self.terminal_type.clone());
        record.add_attribute(KEY_APP_VERSION, resource::VERSION);
        if let Some(id) = self.session_id() {
            record.add_attribute(KEY_SESSION_ID, id);
        }
        fill(&mut record);
        record.add_attribute(KEY_EVENT_NAME, name);
        record.add_attribute(
            KEY_EVENT_SEQUENCE,
            saturating_i64(self.sequence.fetch_add(1, Ordering::Relaxed)),
        );
        events.logger.emit(record);
    }

    fn truncate(&self, text: &str) -> String {
        truncate_chars(text, self.content_max_length)
    }
}

/// OTLP integers are signed, and `AnyValue` takes no `u64` or `usize`.
pub(crate) fn saturating_i64(value: impl TryInto<i64>) -> i64 {
    value.try_into().unwrap_or(i64::MAX)
}

/// Cuts on a char boundary, so slicing multi-byte text can never panic.
pub(crate) fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((at, _)) => text[..at].to_string(),
        None => text.to_string(),
    }
}

fn handle() -> Option<&'static Handle> {
    enabled().then(|| HANDLE.get()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_cuts_at_char_boundaries_not_bytes() {
        assert_eq!(truncate_chars("日本語テキスト", 3), "日本語");
        assert_eq!(truncate_chars("héllo", 2), "hé");
        assert_eq!(truncate_chars("short", 10), "short");
    }

    #[test]
    fn emitting_before_init_is_a_no_op() {
        assert!(!enabled());
        emit::commit_created();
        emit::user_prompt("hello");
        shutdown(Duration::from_millis(1));
    }

    #[test]
    fn integers_past_i64_saturate() {
        assert_eq!(saturating_i64(u64::MAX), i64::MAX);
        assert_eq!(saturating_i64(7_u128), 7);
    }
}
