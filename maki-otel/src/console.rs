//! The console exporter: compact OTLP/JSON into the maki log, never stdout,
//! since stdout belongs to the TUI or the ACP stream.

use std::time::Duration;

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::transform::common::tonic::ResourceAttributesWithSchema;
use opentelemetry_proto::transform::logs::tonic::group_logs_by_resource_and_scope;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{LogBatch, LogExporter};
use opentelemetry_sdk::metrics::Temporality;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;

const SIGNAL_METRICS: &str = "metrics";
const SIGNAL_LOGS: &str = "logs";

pub(crate) struct ConsoleMetrics(pub(crate) Temporality);

impl PushMetricExporter for ConsoleMetrics {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        write(
            SIGNAL_METRICS,
            serde_json::to_string(&ExportMetricsServiceRequest::from(metrics)),
        );
        Ok(())
    }

    fn force_flush(&self) -> OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        Ok(())
    }

    fn temporality(&self) -> Temporality {
        self.0
    }
}

#[derive(Debug, Default)]
pub(crate) struct ConsoleLogs(ResourceAttributesWithSchema);

impl LogExporter for ConsoleLogs {
    async fn export(&self, batch: LogBatch<'_>) -> OTelSdkResult {
        let request = ExportLogsServiceRequest {
            resource_logs: group_logs_by_resource_and_scope(&batch, &self.0),
        };
        write(SIGNAL_LOGS, serde_json::to_string(&request));
        Ok(())
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.0 = resource.into();
    }
}

fn write(signal: &'static str, payload: serde_json::Result<String>) {
    match payload {
        Ok(payload) => {
            tracing::info!(
                otel.signal = signal,
                otel.payload = payload,
                "otel console export"
            )
        }
        Err(error) => tracing::warn!(otel.signal = signal, %error, "otel console export failed"),
    }
}
