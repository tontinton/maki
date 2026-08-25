//! OTLP/JSON: the same messages as protobuf with camelCase names, 64-bit
//! integers as strings, and enums as numbers.

use serde_json::{Map, Value as Json, json};

use crate::attr::{AttrSet, AttrValue};
use crate::encode::otlp::{LogsPayload, MetricsPayload, temporality_code};
use crate::logs::{SEVERITY_INFO, SEVERITY_TEXT_INFO};
use crate::metrics::Value;
use crate::resource::{SCOPE_NAME, VERSION};

/// `serde_json` turns a non-finite double into `null`, which a collector
/// rejects for the whole batch. Proto3 JSON spells them out instead.
fn double(v: f64) -> Json {
    if v.is_finite() {
        json!(v)
    } else if v.is_nan() {
        Json::String("NaN".into())
    } else if v.is_sign_positive() {
        Json::String("Infinity".into())
    } else {
        Json::String("-Infinity".into())
    }
}

fn any_value(value: &AttrValue) -> Json {
    match value {
        AttrValue::Str(v) => json!({ "stringValue": v }),
        AttrValue::Bool(v) => json!({ "boolValue": v }),
        AttrValue::Int(v) => json!({ "intValue": v.to_string() }),
        AttrValue::Double(v) => json!({ "doubleValue": double(*v) }),
    }
}

fn attributes(attrs: &AttrSet) -> Json {
    Json::Array(
        attrs
            .iter()
            .map(|(key, value)| json!({ "key": key, "value": any_value(value) }))
            .collect(),
    )
}

fn scope() -> Json {
    json!({ "name": SCOPE_NAME, "version": VERSION })
}

fn resource(attrs: &AttrSet) -> Json {
    json!({ "attributes": attributes(attrs) })
}

pub fn encode_metrics(payload: &MetricsPayload<'_>) -> Vec<u8> {
    let metrics: Vec<Json> = payload
        .metrics
        .iter()
        .map(|metric| {
            let points: Vec<Json> = metric
                .points
                .iter()
                .map(|point| {
                    let mut obj = Map::new();
                    obj.insert("attributes".into(), attributes(&point.attrs));
                    obj.insert(
                        "startTimeUnixNano".into(),
                        Json::String(point.start_time_unix_nano.to_string()),
                    );
                    obj.insert(
                        "timeUnixNano".into(),
                        Json::String(point.time_unix_nano.to_string()),
                    );
                    match point.value {
                        Value::Int(v) => obj.insert("asInt".into(), Json::String(v.to_string())),
                        Value::Double(v) => obj.insert("asDouble".into(), double(v)),
                    };
                    Json::Object(obj)
                })
                .collect();
            json!({
                "name": metric.def.name,
                "description": metric.def.description,
                "unit": metric.def.unit,
                "sum": {
                    "dataPoints": points,
                    "aggregationTemporality": temporality_code(payload.temporality),
                    "isMonotonic": true,
                },
            })
        })
        .collect();

    let body = json!({
        "resourceMetrics": [{
            "resource": resource(payload.resource),
            "scopeMetrics": [{ "scope": scope(), "metrics": metrics }],
        }],
    });
    serde_json::to_vec(&body).expect("serializing owned json cannot fail")
}

pub fn encode_logs(payload: &LogsPayload<'_>) -> Vec<u8> {
    let records: Vec<Json> = payload
        .records
        .iter()
        .map(|record| {
            let time = record.time_unix_nano.to_string();
            json!({
                "timeUnixNano": time,
                "observedTimeUnixNano": time,
                "severityNumber": SEVERITY_INFO,
                "severityText": SEVERITY_TEXT_INFO,
                "eventName": record.event_name,
                "attributes": attributes(&record.attrs),
            })
        })
        .collect();

    let body = json!({
        "resourceLogs": [{
            "resource": resource(payload.resource),
            "scopeLogs": [{ "scope": scope(), "logRecords": records }],
        }],
    });
    serde_json::to_vec(&body).expect("serializing owned json cannot fail")
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;
    use crate::logs::{EVENT_API_REQUEST, LogRecord};
    use crate::metrics::{COST_USAGE, DataPoint, MetricData, TOKEN_USAGE};
    use crate::settings::Temporality;

    const START: u64 = 1;
    const TIME: u64 = 2;
    const BIG: i64 = 9_007_199_254_740_993;

    fn parse(bytes: Vec<u8>) -> Json {
        serde_json::from_slice(&bytes).expect("encoder should emit valid json")
    }

    #[test]
    fn metrics_request_matches_the_otlp_json_shape() {
        let resource_attrs = AttrSet::new().with("service.name", "maki");
        let metrics = vec![MetricData {
            def: &TOKEN_USAGE,
            points: vec![DataPoint {
                attrs: AttrSet::new().with("type", "input"),
                start_time_unix_nano: START,
                time_unix_nano: TIME,
                value: Value::Int(BIG),
            }],
        }];
        let got = parse(encode_metrics(&MetricsPayload {
            resource: &resource_attrs,
            temporality: Temporality::Delta,
            metrics: &metrics,
        }));
        assert_eq!(
            got,
            json!({
                "resourceMetrics": [{
                    "resource": {
                        "attributes": [
                            { "key": "service.name", "value": { "stringValue": "maki" } }
                        ]
                    },
                    "scopeMetrics": [{
                        "scope": { "name": SCOPE_NAME, "version": VERSION },
                        "metrics": [{
                            "name": TOKEN_USAGE.name,
                            "description": TOKEN_USAGE.description,
                            "unit": TOKEN_USAGE.unit,
                            "sum": {
                                "dataPoints": [{
                                    "attributes": [
                                        { "key": "type", "value": { "stringValue": "input" } }
                                    ],
                                    "startTimeUnixNano": "1",
                                    "timeUnixNano": "2",
                                    "asInt": "9007199254740993"
                                }],
                                "aggregationTemporality": 1,
                                "isMonotonic": true
                            }
                        }]
                    }]
                }]
            })
        );
    }

    #[test]
    fn doubles_stay_numbers() {
        let resource_attrs = AttrSet::new();
        let metrics = vec![MetricData {
            def: &COST_USAGE,
            points: vec![DataPoint {
                attrs: AttrSet::new(),
                start_time_unix_nano: START,
                time_unix_nano: TIME,
                value: Value::Double(0.25),
            }],
        }];
        let got = parse(encode_metrics(&MetricsPayload {
            resource: &resource_attrs,
            temporality: Temporality::Cumulative,
            metrics: &metrics,
        }));
        let sum = &got["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"];
        assert_eq!(sum["dataPoints"][0]["asDouble"], json!(0.25));
        assert_eq!(sum["aggregationTemporality"], json!(2));
    }

    #[test]
    fn logs_request_matches_the_otlp_json_shape() {
        let resource_attrs = AttrSet::new();
        let records = vec![LogRecord {
            time_unix_nano: TIME,
            event_name: EVENT_API_REQUEST,
            attrs: AttrSet::new()
                .with("input_tokens", 10i64)
                .with("cost_usd", 0.5f64)
                .with("ok", true),
        }];
        let got = parse(encode_logs(&LogsPayload {
            resource: &resource_attrs,
            records: &records,
        }));
        let record = &got["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
        assert_eq!(record["eventName"], json!(EVENT_API_REQUEST));
        assert_eq!(record["timeUnixNano"], json!("2"));
        assert_eq!(record["observedTimeUnixNano"], json!("2"));
        assert_eq!(record["severityNumber"], json!(SEVERITY_INFO));
        assert_eq!(
            record["attributes"],
            json!([
                { "key": "cost_usd", "value": { "doubleValue": 0.5 } },
                { "key": "input_tokens", "value": { "intValue": "10" } },
                { "key": "ok", "value": { "boolValue": true } },
            ])
        );
    }

    #[test_case(f64::NAN, "NaN"; "nan")]
    #[test_case(f64::INFINITY, "Infinity"; "positive_infinity")]
    #[test_case(f64::NEG_INFINITY, "-Infinity"; "negative_infinity")]
    fn non_finite_doubles_are_spelled_out(value: f64, expected: &str) {
        let records = vec![LogRecord {
            time_unix_nano: TIME,
            event_name: EVENT_API_REQUEST,
            attrs: AttrSet::new().with("cost_usd", value),
        }];
        let got = parse(encode_logs(&LogsPayload {
            resource: &AttrSet::new(),
            records: &records,
        }));
        let attribute = &got["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["attributes"][0];
        assert_eq!(attribute["value"]["doubleValue"], json!(expected));
    }
}
