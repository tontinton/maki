//! One test that walks the whole path: `init`, an emit, `shutdown`, and a real
//! socket on the other end. It owns the process-wide state in `maki_otel`, so
//! it lives in its own test binary.

use std::time::Duration;

use maki_config::TelemetryConfig;
use maki_otel::metrics::SESSION_COUNT;
use serde_json::Value;

mod support;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const METRICS_PATH: &str = "/v1/metrics";
const EXPORTER_OTLP: &str = "otlp";
const PROTOCOL_HTTP_JSON: &str = "http/json";
const SERVICE_NAME: &str = "maki-test";

#[test]
fn a_session_count_reaches_the_collector() {
    let (endpoint, server) = support::serve_once();
    let config = TelemetryConfig {
        enabled: Some(true),
        metrics_exporter: Some(EXPORTER_OTLP.to_string()),
        protocol: Some(PROTOCOL_HTTP_JSON.to_string()),
        endpoint: Some(endpoint),
        service_name: Some(SERVICE_NAME.to_string()),
        ..TelemetryConfig::default()
    };

    maki_otel::init_with_env(&config, support::no_env).expect("telemetry should start");
    assert!(maki_otel::enabled());

    maki_otel::emit::session_started(maki_otel::emit::START_FRESH, None);
    maki_otel::shutdown(SHUTDOWN_TIMEOUT);
    assert!(!maki_otel::enabled(), "shutdown should flip the fast path");

    let request = server.join().expect("server thread");
    assert_eq!(request.target, METRICS_PATH);

    let body: Value = serde_json::from_slice(&request.body).expect("valid OTLP/JSON");
    let scope = &body["resourceMetrics"][0]["scopeMetrics"][0];
    assert_eq!(scope["metrics"][0]["name"], SESSION_COUNT.name);
    assert_eq!(scope["metrics"][0]["sum"]["dataPoints"][0]["asInt"], "1");

    let service = body["resourceMetrics"][0]["resource"]["attributes"]
        .as_array()
        .expect("resource attributes")
        .iter()
        .find(|kv| kv["key"] == "service.name")
        .expect("service.name is always exported");
    assert_eq!(service["value"]["stringValue"], SERVICE_NAME);
}
