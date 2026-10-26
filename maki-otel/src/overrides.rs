//! The environment wins over `init.lua`, so an operator can move a checked-in
//! config off its collector. A key whose variable is set is dropped before
//! anything reads the config, and the SDK, which reads a variable only for
//! what code leaves unset, takes it from there.

use maki_config::TelemetryConfig;
use opentelemetry_otlp::{
    OTEL_EXPORTER_OTLP_COMPRESSION, OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_EXPORTER_OTLP_HEADERS,
    OTEL_EXPORTER_OTLP_LOGS_ENDPOINT, OTEL_EXPORTER_OTLP_LOGS_HEADERS,
    OTEL_EXPORTER_OTLP_LOGS_PROTOCOL, OTEL_EXPORTER_OTLP_LOGS_TIMEOUT,
    OTEL_EXPORTER_OTLP_METRICS_ENDPOINT, OTEL_EXPORTER_OTLP_METRICS_HEADERS,
    OTEL_EXPORTER_OTLP_METRICS_PROTOCOL, OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE,
    OTEL_EXPORTER_OTLP_METRICS_TIMEOUT, OTEL_EXPORTER_OTLP_PROTOCOL, OTEL_EXPORTER_OTLP_TIMEOUT,
};
use opentelemetry_sdk::logs::{
    OTEL_BLRP_MAX_EXPORT_BATCH_SIZE, OTEL_BLRP_MAX_QUEUE_SIZE, OTEL_BLRP_SCHEDULE_DELAY,
};
use opentelemetry_sdk::metrics::OTEL_METRIC_EXPORT_INTERVAL;

use crate::export::ENV_LOGS_EXPORT_INTERVAL;
use crate::settings::{
    ENV_CONTENT_MAX_LENGTH, ENV_ENABLE, ENV_LOG_TOOL_DETAILS, ENV_LOG_USER_PROMPTS,
    ENV_LOGS_EXPORTER, ENV_METRICS_EXPORTER, ENV_METRICS_INCLUDE_SESSION_ID,
    ENV_METRICS_INCLUDE_VERSION,
};

const ENV_SERVICE_NAME: &str = "OTEL_SERVICE_NAME";
const ENV_RESOURCE_ATTRIBUTES: &str = "OTEL_RESOURCE_ATTRIBUTES";

/// An `init.lua` key and the variables that win over it, its own first.
struct Override {
    key: &'static str,
    env: &'static [&'static str],
    take: fn(&mut TelemetryConfig) -> bool,
}

macro_rules! overrides {
    ($($field:ident: [$($env:expr),+ $(,)?]),+ $(,)?) => {
        &[$(Override {
            key: stringify!($field),
            env: &[$($env),+],
            take: |lua| lua.$field.take().is_some(),
        }),+]
    };
}

/// A per-signal key also loses to the generic variable. A generic key that
/// loses to one signal's variable still applies to the other signal, so
/// `export` drops it there. An endpoint variable drops the protocol too, so
/// a gRPC protocol from `init.lua` never reaches an HTTP collector the
/// environment names.
const OVERRIDES: &[Override] = overrides! {
    enabled: [ENV_ENABLE],
    metrics_exporter: [ENV_METRICS_EXPORTER],
    logs_exporter: [ENV_LOGS_EXPORTER],
    protocol: [OTEL_EXPORTER_OTLP_PROTOCOL, OTEL_EXPORTER_OTLP_ENDPOINT],
    endpoint: [OTEL_EXPORTER_OTLP_ENDPOINT],
    headers: [OTEL_EXPORTER_OTLP_HEADERS],
    timeout_ms: [OTEL_EXPORTER_OTLP_TIMEOUT],
    compression: [OTEL_EXPORTER_OTLP_COMPRESSION],
    metrics_protocol: [
        OTEL_EXPORTER_OTLP_METRICS_PROTOCOL,
        OTEL_EXPORTER_OTLP_PROTOCOL,
        OTEL_EXPORTER_OTLP_METRICS_ENDPOINT,
        OTEL_EXPORTER_OTLP_ENDPOINT,
    ],
    metrics_endpoint: [OTEL_EXPORTER_OTLP_METRICS_ENDPOINT, OTEL_EXPORTER_OTLP_ENDPOINT],
    metrics_headers: [OTEL_EXPORTER_OTLP_METRICS_HEADERS],
    metrics_timeout_ms: [OTEL_EXPORTER_OTLP_METRICS_TIMEOUT, OTEL_EXPORTER_OTLP_TIMEOUT],
    logs_protocol: [
        OTEL_EXPORTER_OTLP_LOGS_PROTOCOL,
        OTEL_EXPORTER_OTLP_PROTOCOL,
        OTEL_EXPORTER_OTLP_LOGS_ENDPOINT,
        OTEL_EXPORTER_OTLP_ENDPOINT,
    ],
    logs_endpoint: [OTEL_EXPORTER_OTLP_LOGS_ENDPOINT, OTEL_EXPORTER_OTLP_ENDPOINT],
    logs_headers: [OTEL_EXPORTER_OTLP_LOGS_HEADERS],
    logs_timeout_ms: [OTEL_EXPORTER_OTLP_LOGS_TIMEOUT, OTEL_EXPORTER_OTLP_TIMEOUT],
    metrics_interval_ms: [OTEL_METRIC_EXPORT_INTERVAL],
    logs_interval_ms: [ENV_LOGS_EXPORT_INTERVAL, OTEL_BLRP_SCHEDULE_DELAY],
    logs_max_queue_size: [OTEL_BLRP_MAX_QUEUE_SIZE],
    logs_max_export_batch_size: [OTEL_BLRP_MAX_EXPORT_BATCH_SIZE],
    metrics_temporality: [OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE],
    service_name: [ENV_SERVICE_NAME],
    resource_attributes: [ENV_RESOURCE_ATTRIBUTES],
    metrics_include_session_id: [ENV_METRICS_INCLUDE_SESSION_ID],
    metrics_include_version: [ENV_METRICS_INCLUDE_VERSION],
    log_user_prompts: [ENV_LOG_USER_PROMPTS],
    log_tool_details: [ENV_LOG_TOOL_DETAILS],
    content_max_length: [ENV_CONTENT_MAX_LENGTH],
};

/// The key a variable overrode, for the log.
pub(crate) struct Overridden {
    pub(crate) key: &'static str,
    pub(crate) env: &'static str,
}

pub(crate) fn env_wins(
    lua: &TelemetryConfig,
    is_set: impl Fn(&str) -> bool,
) -> (TelemetryConfig, Vec<Overridden>) {
    let mut lua = lua.clone();
    let overridden = OVERRIDES
        .iter()
        .filter_map(|over| {
            let env = over.env.iter().copied().find(|&env| is_set(env))?;
            (over.take)(&mut lua).then_some(Overridden { key: over.key, env })
        })
        .collect();
    (lua, overridden)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const LUA_VALUE: &str = "from-lua";
    const ENV_DOC_SEPARATOR: &str = ", ";

    fn lua() -> TelemetryConfig {
        TelemetryConfig {
            protocol: Some(LUA_VALUE.to_string()),
            metrics_protocol: Some(LUA_VALUE.to_string()),
            endpoint: Some(LUA_VALUE.to_string()),
            metrics_endpoint: Some(LUA_VALUE.to_string()),
            logs_endpoint: Some(LUA_VALUE.to_string()),
            service_name: Some(LUA_VALUE.to_string()),
            ..TelemetryConfig::default()
        }
    }

    #[test_case(OTEL_EXPORTER_OTLP_ENDPOINT, &["protocol", "endpoint", "metrics_protocol", "metrics_endpoint", "logs_endpoint"]; "generic_variable_beats_per_signal_keys")]
    #[test_case(OTEL_EXPORTER_OTLP_METRICS_ENDPOINT, &["metrics_protocol", "metrics_endpoint"]; "per_signal_variable_leaves_the_generic_key")]
    #[test_case(OTEL_EXPORTER_OTLP_PROTOCOL, &["protocol", "metrics_protocol"]; "protocol_variable_leaves_the_endpoint")]
    #[test_case(ENV_SERVICE_NAME, &["service_name"]; "service_name")]
    #[test_case(OTEL_EXPORTER_OTLP_HEADERS, &[]; "a_key_that_is_not_set_is_not_reported")]
    fn drops_the_keys_a_set_variable_overrides(env: &str, expected: &[&str]) {
        let (kept, overridden) = env_wins(&lua(), |key| key == env);
        let keys: Vec<_> = overridden.iter().map(|over| over.key).collect();
        assert_eq!(keys, expected);
        assert!(overridden.iter().all(|over| over.env == env));
        let dropped = |key| keys.contains(&key);
        assert_eq!(kept.protocol.is_none(), dropped("protocol"));
        assert_eq!(kept.endpoint.is_none(), dropped("endpoint"));
        assert_eq!(kept.metrics_endpoint.is_none(), dropped("metrics_endpoint"));
        assert_eq!(kept.service_name.is_none(), dropped("service_name"));
    }

    #[test]
    fn every_documented_variable_overrides_its_key() {
        for field in TelemetryConfig::FIELDS {
            let Some(documented) = field.env else {
                continue;
            };
            let over = OVERRIDES
                .iter()
                .find(|over| over.key == field.name)
                .unwrap_or_else(|| panic!("{} has no override", field.name));
            for env in documented.split(ENV_DOC_SEPARATOR) {
                assert!(
                    over.env.contains(&env),
                    "{env} does not override {}",
                    field.name
                );
            }
        }
    }
}
