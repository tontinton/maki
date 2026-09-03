//! The privacy defaults, end to end: with telemetry on and nothing opted in,
//! prompt text and tool input must never reach the collector, while the
//! session id set at start must reach every event. Owns the process-wide
//! state in `maki_otel`, so it lives in its own test binary.

use std::time::Duration;

use maki_config::TelemetryConfig;
use maki_otel::emit::{self, ToolResult};
use maki_otel::logs::{EVENT_TOOL_RESULT, EVENT_USER_PROMPT};
use serde_json::Value;

mod support;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const LOGS_PATH: &str = "/v1/logs";
const EXPORTER_OTLP: &str = "otlp";
const PROTOCOL_HTTP_JSON: &str = "http/json";
/// Keeps the interval timer out of the test; the one export comes from
/// shutdown.
const NEVER_MS: u64 = 3_600_000;
const SESSION_ID: &str = "sess-privacy";
/// Multi-byte on purpose: the exported length must count chars, not bytes.
const SECRET_PROMPT: &str = "the 秘密 prompt";
const SECRET_INPUT: &str = "cat /etc/shadow";

fn find<'a>(records: &'a [Value], event: &str) -> &'a Value {
    records
        .iter()
        .find(|r| r["eventName"] == event)
        .unwrap_or_else(|| panic!("missing {event} event"))
}

fn attr<'a>(record: &'a Value, key: &str) -> Option<&'a Value> {
    record["attributes"]
        .as_array()
        .expect("attributes")
        .iter()
        .find(|kv| kv["key"] == key)
        .map(|kv| &kv["value"])
}

#[test]
fn prompt_text_and_tool_input_stay_home_by_default() {
    let (endpoint, server) = support::serve_once();
    let config = TelemetryConfig {
        enabled: Some(true),
        logs_exporter: Some(EXPORTER_OTLP.to_string()),
        protocol: Some(PROTOCOL_HTTP_JSON.to_string()),
        endpoint: Some(endpoint),
        logs_interval_ms: Some(NEVER_MS),
        ..TelemetryConfig::default()
    };
    maki_otel::init_with_env(&config, support::no_env).expect("telemetry should start");
    assert!(
        !maki_otel::logs_tool_details(),
        "tool details must be opt-in"
    );

    emit::session_started(emit::START_FRESH, Some(SESSION_ID));
    emit::user_prompt(SECRET_PROMPT);
    emit::tool_result(&ToolResult {
        tool_name: "bash",
        tool_source: "builtin",
        success: true,
        duration: Duration::from_millis(5),
        error_type: None,
        tool_input: Some(SECRET_INPUT),
    });
    maki_otel::shutdown(SHUTDOWN_TIMEOUT);

    let request = server.join().expect("server thread");
    assert_eq!(request.target, LOGS_PATH);

    // The strongest check first: the secret bytes are nowhere in the payload.
    let text = String::from_utf8(request.body.clone()).expect("utf-8 OTLP/JSON");
    assert!(!text.contains("秘密"), "prompt text leaked: {text}");
    assert!(!text.contains(SECRET_INPUT), "tool input leaked: {text}");

    let body: Value = serde_json::from_slice(&request.body).expect("valid OTLP/JSON");
    let records = body["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
        .as_array()
        .expect("log records");

    let prompt = find(records, EVENT_USER_PROMPT);
    assert_eq!(
        attr(prompt, "prompt_length").expect("length is always attached")["intValue"],
        SECRET_PROMPT.chars().count().to_string()
    );
    assert!(attr(prompt, "prompt").is_none());

    let tool = find(records, EVENT_TOOL_RESULT);
    assert!(attr(tool, "tool_input").is_none());

    for record in records {
        assert_eq!(
            attr(record, "session.id").expect("every event carries the session")["stringValue"],
            SESSION_ID
        );
    }
}
