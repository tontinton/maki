//! The background task. Call sites only ever `try_send` into a bounded
//! channel; everything else (aggregation, batching, exporting, retrying)
//! happens here, off the hot path.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use flume::{Receiver, Sender};
use futures_lite::{StreamExt, future, stream};

use crate::attr::AttrSet;
use crate::encode::otlp::{LogsPayload, MetricsPayload};
use crate::logs::LogRecord;
use crate::metrics::{Measurement, Registry};
use crate::settings::Settings;
use crate::transport::{Payload, Transport};

/// Asks the pipeline to export everything and stop. The pipeline answers on
/// the enclosed channel once the last batch has left.
pub struct Shutdown {
    /// The caller stops waiting after this, so the final flush must too.
    pub budget: Duration,
    pub done: Sender<()>,
}

/// Counts what never made it into the pipeline. Reported once per metrics
/// interval so a saturated queue is visible without spamming the log.
#[derive(Default)]
pub struct Dropped {
    pub metrics: AtomicU64,
    pub logs: AtomicU64,
}

impl Dropped {
    fn take(&self) -> (u64, u64) {
        (
            self.metrics.swap(0, Ordering::Relaxed),
            self.logs.swap(0, Ordering::Relaxed),
        )
    }
}

pub struct Exporters {
    pub metrics: Vec<Box<dyn Transport>>,
    pub logs: Vec<Box<dyn Transport>>,
}

pub struct Inputs {
    pub metrics: Receiver<Measurement>,
    pub logs: Receiver<LogRecord>,
    pub shutdown: Receiver<Shutdown>,
}

enum Event {
    Measurement(Measurement),
    Log(LogRecord),
    Shutdown(Shutdown),
    Tick,
}

pub struct Pipeline {
    resource: AttrSet,
    registry: Registry,
    pending: Vec<LogRecord>,
    exporters: Exporters,
    dropped: Arc<Dropped>,
    metrics_interval: Duration,
    logs_interval: Duration,
    logs_batch_size: usize,
    metrics_export_timeout: Duration,
    logs_export_timeout: Duration,
}

impl Pipeline {
    pub fn new(
        settings: &Settings,
        resource: AttrSet,
        exporters: Exporters,
        dropped: Arc<Dropped>,
        start_nanos: u64,
    ) -> Self {
        Self {
            resource,
            registry: Registry::new(settings.temporality, start_nanos),
            pending: Vec::with_capacity(settings.logs_max_export_batch_size),
            exporters,
            dropped,
            metrics_interval: settings.metrics_interval,
            logs_interval: settings.logs_interval,
            logs_batch_size: settings.logs_max_export_batch_size,
            metrics_export_timeout: settings.metrics_export_timeout,
            logs_export_timeout: settings.logs_export_timeout,
        }
    }

    /// Runs until every producer is gone or a shutdown arrives. Telemetry is
    /// polled before shutdown, so by the time a shutdown is seen both queues
    /// are already empty and the final export cannot miss anything.
    pub async fn run(mut self, inputs: Inputs) {
        let mut events = stream::or(
            stream::or(
                inputs.logs.into_stream().map(Event::Log),
                inputs.metrics.into_stream().map(Event::Measurement),
            ),
            inputs.shutdown.into_stream().map(Event::Shutdown),
        );

        let mut metrics_at = Instant::now() + self.metrics_interval;
        let mut logs_at = Instant::now() + self.logs_interval;

        loop {
            let deadline = metrics_at.min(logs_at);
            let event = future::or(events.next(), async {
                smol::Timer::at(deadline).await;
                Some(Event::Tick)
            })
            .await;

            match event {
                None => break,
                Some(Event::Measurement(m)) => self.registry.record(m),
                Some(Event::Log(record)) => {
                    self.pending.push(record);
                    if self.pending.len() >= self.logs_batch_size {
                        self.export_logs(Instant::now() + self.logs_export_timeout)
                            .await;
                        logs_at = Instant::now() + self.logs_interval;
                    }
                }
                Some(Event::Tick) => {
                    let now = Instant::now();
                    if now >= metrics_at {
                        self.report_drops();
                        self.export_metrics(now + self.metrics_export_timeout).await;
                        metrics_at = now + self.metrics_interval;
                    }
                    if now >= logs_at {
                        self.export_logs(now + self.logs_export_timeout).await;
                        logs_at = now + self.logs_interval;
                    }
                }
                Some(Event::Shutdown(request)) => {
                    self.flush(request.budget).await;
                    let _ = request.done.send(());
                    return;
                }
            }
        }
        self.flush(self.metrics_export_timeout.max(self.logs_export_timeout))
            .await;
    }

    /// One deadline for the whole flush: whoever is waiting gave us `budget`,
    /// and a per exporter timeout would outlive it several times over.
    async fn flush(&mut self, budget: Duration) {
        let deadline = Instant::now() + budget;
        self.report_drops();
        self.export_metrics(deadline).await;
        self.export_logs(deadline).await;
    }

    fn report_drops(&self) {
        let (metrics, logs) = self.dropped.take();
        if metrics > 0 || logs > 0 {
            tracing::warn!(
                otel.dropped_measurements = metrics,
                otel.dropped_events = logs,
                "otel queue full, telemetry dropped"
            );
        }
    }

    async fn export_metrics(&mut self, deadline: Instant) {
        if self.exporters.metrics.is_empty() {
            self.registry.clear();
            return;
        }
        if self.registry.is_empty() {
            return;
        }
        let metrics = self.registry.collect(now_nanos());
        let payload = Payload::Metrics(MetricsPayload {
            resource: &self.resource,
            temporality: self.registry.temporality(),
            metrics: &metrics,
        });
        send_all(&self.exporters.metrics, &payload, deadline).await;
    }

    async fn export_logs(&mut self, deadline: Instant) {
        if self.exporters.logs.is_empty() || self.pending.is_empty() {
            self.pending.clear();
            return;
        }
        let pending = std::mem::take(&mut self.pending);
        for chunk in pending.chunks(self.logs_batch_size) {
            let payload = Payload::Logs(LogsPayload {
                resource: &self.resource,
                records: chunk,
            });
            send_all(&self.exporters.logs, &payload, deadline).await;
        }
    }
}

/// Failures are logged and swallowed: telemetry must never reach the user.
async fn send_all(exporters: &[Box<dyn Transport>], payload: &Payload<'_>, deadline: Instant) {
    for exporter in exporters {
        let remaining = deadline.saturating_duration_since(Instant::now());
        // The transport honours the deadline itself; the race is a backstop
        // against one that hangs, so it can never wedge the pipeline.
        let result = future::or(exporter.export(payload, deadline), async {
            smol::Timer::at(deadline).await;
            Err(crate::transport::ExportError::Timeout(remaining))
        })
        .await;
        if let Err(error) = result {
            tracing::warn!(%error, "otel export failed");
        }
    }
}

pub fn now_nanos() -> u64 {
    jiff::Timestamp::now().as_nanosecond().max(0) as u64
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::thread::JoinHandle;

    use maki_config::TelemetryConfig;

    use super::*;
    use crate::attr::AttrSet;
    use crate::logs::{EVENT_API_REQUEST, LogRecord};
    use crate::metrics::{COMMIT_COUNT, Measurement, Value};
    use crate::settings::{ENV_ENABLE, resolve};
    use crate::transport::{ExportError, ExportFuture};

    /// Long enough that no test is driven by a timer; every export comes
    /// from an explicit shutdown or the senders going away.
    const NEVER: Duration = Duration::from_secs(3600);
    const SHUTDOWN_BUDGET: Duration = Duration::from_secs(2);
    const BATCH: usize = 2;
    const CAPACITY: usize = 16;
    const FAIL_STATUS: u16 = 500;

    #[derive(Default)]
    struct Recorder {
        metric_batches: Mutex<Vec<usize>>,
        log_batches: Mutex<Vec<usize>>,
    }

    struct Fake(Arc<Recorder>);

    impl Transport for Fake {
        fn export<'a>(&'a self, payload: &'a Payload<'a>, _deadline: Instant) -> ExportFuture<'a> {
            match payload {
                Payload::Metrics(p) => self
                    .0
                    .metric_batches
                    .lock()
                    .unwrap()
                    .push(p.metrics.iter().map(|m| m.points.len()).sum()),
                Payload::Logs(p) => self.0.log_batches.lock().unwrap().push(p.records.len()),
            }
            Box::pin(async { Ok(()) })
        }
    }

    struct Failing;

    impl Transport for Failing {
        fn export<'a>(&'a self, _payload: &'a Payload<'a>, _deadline: Instant) -> ExportFuture<'a> {
            Box::pin(async {
                Err(ExportError::Http {
                    status: FAIL_STATUS,
                    message: String::new(),
                })
            })
        }
    }

    /// The pipeline runs on its own thread and every step is confirmed by an
    /// acknowledgement, so the tests are ordered without a single sleep.
    struct Harness {
        metrics_tx: Sender<Measurement>,
        logs_tx: Sender<LogRecord>,
        shutdown_tx: Sender<Shutdown>,
        recorder: Arc<Recorder>,
        thread: Option<JoinHandle<()>>,
    }

    impl Harness {
        fn start() -> Self {
            Self::start_with(|recorder| Exporters {
                metrics: vec![Box::new(Fake(Arc::clone(recorder)))],
                logs: vec![Box::new(Fake(Arc::clone(recorder)))],
            })
        }

        fn start_with(exporters: impl FnOnce(&Arc<Recorder>) -> Exporters) -> Self {
            let mut settings = resolve(&TelemetryConfig::default(), |k| {
                (k == ENV_ENABLE).then(|| "1".to_string())
            })
            .unwrap()
            .unwrap();
            settings.metrics_interval = NEVER;
            settings.logs_interval = NEVER;
            settings.logs_max_export_batch_size = BATCH;

            let recorder = Arc::new(Recorder::default());
            let (metrics_tx, metrics) = flume::bounded(CAPACITY);
            let (logs_tx, logs) = flume::bounded(CAPACITY);
            let (shutdown_tx, shutdown) = flume::bounded(CAPACITY);
            let pipeline = Pipeline::new(
                &settings,
                AttrSet::new(),
                exporters(&recorder),
                Arc::new(Dropped::default()),
                0,
            );
            let thread = std::thread::spawn(move || {
                smol::block_on(pipeline.run(Inputs {
                    metrics,
                    logs,
                    shutdown,
                }));
            });
            Self {
                metrics_tx,
                logs_tx,
                shutdown_tx,
                recorder,
                thread: Some(thread),
            }
        }

        fn log(&self) {
            self.logs_tx
                .send(LogRecord {
                    time_unix_nano: 1,
                    event_name: EVENT_API_REQUEST,
                    attrs: AttrSet::new(),
                })
                .unwrap();
        }

        fn commit(&self) {
            self.metrics_tx
                .send(Measurement {
                    def: &COMMIT_COUNT,
                    value: Value::Int(1),
                    attrs: AttrSet::new(),
                })
                .unwrap();
        }

        fn shutdown(&mut self) {
            let (done_tx, done_rx) = flume::bounded(1);
            self.shutdown_tx
                .send(Shutdown {
                    budget: SHUTDOWN_BUDGET,
                    done: done_tx,
                })
                .unwrap();
            done_rx.recv().expect("shutdown should be acknowledged");
            self.thread.take().unwrap().join().unwrap();
        }

        /// Drops every sender, which is the other way the task ends.
        fn close(mut self) -> Arc<Recorder> {
            let thread = self.thread.take().expect("pipeline thread");
            let recorder = Arc::clone(&self.recorder);
            drop(self);
            thread.join().unwrap();
            recorder
        }

        fn log_batches(&self) -> Vec<usize> {
            self.recorder.log_batches.lock().unwrap().clone()
        }

        fn metric_batches(&self) -> Vec<usize> {
            self.recorder.metric_batches.lock().unwrap().clone()
        }
    }

    #[test]
    fn shutdown_flushes_pending_events() {
        let mut h = Harness::start();
        h.log();
        h.shutdown();
        assert_eq!(h.log_batches(), vec![1]);
    }

    #[test]
    fn events_split_at_the_batch_size() {
        let mut h = Harness::start();
        for _ in 0..5 {
            h.log();
        }
        h.shutdown();
        assert_eq!(h.log_batches(), vec![BATCH, BATCH, 1]);
    }

    #[test]
    fn measurements_with_equal_attributes_export_as_one_point() {
        let mut h = Harness::start();
        for _ in 0..3 {
            h.commit();
        }
        h.shutdown();
        assert_eq!(h.metric_batches(), vec![1]);
    }

    #[test]
    fn nothing_is_exported_when_nothing_was_recorded() {
        let mut h = Harness::start();
        h.shutdown();
        assert!(h.metric_batches().is_empty());
        assert!(h.log_batches().is_empty());
    }

    #[test]
    fn a_shutdown_never_overtakes_queued_telemetry() {
        let mut h = Harness::start();
        for _ in 0..3 {
            h.log();
            h.commit();
        }
        h.shutdown();
        assert_eq!(h.log_batches(), vec![BATCH, 1]);
        assert_eq!(h.metric_batches(), vec![1]);
    }

    #[test]
    fn dropping_every_producer_flushes_and_stops_the_task() {
        let h = Harness::start();
        h.log();
        let recorder = h.close();
        assert_eq!(*recorder.log_batches.lock().unwrap(), vec![1]);
    }

    #[test]
    fn a_failing_exporter_does_not_starve_the_next_one() {
        let mut h = Harness::start_with(|recorder| Exporters {
            metrics: vec![Box::new(Failing), Box::new(Fake(Arc::clone(recorder)))],
            logs: vec![Box::new(Failing), Box::new(Fake(Arc::clone(recorder)))],
        });
        h.log();
        h.commit();
        h.shutdown();
        assert_eq!(h.metric_batches(), vec![1]);
        assert_eq!(h.log_batches(), vec![1]);
    }
}
