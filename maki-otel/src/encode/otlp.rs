//! OTLP export requests in protobuf.
//!
//! Field numbers come from opentelemetry-proto v1.5 (`metrics.proto`,
//! `logs.proto`, `common.proto`, `resource.proto`).

use crate::attr::{AttrSet, AttrValue};
use crate::encode::pb::Writer;
use crate::logs::{LogRecord, SEVERITY_INFO, SEVERITY_TEXT_INFO};
use crate::metrics::{MetricData, Value};
use crate::resource::{SCOPE_NAME, VERSION};
use crate::settings::Temporality;

const ANY_STRING: u32 = 1;
const ANY_BOOL: u32 = 2;
const ANY_INT: u32 = 3;
const ANY_DOUBLE: u32 = 4;

const KV_KEY: u32 = 1;
const KV_VALUE: u32 = 2;

const SCOPE_FIELD_NAME: u32 = 1;
const SCOPE_FIELD_VERSION: u32 = 2;

const RESOURCE_ATTRIBUTES: u32 = 1;

const EXPORT_METRICS_RESOURCE_METRICS: u32 = 1;
const RESOURCE_METRICS_RESOURCE: u32 = 1;
const RESOURCE_METRICS_SCOPE_METRICS: u32 = 2;
const SCOPE_METRICS_SCOPE: u32 = 1;
const SCOPE_METRICS_METRICS: u32 = 2;

const METRIC_NAME: u32 = 1;
const METRIC_DESCRIPTION: u32 = 2;
const METRIC_UNIT: u32 = 3;
const METRIC_SUM: u32 = 7;

const SUM_DATA_POINTS: u32 = 1;
const SUM_TEMPORALITY: u32 = 2;
const SUM_IS_MONOTONIC: u32 = 3;

const POINT_START_TIME: u32 = 2;
const POINT_TIME: u32 = 3;
const POINT_AS_DOUBLE: u32 = 4;
const POINT_AS_INT: u32 = 6;
const POINT_ATTRIBUTES: u32 = 7;

const EXPORT_LOGS_RESOURCE_LOGS: u32 = 1;
const RESOURCE_LOGS_RESOURCE: u32 = 1;
const RESOURCE_LOGS_SCOPE_LOGS: u32 = 2;
const SCOPE_LOGS_SCOPE: u32 = 1;
const SCOPE_LOGS_RECORDS: u32 = 2;

const LOG_TIME: u32 = 1;
const LOG_SEVERITY_NUMBER: u32 = 2;
const LOG_SEVERITY_TEXT: u32 = 3;
const LOG_ATTRIBUTES: u32 = 6;
const LOG_OBSERVED_TIME: u32 = 11;
const LOG_EVENT_NAME: u32 = 12;

const TEMPORALITY_DELTA: i32 = 1;
const TEMPORALITY_CUMULATIVE: i32 = 2;

/// Temporality is a registry-wide preference, so it sits here and not on
/// every metric.
pub struct MetricsPayload<'a> {
    pub resource: &'a AttrSet,
    pub temporality: Temporality,
    pub metrics: &'a [MetricData],
}

pub struct LogsPayload<'a> {
    pub resource: &'a AttrSet,
    pub records: &'a [LogRecord],
}

pub fn temporality_code(temporality: Temporality) -> i32 {
    match temporality {
        Temporality::Delta => TEMPORALITY_DELTA,
        Temporality::Cumulative => TEMPORALITY_CUMULATIVE,
    }
}

fn write_any_value(w: &mut Writer, field: u32, value: &AttrValue) {
    w.message(field, |any| match value {
        AttrValue::Str(v) => any.string(ANY_STRING, v),
        AttrValue::Bool(v) => any.bool(ANY_BOOL, *v),
        AttrValue::Int(v) => any.int64(ANY_INT, *v),
        AttrValue::Double(v) => any.double(ANY_DOUBLE, *v),
    });
}

fn write_attributes(w: &mut Writer, field: u32, attrs: &AttrSet) {
    for (key, value) in attrs.iter() {
        w.message(field, |kv| {
            kv.string(KV_KEY, key);
            write_any_value(kv, KV_VALUE, value);
        });
    }
}

fn write_scope(w: &mut Writer, field: u32) {
    w.message(field, |scope| {
        scope.string(SCOPE_FIELD_NAME, SCOPE_NAME);
        scope.string(SCOPE_FIELD_VERSION, VERSION);
    });
}

fn write_resource(w: &mut Writer, field: u32, resource: &AttrSet) {
    w.message(field, |r| {
        write_attributes(r, RESOURCE_ATTRIBUTES, resource);
    });
}

pub fn encode_metrics(payload: &MetricsPayload<'_>) -> Vec<u8> {
    let mut w = Writer::with_capacity(1024);
    w.message(EXPORT_METRICS_RESOURCE_METRICS, |rm| {
        write_resource(rm, RESOURCE_METRICS_RESOURCE, payload.resource);
        rm.message(RESOURCE_METRICS_SCOPE_METRICS, |sm| {
            write_scope(sm, SCOPE_METRICS_SCOPE);
            for metric in payload.metrics {
                sm.message(SCOPE_METRICS_METRICS, |m| {
                    m.string(METRIC_NAME, metric.def.name);
                    m.string(METRIC_DESCRIPTION, metric.def.description);
                    m.string(METRIC_UNIT, metric.def.unit);
                    m.message(METRIC_SUM, |sum| {
                        for point in &metric.points {
                            sum.message(SUM_DATA_POINTS, |p| {
                                p.fixed64(POINT_START_TIME, point.start_time_unix_nano);
                                p.fixed64(POINT_TIME, point.time_unix_nano);
                                match point.value {
                                    Value::Int(v) => p.sfixed64(POINT_AS_INT, v),
                                    Value::Double(v) => p.double(POINT_AS_DOUBLE, v),
                                }
                                write_attributes(p, POINT_ATTRIBUTES, &point.attrs);
                            });
                        }
                        sum.int32(SUM_TEMPORALITY, temporality_code(payload.temporality));
                        sum.bool(SUM_IS_MONOTONIC, true);
                    });
                });
            }
        });
    });
    w.into_bytes()
}

pub fn encode_logs(payload: &LogsPayload<'_>) -> Vec<u8> {
    let mut w = Writer::with_capacity(1024);
    w.message(EXPORT_LOGS_RESOURCE_LOGS, |rl| {
        write_resource(rl, RESOURCE_LOGS_RESOURCE, payload.resource);
        rl.message(RESOURCE_LOGS_SCOPE_LOGS, |sl| {
            write_scope(sl, SCOPE_LOGS_SCOPE);
            for record in payload.records {
                sl.message(SCOPE_LOGS_RECORDS, |r| {
                    r.fixed64(LOG_TIME, record.time_unix_nano);
                    r.int32(LOG_SEVERITY_NUMBER, SEVERITY_INFO);
                    r.string(LOG_SEVERITY_TEXT, SEVERITY_TEXT_INFO);
                    write_attributes(r, LOG_ATTRIBUTES, &record.attrs);
                    r.fixed64(LOG_OBSERVED_TIME, record.time_unix_nano);
                    r.string(LOG_EVENT_NAME, record.event_name);
                });
            }
        });
    });
    w.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{COMMIT_COUNT, DataPoint};

    const TIME: u64 = 0x0102030405060708;
    const START: u64 = 1;

    fn empty_resource() -> AttrSet {
        AttrSet::new()
    }

    fn commit_metric() -> MetricData {
        MetricData {
            def: &COMMIT_COUNT,
            points: vec![DataPoint {
                attrs: AttrSet::new(),
                start_time_unix_nano: START,
                time_unix_nano: TIME,
                value: Value::Int(1),
            }],
        }
    }

    /// Walks the top-level length-delimited fields of a message so tests can
    /// assert structure without a protobuf decoder.
    fn fields(bytes: &[u8]) -> Vec<(u32, u32, &[u8])> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let (tag, read) = varint(&bytes[i..]);
            i += read;
            let (field, wire) = ((tag >> 3) as u32, (tag & 7) as u32);
            let len = match wire {
                0 => varint(&bytes[i..]).1,
                1 => 8,
                5 => 4,
                2 => {
                    let (len, read) = varint(&bytes[i..]);
                    i += read;
                    len as usize
                }
                other => panic!("unexpected wire type {other}"),
            };
            out.push((field, wire, &bytes[i..i + len]));
            i += len;
        }
        out
    }

    fn varint(bytes: &[u8]) -> (u64, usize) {
        let mut value = 0u64;
        let mut shift = 0;
        for (i, byte) in bytes.iter().enumerate() {
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return (value, i + 1);
            }
            shift += 7;
        }
        panic!("truncated varint");
    }

    fn only(bytes: &[u8], field: u32) -> &[u8] {
        let found: Vec<&[u8]> = fields(bytes)
            .into_iter()
            .filter(|(f, _, _)| *f == field)
            .map(|(_, _, body)| body)
            .collect();
        assert_eq!(found.len(), 1, "expected exactly one field {field}");
        found[0]
    }

    #[test]
    fn metrics_request_nests_resource_scope_and_metric() {
        let resource = empty_resource();
        let metrics = vec![commit_metric()];
        let bytes = encode_metrics(&MetricsPayload {
            resource: &resource,
            temporality: Temporality::Delta,
            metrics: &metrics,
        });

        let resource_metrics = only(&bytes, EXPORT_METRICS_RESOURCE_METRICS);
        let scope_metrics = only(resource_metrics, RESOURCE_METRICS_SCOPE_METRICS);
        let metric = only(scope_metrics, SCOPE_METRICS_METRICS);
        assert_eq!(only(metric, METRIC_NAME), COMMIT_COUNT.name.as_bytes());

        let sum = only(metric, METRIC_SUM);
        let point = only(sum, SUM_DATA_POINTS);
        assert_eq!(
            fields(point)
                .iter()
                .find(|(f, _, _)| *f == POINT_TIME)
                .map(|(_, _, b)| *b),
            Some(&TIME.to_le_bytes()[..])
        );
        let as_int = fields(point)
            .into_iter()
            .find(|(f, _, _)| *f == POINT_AS_INT)
            .expect("as_int should be present");
        assert_eq!(as_int.1, 1, "as_int is a fixed64 wire type");
        assert_eq!(as_int.2, &1i64.to_le_bytes()[..]);
    }

    #[test]
    fn attribute_values_pick_the_matching_any_value_field() {
        let resource = AttrSet::new()
            .with("s", "v")
            .with("i", 1i64)
            .with("d", 1.5f64)
            .with("b", true);
        let bytes = encode_metrics(&MetricsPayload {
            resource: &resource,
            temporality: Temporality::Delta,
            metrics: &[],
        });
        let resource_bytes = only(
            only(&bytes, EXPORT_METRICS_RESOURCE_METRICS),
            RESOURCE_METRICS_RESOURCE,
        );
        let kinds: Vec<u32> = fields(resource_bytes)
            .into_iter()
            .map(|(_, _, kv)| {
                let value = only(kv, KV_VALUE);
                fields(value)[0].0
            })
            .collect();
        // Attributes are sorted by key: b, d, i, s.
        assert_eq!(kinds, vec![ANY_BOOL, ANY_DOUBLE, ANY_INT, ANY_STRING]);
    }

    #[test]
    fn log_records_carry_event_name_and_severity() {
        let resource = empty_resource();
        let records = vec![LogRecord {
            time_unix_nano: TIME,
            event_name: crate::logs::EVENT_API_REQUEST,
            attrs: AttrSet::new().with("model", "m"),
        }];
        let bytes = encode_logs(&LogsPayload {
            resource: &resource,
            records: &records,
        });
        let record = only(
            only(
                only(&bytes, EXPORT_LOGS_RESOURCE_LOGS),
                RESOURCE_LOGS_SCOPE_LOGS,
            ),
            SCOPE_LOGS_RECORDS,
        );
        assert_eq!(
            only(record, LOG_EVENT_NAME),
            crate::logs::EVENT_API_REQUEST.as_bytes()
        );
        assert_eq!(
            only(record, LOG_SEVERITY_TEXT),
            SEVERITY_TEXT_INFO.as_bytes()
        );
        assert_eq!(only(record, LOG_TIME), &TIME.to_le_bytes()[..]);
    }

    #[test]
    fn an_empty_batch_still_carries_the_resource() {
        let resource = AttrSet::new().with("service.name", "maki");
        let bytes = encode_logs(&LogsPayload {
            resource: &resource,
            records: &[],
        });
        let resource_logs = only(&bytes, EXPORT_LOGS_RESOURCE_LOGS);
        assert!(!only(resource_logs, RESOURCE_LOGS_RESOURCE).is_empty());
    }
}
