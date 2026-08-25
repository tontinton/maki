//! Opt-in OpenTelemetry export for maki.
//!
//! Off unless `MAKI_ENABLE_TELEMETRY` (or `telemetry.enabled`) says otherwise,
//! and when it is off every call site costs one relaxed atomic load.
//!
//! ```text
//! call sites --try_send--> bounded channels --> pipeline task (smol)
//!                            events   (logs_max_queue_size)
//!                            measurements (METRICS_QUEUE_SIZE)
//!                                                      |
//!                              aggregate + batch ------+--> Transport::export
//! ```

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use flume::Sender;
use maki_config::TelemetryConfig;
use thiserror::Error;

use crate::attr::AttrSet;
use crate::logs::LogRecord;
use crate::metrics::{Measurement, MetricDef, Value};
use crate::pipeline::{Dropped, Exporters, Inputs, Pipeline, Shutdown, now_nanos};
use crate::settings::{Exporter, SettingsError, SignalSettings};
use crate::transport::{ConsoleTransport, OtlpTransport, Transport};

pub mod attr;
pub mod emit;
pub mod encode;
pub mod logs;
pub mod metrics;
pub mod pipeline;
pub mod resource;
pub mod settings;
pub mod transport;

/// Metric points are tiny and aggregated on arrival, so the queue only needs
/// to absorb bursts between ticks.
const METRICS_QUEUE_SIZE: usize = 2048;
const KEY_SESSION_ID: &str = "session.id";
const KEY_APP_VERSION: &str = "app.version";
const KEY_TERMINAL_TYPE: &str = "terminal.type";
const KEY_EVENT_NAME: &str = "event.name";
const KEY_EVENT_SEQUENCE: &str = "event.sequence";
const TERMINAL_UNKNOWN: &str = "unknown";

static ENABLED: AtomicBool = AtomicBool::new(false);
static HANDLE: OnceLock<Handle> = OnceLock::new();

#[derive(Debug, Error)]
pub enum InitError {
    #[error(transparent)]
    Settings(#[from] SettingsError),
    #[error("could not build the OTLP HTTP client: {0}")]
    Client(#[from] isahc::Error),
}

struct Handle {
    measurements: Sender<Measurement>,
    events: Sender<LogRecord>,
    shutdown: Sender<Shutdown>,
    dropped: Arc<Dropped>,
    session_id: RwLock<Option<String>>,
    sequence: AtomicU64,
    terminal_type: String,
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
    let Some(settings) = settings::resolve(config, |key| std::env::var(key).ok())? else {
        return Ok(());
    };
    let exports_metrics = !settings.metrics_exporters.is_empty();
    let exports_logs = !settings.logs_exporters.is_empty();
    if !exports_metrics && !exports_logs {
        return Ok(());
    }

    let exporters = Exporters {
        metrics: build_transports(&settings.metrics_exporters, settings.metrics.as_ref())?,
        logs: build_transports(&settings.logs_exporters, settings.logs.as_ref())?,
    };

    let (measurements, metrics_rx) = flume::bounded(METRICS_QUEUE_SIZE);
    let (events, logs_rx) = flume::bounded(settings.logs_max_queue_size);
    let (shutdown, shutdown_rx) = flume::bounded(1);
    let dropped = Arc::new(Dropped::default());

    let pipeline = Pipeline::new(
        &settings,
        resource::build(&settings),
        exporters,
        Arc::clone(&dropped),
        now_nanos(),
    );
    smol::spawn(pipeline.run(Inputs {
        metrics: metrics_rx,
        logs: logs_rx,
        shutdown: shutdown_rx,
    }))
    .detach();

    let handle = Handle {
        measurements,
        events,
        shutdown,
        dropped,
        session_id: RwLock::new(None),
        sequence: AtomicU64::new(0),
        terminal_type: terminal_type(),
        metrics_include_session_id: settings.metrics_include_session_id,
        metrics_include_version: settings.metrics_include_version,
        log_user_prompts: settings.log_user_prompts,
        log_tool_details: settings.log_tool_details,
        content_max_length: settings.content_max_length,
    };
    if HANDLE.set(handle).is_err() {
        return Ok(());
    }
    ENABLED.store(true, Ordering::Relaxed);
    tracing::info!(
        otel.metrics = exports_metrics,
        otel.logs = exports_logs,
        otel.service_name = settings.service_name,
        "telemetry enabled"
    );
    Ok(())
}

fn build_transports(
    kinds: &[Exporter],
    signal_settings: Option<&SignalSettings>,
) -> Result<Vec<Box<dyn Transport>>, isahc::Error> {
    let mut out: Vec<Box<dyn Transport>> = Vec::with_capacity(kinds.len());
    for kind in kinds {
        match kind {
            Exporter::Console => out.push(Box::new(ConsoleTransport)),
            // Settings resolution guarantees an endpoint whenever otlp is on.
            Exporter::Otlp => {
                let Some(settings) = signal_settings else {
                    continue;
                };
                out.push(Box::new(OtlpTransport::new(settings.clone())?));
            }
        }
    }
    Ok(out)
}

fn terminal_type() -> String {
    std::env::var("TERM_PROGRAM")
        .or_else(|_| std::env::var("TERM"))
        .unwrap_or_else(|_| TERMINAL_UNKNOWN.to_string())
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
        *slot = Some(id.to_string());
    }
}

/// Final flush. Safe to call more than once and from a process that is about
/// to `exit`, which skips destructors.
pub fn shutdown(timeout: Duration) {
    let Some(handle) = HANDLE.get() else {
        return;
    };
    if !ENABLED.swap(false, Ordering::Relaxed) {
        return;
    }
    let (done_tx, done_rx) = flume::bounded(1);
    let request = Shutdown {
        budget: timeout,
        done: done_tx,
    };
    if handle.shutdown.try_send(request).is_ok() {
        let _ = done_rx.recv_timeout(timeout);
    }
}

impl Handle {
    fn session_id(&self) -> Option<String> {
        self.session_id.read().ok().and_then(|s| s.clone())
    }

    /// Session id and version multiply metric cardinality, so they are
    /// opt-in per the OTel env vars.
    fn record(&self, def: &'static MetricDef, value: Value, extra: AttrSet) {
        let mut attrs = AttrSet::new().with(KEY_TERMINAL_TYPE, self.terminal_type.as_str());
        if self.metrics_include_session_id
            && let Some(id) = self.session_id()
        {
            attrs.insert(KEY_SESSION_ID, id);
        }
        if self.metrics_include_version {
            attrs.insert(KEY_APP_VERSION, resource::VERSION);
        }
        attrs.extend_from(&extra);
        send_or_drop(
            &self.measurements,
            Measurement { def, value, attrs },
            &self.dropped.metrics,
        );
    }

    /// The name is repeated as an attribute because older collectors drop
    /// the newer OTLP `event_name` field; the sequence orders events that
    /// share a timestamp.
    fn event(&self, name: &'static str, extra: AttrSet) {
        let mut attrs = AttrSet::new()
            .with(KEY_TERMINAL_TYPE, self.terminal_type.as_str())
            .with(KEY_APP_VERSION, resource::VERSION)
            .with_opt(KEY_SESSION_ID, self.session_id());
        attrs.extend_from(&extra);
        attrs.insert(KEY_EVENT_NAME, name);
        attrs.insert(
            KEY_EVENT_SEQUENCE,
            self.sequence.fetch_add(1, Ordering::Relaxed),
        );
        send_or_drop(
            &self.events,
            LogRecord {
                time_unix_nano: now_nanos(),
                event_name: name,
                attrs,
            },
            &self.dropped.logs,
        );
    }

    fn truncate(&self, text: &str) -> String {
        truncate_chars(text, self.content_max_length)
    }
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

/// A slow collector must never stall a turn, so a full queue costs the item
/// and a counter instead of the caller's time.
fn send_or_drop<T>(queue: &Sender<T>, item: T, dropped: &AtomicU64) {
    if queue.try_send(item).is_err() {
        dropped.fetch_add(1, Ordering::Relaxed);
    }
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
    fn a_full_queue_drops_and_counts_instead_of_blocking() {
        let (tx, rx) = flume::bounded(1);
        let dropped = AtomicU64::new(0);
        send_or_drop(&tx, 1, &dropped);
        send_or_drop(&tx, 2, &dropped);
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        assert_eq!(rx.try_recv(), Ok(1));
    }
}
