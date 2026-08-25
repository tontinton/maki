//! Resolution of telemetry settings from environment variables and `init.lua`.
//!
//! Precedence is env var > `init.lua` > default. Env lookups go through an
//! injected closure so tests never touch the process environment.

use std::collections::BTreeMap;
use std::time::Duration;

use maki_config::TelemetryConfig;
use thiserror::Error;

pub const ENV_ENABLE: &str = "MAKI_ENABLE_TELEMETRY";
pub const ENV_SDK_DISABLED: &str = "OTEL_SDK_DISABLED";
pub const ENV_METRICS_EXPORTER: &str = "OTEL_METRICS_EXPORTER";
pub const ENV_LOGS_EXPORTER: &str = "OTEL_LOGS_EXPORTER";
pub const ENV_PROTOCOL: &str = "OTEL_EXPORTER_OTLP_PROTOCOL";
pub const ENV_ENDPOINT: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
pub const ENV_HEADERS: &str = "OTEL_EXPORTER_OTLP_HEADERS";
pub const ENV_TIMEOUT: &str = "OTEL_EXPORTER_OTLP_TIMEOUT";
pub const ENV_COMPRESSION: &str = "OTEL_EXPORTER_OTLP_COMPRESSION";
pub const ENV_METRIC_EXPORT_INTERVAL: &str = "OTEL_METRIC_EXPORT_INTERVAL";
pub const ENV_METRIC_EXPORT_TIMEOUT: &str = "OTEL_METRIC_EXPORT_TIMEOUT";
pub const ENV_LOGS_EXPORT_INTERVAL: &str = "OTEL_LOGS_EXPORT_INTERVAL";
pub const ENV_BLRP_SCHEDULE_DELAY: &str = "OTEL_BLRP_SCHEDULE_DELAY";
pub const ENV_BLRP_MAX_QUEUE_SIZE: &str = "OTEL_BLRP_MAX_QUEUE_SIZE";
pub const ENV_BLRP_MAX_EXPORT_BATCH_SIZE: &str = "OTEL_BLRP_MAX_EXPORT_BATCH_SIZE";
pub const ENV_BLRP_EXPORT_TIMEOUT: &str = "OTEL_BLRP_EXPORT_TIMEOUT";
pub const ENV_TEMPORALITY: &str = "OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE";
pub const ENV_SERVICE_NAME: &str = "OTEL_SERVICE_NAME";
pub const ENV_RESOURCE_ATTRIBUTES: &str = "OTEL_RESOURCE_ATTRIBUTES";
pub const ENV_METRICS_INCLUDE_SESSION_ID: &str = "OTEL_METRICS_INCLUDE_SESSION_ID";
pub const ENV_METRICS_INCLUDE_VERSION: &str = "OTEL_METRICS_INCLUDE_VERSION";
pub const ENV_LOG_USER_PROMPTS: &str = "OTEL_LOG_USER_PROMPTS";
pub const ENV_LOG_TOOL_DETAILS: &str = "OTEL_LOG_TOOL_DETAILS";
pub const ENV_CONTENT_MAX_LENGTH: &str = "MAKI_OTEL_CONTENT_MAX_LENGTH";

pub const DEFAULT_SERVICE_NAME: &str = "maki";
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
pub const DEFAULT_METRICS_INTERVAL_MS: u64 = 60_000;
pub const DEFAULT_METRICS_EXPORT_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_LOGS_INTERVAL_MS: u64 = 5_000;
pub const DEFAULT_LOGS_MAX_QUEUE_SIZE: usize = 2048;
pub const DEFAULT_LOGS_MAX_EXPORT_BATCH_SIZE: usize = 512;
pub const DEFAULT_LOGS_EXPORT_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_CONTENT_MAX_LENGTH: usize = 10_240;
/// The one attribute that is on by default: a session is the unit teams slice
/// by, and one maki process only ever reports a handful of them.
pub const DEFAULT_METRICS_INCLUDE_SESSION_ID: bool = true;

const SIGNAL_METRICS: &str = "METRICS";
const SIGNAL_LOGS: &str = "LOGS";

const EXPECTED_EXPORTER: &str = "otlp, console or none";
const EXPECTED_PROTOCOL: &str = "grpc, http/protobuf or http/json";
const EXPECTED_COMPRESSION: &str = "gzip or none";
const EXPECTED_TEMPORALITY: &str = "delta or cumulative";
const EXPECTED_BOOL: &str = "true or false";
const EXPECTED_MILLIS: &str = "a number of milliseconds";
const EXPECTED_COUNT: &str = "a positive integer";

/// A zero interval spins the export loop on the same executor the agent runs
/// on, and a zero timeout fails every export, so durations have a floor.
const MIN_DURATION_MS: u64 = 100;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SettingsError {
    #[error("invalid telemetry setting {key}={value:?} (from {origin}), expected {expected}")]
    Invalid {
        origin: Source,
        key: String,
        value: String,
        expected: &'static str,
    },
    #[error(
        "telemetry is on and {exporter}=otlp, but no protocol is set; \
         set OTEL_EXPORTER_OTLP_PROTOCOL or telemetry.protocol to grpc, http/protobuf or http/json"
    )]
    MissingProtocol { exporter: &'static str },
    #[error(
        "telemetry is on and {exporter}=otlp, but no endpoint is set; \
         set OTEL_EXPORTER_OTLP_ENDPOINT or telemetry.endpoint"
    )]
    MissingEndpoint { exporter: &'static str },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Env,
    Lua,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Env => write!(f, "the environment"),
            Self::Lua => write!(f, "init.lua"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exporter {
    Otlp,
    Console,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Grpc,
    HttpProtobuf,
    HttpJson,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Temporality {
    Delta,
    Cumulative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Metrics,
    Logs,
}

impl Signal {
    fn env_infix(self) -> &'static str {
        match self {
            Self::Metrics => SIGNAL_METRICS,
            Self::Logs => SIGNAL_LOGS,
        }
    }

    fn lua_prefix(self) -> &'static str {
        match self {
            Self::Metrics => "metrics_",
            Self::Logs => "logs_",
        }
    }

    fn http_path(self) -> &'static str {
        match self {
            Self::Metrics => "/v1/metrics",
            Self::Logs => "/v1/logs",
        }
    }

    fn grpc_path(self) -> &'static str {
        match self {
            Self::Metrics => "/opentelemetry.proto.collector.metrics.v1.MetricsService/Export",
            Self::Logs => "/opentelemetry.proto.collector.logs.v1.LogsService/Export",
        }
    }

    /// The exporter variable that turned this signal on, named in the errors
    /// about a missing protocol or endpoint.
    fn exporter_env(self) -> &'static str {
        match self {
            Self::Metrics => ENV_METRICS_EXPORTER,
            Self::Logs => ENV_LOGS_EXPORTER,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalSettings {
    pub protocol: Protocol,
    /// Full URL including any signal path. Ready to POST to.
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub timeout: Duration,
    pub compression: Compression,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub metrics_exporters: Vec<Exporter>,
    pub logs_exporters: Vec<Exporter>,
    pub metrics: Option<SignalSettings>,
    pub logs: Option<SignalSettings>,
    pub metrics_interval: Duration,
    pub metrics_export_timeout: Duration,
    pub logs_interval: Duration,
    pub logs_max_queue_size: usize,
    pub logs_max_export_batch_size: usize,
    pub logs_export_timeout: Duration,
    pub temporality: Temporality,
    pub service_name: String,
    pub resource_attributes: Vec<(String, String)>,
    pub metrics_include_session_id: bool,
    pub metrics_include_version: bool,
    pub log_user_prompts: bool,
    pub log_tool_details: bool,
    pub content_max_length: usize,
}

/// Reads one key from either source, remembering which one answered.
struct Resolver<'a, F> {
    env: F,
    lua: &'a TelemetryConfig,
}

struct Found {
    source: Source,
    key: String,
    value: String,
    /// Set when the value came from a per-signal key rather than a generic
    /// one, which decides whether an endpoint already names its signal.
    specific: bool,
}

impl Found {
    fn specific(self) -> Self {
        Self {
            specific: true,
            ..self
        }
    }
}

impl<F: Fn(&str) -> Option<String>> Resolver<'_, F> {
    fn env(&self, key: &str) -> Option<Found> {
        let value = (self.env)(key)?;
        let value = value.trim().to_string();
        (!value.is_empty()).then(|| Found {
            source: Source::Env,
            key: key.to_string(),
            value,
            specific: false,
        })
    }

    fn lua_value(&self, key: &str, value: Option<String>) -> Option<Found> {
        let value = value?.trim().to_string();
        (!value.is_empty()).then(|| Found {
            source: Source::Lua,
            key: format!("telemetry.{key}"),
            value,
            specific: false,
        })
    }

    fn get(&self, env_key: &str, lua_key: &str, lua_value: Option<String>) -> Option<Found> {
        self.env(env_key)
            .or_else(|| self.lua_value(lua_key, lua_value))
    }

    /// Per-signal key falling back to the generic one. Both are given as
    /// `(env suffix or key, lua suffix or key, configured value)`. The env
    /// always beats `init.lua`, so a generic env var wins over a per-signal
    /// Lua field: an operator setting `OTEL_EXPORTER_OTLP_ENDPOINT` must be
    /// able to move a checked-in config off its collector.
    fn signal_get(
        &self,
        signal: Signal,
        specific: (&str, &str, Option<String>),
        generic: (&str, &str, Option<String>),
    ) -> Option<Found> {
        let env_key = format!("OTEL_EXPORTER_OTLP_{}_{}", signal.env_infix(), specific.0);
        let lua_key = format!("{}{}", signal.lua_prefix(), specific.1);
        self.env(&env_key)
            .map(Found::specific)
            .or_else(|| self.env(generic.0))
            .or_else(|| self.lua_value(&lua_key, specific.2).map(Found::specific))
            .or_else(|| self.lua_value(generic.1, generic.2))
    }

    fn millis_or(
        &self,
        env_key: &str,
        lua_key: &str,
        lua_value: Option<u64>,
        default_ms: u64,
    ) -> Result<Duration, SettingsError> {
        match self.get(env_key, lua_key, lua_value.map(|v| v.to_string())) {
            Some(found) => millis(&found),
            None => Ok(Duration::from_millis(default_ms)),
        }
    }

    fn count_or(
        &self,
        env_key: &str,
        lua_key: &str,
        lua_value: Option<usize>,
        default: usize,
    ) -> Result<usize, SettingsError> {
        match self.get(env_key, lua_key, lua_value.map(|v| v.to_string())) {
            Some(found) => parse_u64(&found, EXPECTED_COUNT).map(|n| n as usize),
            None => Ok(default),
        }
    }
}

fn millis(found: &Found) -> Result<Duration, SettingsError> {
    parse_u64(found, EXPECTED_MILLIS).map(|ms| Duration::from_millis(ms.max(MIN_DURATION_MS)))
}

fn parse_bool(found: &Found) -> Result<bool, SettingsError> {
    match found.value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(invalid(found, EXPECTED_BOOL)),
    }
}

fn parse_u64(found: &Found, expected: &'static str) -> Result<u64, SettingsError> {
    found
        .value
        .parse::<u64>()
        .map_err(|_| invalid(found, expected))
}

fn invalid(found: &Found, expected: &'static str) -> SettingsError {
    SettingsError::Invalid {
        origin: found.source,
        key: found.key.clone(),
        value: found.value.clone(),
        expected,
    }
}

/// A repeated exporter would build a second transport and export every batch
/// twice, so the list keeps first mention only.
fn parse_exporters(found: &Found) -> Result<Vec<Exporter>, SettingsError> {
    let mut out: Vec<Exporter> = Vec::new();
    for part in found.value.split(',') {
        let exporter = match part.trim().to_ascii_lowercase().as_str() {
            "" | "none" => continue,
            "otlp" => Exporter::Otlp,
            "console" => Exporter::Console,
            _ => return Err(invalid(found, EXPECTED_EXPORTER)),
        };
        if !out.contains(&exporter) {
            out.push(exporter);
        }
    }
    Ok(out)
}

fn parse_protocol(found: &Found) -> Result<Protocol, SettingsError> {
    match found.value.to_ascii_lowercase().as_str() {
        "grpc" => Ok(Protocol::Grpc),
        "http/protobuf" => Ok(Protocol::HttpProtobuf),
        "http/json" => Ok(Protocol::HttpJson),
        _ => Err(invalid(found, EXPECTED_PROTOCOL)),
    }
}

fn parse_compression(found: &Found) -> Result<Compression, SettingsError> {
    match found.value.to_ascii_lowercase().as_str() {
        "none" => Ok(Compression::None),
        "gzip" => Ok(Compression::Gzip),
        _ => Err(invalid(found, EXPECTED_COMPRESSION)),
    }
}

fn parse_temporality(found: &Found) -> Result<Temporality, SettingsError> {
    match found.value.to_ascii_lowercase().as_str() {
        "delta" => Ok(Temporality::Delta),
        "cumulative" => Ok(Temporality::Cumulative),
        _ => Err(invalid(found, EXPECTED_TEMPORALITY)),
    }
}

/// `k1=v1,k2=v2` with percent-decoded values, per the OTLP env var spec.
fn parse_key_values(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            let k = k.trim();
            (!k.is_empty()).then(|| (k.to_string(), percent_decode(v.trim())))
        })
        .collect()
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(byte) = hex {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn pairs(table: Option<&BTreeMap<String, String>>) -> Vec<(String, String)> {
    table
        .map(|t| t.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

/// Later entries win, so per-signal headers override generic ones by key.
fn merge_headers(
    generic: &[(String, String)],
    specific: &[(String, String)],
) -> Vec<(String, String)> {
    let mut merged: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in generic.iter().chain(specific) {
        merged.insert(k.clone(), v.clone());
    }
    merged.into_iter().collect()
}

/// The full URL to POST to. Generic HTTP endpoints get the signal path
/// appended and per-signal ones are used verbatim; a gRPC endpoint is always
/// a bare authority and always gets the service method.
fn build_url(protocol: Protocol, endpoint: &str, signal: Signal, per_signal: bool) -> String {
    let trimmed = endpoint.trim_end_matches('/');
    match protocol {
        Protocol::Grpc => format!("{trimmed}{}", signal.grpc_path()),
        _ if per_signal => endpoint.to_string(),
        _ => format!("{trimmed}{}", signal.http_path()),
    }
}

/// `None` when telemetry is off, so callers can skip building anything.
pub fn resolve<F: Fn(&str) -> Option<String>>(
    lua: &TelemetryConfig,
    env: F,
) -> Result<Option<Settings>, SettingsError> {
    let r = Resolver { env, lua };

    // The OTel kill switch outranks our own opt in, so an org can turn every
    // SDK off in one place without touching a checked-in `init.lua`.
    if let Some(found) = r.env(ENV_SDK_DISABLED)
        && parse_bool(&found)?
    {
        return Ok(None);
    }
    let enabled = match r.env(ENV_ENABLE) {
        Some(found) => parse_bool(&found)?,
        None => lua.enabled.unwrap_or_default(),
    };
    if !enabled {
        return Ok(None);
    }

    let metrics_exporters = match r.get(
        ENV_METRICS_EXPORTER,
        "metrics_exporter",
        lua.metrics_exporter.clone(),
    ) {
        Some(found) => parse_exporters(&found)?,
        None => Vec::new(),
    };
    let logs_exporters = match r.get(
        ENV_LOGS_EXPORTER,
        "logs_exporter",
        lua.logs_exporter.clone(),
    ) {
        Some(found) => parse_exporters(&found)?,
        None => Vec::new(),
    };

    let generic_headers = match r.get(ENV_HEADERS, "headers", None) {
        Some(found) => parse_key_values(&found.value),
        None => pairs(lua.headers.as_ref()),
    };

    let compression = match r.get(ENV_COMPRESSION, "compression", lua.compression.clone()) {
        Some(found) => parse_compression(&found)?,
        None => Compression::None,
    };

    let metrics = signal_settings(
        &r,
        Signal::Metrics,
        &generic_headers,
        compression,
        &metrics_exporters,
    )?;
    let logs = signal_settings(
        &r,
        Signal::Logs,
        &generic_headers,
        compression,
        &logs_exporters,
    )?;

    let metrics_interval = r.millis_or(
        ENV_METRIC_EXPORT_INTERVAL,
        "metrics_interval_ms",
        lua.metrics_interval_ms,
        DEFAULT_METRICS_INTERVAL_MS,
    )?;
    let metrics_export_timeout = r.millis_or(
        ENV_METRIC_EXPORT_TIMEOUT,
        "metrics_export_timeout_ms",
        lua.metrics_export_timeout_ms,
        DEFAULT_METRICS_EXPORT_TIMEOUT_MS,
    )?;
    // The one duration with a spec alias, which slots below the Lua value
    // because OTEL_LOGS_EXPORT_INTERVAL is the name maki documents.
    let logs_interval = match r
        .get(
            ENV_LOGS_EXPORT_INTERVAL,
            "logs_interval_ms",
            lua.logs_interval_ms.map(|v| v.to_string()),
        )
        .or_else(|| r.env(ENV_BLRP_SCHEDULE_DELAY))
    {
        Some(found) => millis(&found)?,
        None => Duration::from_millis(DEFAULT_LOGS_INTERVAL_MS),
    };
    let logs_export_timeout = r.millis_or(
        ENV_BLRP_EXPORT_TIMEOUT,
        "logs_export_timeout_ms",
        lua.logs_export_timeout_ms,
        DEFAULT_LOGS_EXPORT_TIMEOUT_MS,
    )?;

    let logs_max_queue_size = r
        .count_or(
            ENV_BLRP_MAX_QUEUE_SIZE,
            "logs_max_queue_size",
            lua.logs_max_queue_size,
            DEFAULT_LOGS_MAX_QUEUE_SIZE,
        )?
        .max(1);
    let logs_max_export_batch_size = r
        .count_or(
            ENV_BLRP_MAX_EXPORT_BATCH_SIZE,
            "logs_max_export_batch_size",
            lua.logs_max_export_batch_size,
            DEFAULT_LOGS_MAX_EXPORT_BATCH_SIZE,
        )?
        .max(1);
    let content_max_length = r.count_or(
        ENV_CONTENT_MAX_LENGTH,
        "content_max_length",
        lua.content_max_length,
        DEFAULT_CONTENT_MAX_LENGTH,
    )?;

    let temporality = match r.get(
        ENV_TEMPORALITY,
        "metrics_temporality",
        lua.metrics_temporality.clone(),
    ) {
        Some(found) => parse_temporality(&found)?,
        None => Temporality::Delta,
    };

    let configured_service_name = r.get(ENV_SERVICE_NAME, "service_name", lua.service_name.clone());
    let named_explicitly = configured_service_name.is_some();
    let service_name =
        configured_service_name.map_or_else(|| DEFAULT_SERVICE_NAME.to_string(), |f| f.value);

    let mut resource_attributes = match r.get(ENV_RESOURCE_ATTRIBUTES, "resource_attributes", None)
    {
        Some(found) => parse_key_values(&found.value),
        None => pairs(lua.resource_attributes.as_ref()),
    };
    // Per the spec, an explicit service name outranks a `service.name` buried
    // in the attribute list. Left alone, the attribute is how you set it.
    if named_explicitly {
        resource_attributes.retain(|(key, _)| key != crate::resource::KEY_SERVICE_NAME);
    }

    let flag = |env_key: &str, lua_value: Option<bool>, default: bool| match r.env(env_key) {
        Some(found) => parse_bool(&found),
        None => Ok(lua_value.unwrap_or(default)),
    };

    Ok(Some(Settings {
        metrics_exporters,
        logs_exporters,
        metrics,
        logs,
        metrics_interval,
        metrics_export_timeout,
        logs_interval,
        logs_max_queue_size,
        logs_max_export_batch_size,
        logs_export_timeout,
        temporality,
        service_name,
        resource_attributes,
        metrics_include_session_id: flag(
            ENV_METRICS_INCLUDE_SESSION_ID,
            lua.metrics_include_session_id,
            DEFAULT_METRICS_INCLUDE_SESSION_ID,
        )?,
        metrics_include_version: flag(
            ENV_METRICS_INCLUDE_VERSION,
            lua.metrics_include_version,
            false,
        )?,
        log_user_prompts: flag(ENV_LOG_USER_PROMPTS, lua.log_user_prompts, false)?,
        log_tool_details: flag(ENV_LOG_TOOL_DETAILS, lua.log_tool_details, false)?,
        content_max_length,
    }))
}

fn signal_settings<F: Fn(&str) -> Option<String>>(
    r: &Resolver<'_, F>,
    signal: Signal,
    generic_headers: &[(String, String)],
    generic_compression: Compression,
    exporters: &[Exporter],
) -> Result<Option<SignalSettings>, SettingsError> {
    if !exporters.contains(&Exporter::Otlp) {
        return Ok(None);
    }
    let lua = r.lua;
    let (specific_protocol, specific_endpoint, specific_headers, specific_timeout) = match signal {
        Signal::Metrics => (
            lua.metrics_protocol.clone(),
            lua.metrics_endpoint.clone(),
            lua.metrics_headers.as_ref(),
            lua.metrics_timeout_ms.map(|v| v.to_string()),
        ),
        Signal::Logs => (
            lua.logs_protocol.clone(),
            lua.logs_endpoint.clone(),
            lua.logs_headers.as_ref(),
            lua.logs_timeout_ms.map(|v| v.to_string()),
        ),
    };

    let protocol = r
        .signal_get(
            signal,
            ("PROTOCOL", "protocol", specific_protocol),
            (ENV_PROTOCOL, "protocol", lua.protocol.clone()),
        )
        .ok_or(SettingsError::MissingProtocol {
            exporter: signal.exporter_env(),
        })?;
    let protocol = parse_protocol(&protocol)?;

    let endpoint = r
        .signal_get(
            signal,
            ("ENDPOINT", "endpoint", specific_endpoint),
            (ENV_ENDPOINT, "endpoint", lua.endpoint.clone()),
        )
        .ok_or(SettingsError::MissingEndpoint {
            exporter: signal.exporter_env(),
        })?;

    let specific_headers = match r.env(&format!(
        "OTEL_EXPORTER_OTLP_{}_HEADERS",
        signal.env_infix()
    )) {
        Some(found) => parse_key_values(&found.value),
        None => pairs(specific_headers),
    };

    let timeout = match r.signal_get(
        signal,
        ("TIMEOUT", "timeout_ms", specific_timeout),
        (
            ENV_TIMEOUT,
            "timeout_ms",
            lua.timeout_ms.map(|v| v.to_string()),
        ),
    ) {
        Some(found) => millis(&found)?,
        None => Duration::from_millis(DEFAULT_TIMEOUT_MS),
    };

    Ok(Some(SignalSettings {
        protocol,
        url: build_url(protocol, &endpoint.value, signal, endpoint.specific),
        headers: merge_headers(generic_headers, &specific_headers),
        timeout,
        compression: generic_compression,
    }))
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const ENDPOINT: &str = "http://localhost:4318";
    const GRPC_ENDPOINT: &str = "http://localhost:4317";

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key: &str| owned.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    fn resolve_env(pairs: &[(&str, &str)]) -> Result<Option<Settings>, SettingsError> {
        resolve(&TelemetryConfig::default(), env_from(pairs))
    }

    /// Extras come first so they shadow the base entries in the lookup.
    fn otlp_env(extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut pairs: Vec<(String, String)> = extra
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        pairs.extend([
            (ENV_ENABLE.to_string(), "1".to_string()),
            (ENV_METRICS_EXPORTER.to_string(), "otlp".to_string()),
            (ENV_PROTOCOL.to_string(), "http/protobuf".to_string()),
            (ENV_ENDPOINT.to_string(), ENDPOINT.to_string()),
        ]);
        pairs
    }

    fn resolve_otlp(extra: &[(&str, &str)]) -> Settings {
        let pairs = otlp_env(extra);
        let refs: Vec<(&str, &str)> = pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        resolve_env(&refs)
            .expect("settings should resolve")
            .expect("telemetry should be enabled")
    }

    #[test]
    fn disabled_by_default() {
        assert_eq!(resolve_env(&[]).unwrap(), None);
    }

    #[test]
    fn enabled_without_exporters_has_no_work() {
        let settings = resolve_env(&[(ENV_ENABLE, "1")]).unwrap().unwrap();
        assert!(settings.metrics_exporters.is_empty());
        assert!(settings.logs_exporters.is_empty());
    }

    #[test]
    fn env_beats_lua() {
        let lua = TelemetryConfig {
            enabled: Some(true),
            service_name: Some("from-lua".to_string()),
            ..TelemetryConfig::default()
        };
        let settings = resolve(&lua, env_from(&[(ENV_SERVICE_NAME, "from-env")]))
            .unwrap()
            .unwrap();
        assert_eq!(settings.service_name, "from-env");
    }

    #[test]
    fn lua_beats_default() {
        let lua = TelemetryConfig {
            enabled: Some(true),
            service_name: Some("from-lua".to_string()),
            ..TelemetryConfig::default()
        };
        let settings = resolve(&lua, env_from(&[])).unwrap().unwrap();
        assert_eq!(settings.service_name, "from-lua");
    }

    #[test]
    fn env_disable_overrides_lua_enable() {
        let lua = TelemetryConfig {
            enabled: Some(true),
            ..TelemetryConfig::default()
        };
        assert_eq!(resolve(&lua, env_from(&[(ENV_ENABLE, "0")])).unwrap(), None);
    }

    #[test_case("otlp", &[Exporter::Otlp]; "otlp_only")]
    #[test_case("console", &[Exporter::Console]; "console_only")]
    #[test_case("none", &[]; "none_is_empty")]
    #[test_case("otlp,console", &[Exporter::Otlp, Exporter::Console]; "comma_separated")]
    #[test_case(" otlp , console ", &[Exporter::Otlp, Exporter::Console]; "whitespace_tolerant")]
    fn parses_exporter_lists(raw: &str, expected: &[Exporter]) {
        let lua = TelemetryConfig {
            enabled: Some(true),
            ..TelemetryConfig::default()
        };
        let settings = resolve(
            &lua,
            env_from(&[
                (ENV_METRICS_EXPORTER, raw),
                (ENV_PROTOCOL, "http/json"),
                (ENV_ENDPOINT, ENDPOINT),
            ]),
        )
        .unwrap()
        .unwrap();
        assert_eq!(settings.metrics_exporters, expected);
    }

    #[test]
    fn rejects_unknown_exporter() {
        let err =
            resolve_env(&[(ENV_ENABLE, "1"), (ENV_METRICS_EXPORTER, "prometheus")]).unwrap_err();
        assert_eq!(
            err,
            SettingsError::Invalid {
                origin: Source::Env,
                key: ENV_METRICS_EXPORTER.to_string(),
                value: "prometheus".to_string(),
                expected: EXPECTED_EXPORTER,
            }
        );
    }

    #[test]
    fn otlp_without_protocol_is_an_error() {
        let err = resolve_env(&[(ENV_ENABLE, "1"), (ENV_METRICS_EXPORTER, "otlp")]).unwrap_err();
        assert_eq!(
            err,
            SettingsError::MissingProtocol {
                exporter: ENV_METRICS_EXPORTER
            }
        );
    }

    #[test]
    fn otlp_without_endpoint_is_an_error() {
        let err = resolve_env(&[
            (ENV_ENABLE, "1"),
            (ENV_METRICS_EXPORTER, "otlp"),
            (ENV_PROTOCOL, "grpc"),
        ])
        .unwrap_err();
        assert_eq!(
            err,
            SettingsError::MissingEndpoint {
                exporter: ENV_METRICS_EXPORTER
            }
        );
    }

    #[test]
    fn console_exporter_needs_no_endpoint() {
        let settings = resolve_env(&[(ENV_ENABLE, "1"), (ENV_METRICS_EXPORTER, "console")])
            .unwrap()
            .unwrap();
        assert!(settings.metrics.is_none());
        assert_eq!(settings.metrics_exporters, vec![Exporter::Console]);
    }

    #[test_case(ENDPOINT, "http://localhost:4318/v1/metrics"; "plain")]
    #[test_case("http://localhost:4318/", "http://localhost:4318/v1/metrics"; "trailing_slash")]
    #[test_case("http://host/otlp", "http://host/otlp/v1/metrics"; "with_base_path")]
    fn http_generic_endpoint_gets_signal_path(endpoint: &str, expected: &str) {
        let settings = resolve_otlp(&[(ENV_ENDPOINT, endpoint)]);
        assert_eq!(settings.metrics.unwrap().url, expected);
    }

    #[test]
    fn http_per_signal_endpoint_is_used_verbatim() {
        let settings =
            resolve_otlp(&[("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", "http://host/custom/")]);
        assert_eq!(settings.metrics.unwrap().url, "http://host/custom/");
    }

    #[test]
    fn grpc_endpoint_gets_the_service_method() {
        let settings = resolve_otlp(&[(ENV_PROTOCOL, "grpc"), (ENV_ENDPOINT, GRPC_ENDPOINT)]);
        let metrics = settings.metrics.unwrap();
        assert_eq!(metrics.protocol, Protocol::Grpc);
        assert_eq!(
            metrics.url,
            format!("{GRPC_ENDPOINT}{}", Signal::Metrics.grpc_path())
        );
    }

    #[test]
    fn per_signal_headers_merge_over_generic() {
        let settings = resolve_otlp(&[
            (ENV_HEADERS, "a=1,b=2"),
            ("OTEL_EXPORTER_OTLP_METRICS_HEADERS", "b=3,c=4"),
        ]);
        assert_eq!(
            settings.metrics.unwrap().headers,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "3".to_string()),
                ("c".to_string(), "4".to_string()),
            ]
        );
    }

    #[test]
    fn header_values_are_percent_decoded() {
        let settings = resolve_otlp(&[(ENV_HEADERS, "authorization=Bearer%20tok%2Fen")]);
        assert_eq!(
            settings.metrics.unwrap().headers,
            vec![("authorization".to_string(), "Bearer tok/en".to_string())]
        );
    }

    #[test]
    fn per_signal_timeout_overrides_generic() {
        let settings = resolve_otlp(&[
            (ENV_TIMEOUT, "1000"),
            ("OTEL_EXPORTER_OTLP_METRICS_TIMEOUT", "2000"),
        ]);
        assert_eq!(
            settings.metrics.unwrap().timeout,
            Duration::from_millis(2000)
        );
    }

    #[test]
    fn blrp_schedule_delay_is_an_alias_for_the_logs_interval() {
        let settings = resolve_otlp(&[(ENV_BLRP_SCHEDULE_DELAY, "250")]);
        assert_eq!(settings.logs_interval, Duration::from_millis(250));
    }

    #[test]
    fn logs_export_interval_wins_over_the_alias() {
        let settings = resolve_otlp(&[
            (ENV_BLRP_SCHEDULE_DELAY, "250"),
            (ENV_LOGS_EXPORT_INTERVAL, "700"),
        ]);
        assert_eq!(settings.logs_interval, Duration::from_millis(700));
    }

    #[test_case("1", true; "one")]
    #[test_case("true", true; "literal_true")]
    #[test_case("TRUE", true; "uppercase")]
    #[test_case("0", false; "zero")]
    #[test_case("off", false; "off")]
    fn parses_booleans(raw: &str, expected: bool) {
        let settings = resolve_otlp(&[(ENV_LOG_USER_PROMPTS, raw)]);
        assert_eq!(settings.log_user_prompts, expected);
    }

    #[test]
    fn rejects_non_boolean_flags() {
        let err = resolve_env(&[(ENV_ENABLE, "1"), (ENV_LOG_TOOL_DETAILS, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            SettingsError::Invalid {
                origin: Source::Env,
                key: ENV_LOG_TOOL_DETAILS.to_string(),
                value: "maybe".to_string(),
                expected: EXPECTED_BOOL,
            }
        );
    }

    #[test]
    fn rejects_non_numeric_intervals() {
        let err =
            resolve_env(&[(ENV_ENABLE, "1"), (ENV_METRIC_EXPORT_INTERVAL, "1m")]).unwrap_err();
        assert_eq!(
            err,
            SettingsError::Invalid {
                origin: Source::Env,
                key: ENV_METRIC_EXPORT_INTERVAL.to_string(),
                value: "1m".to_string(),
                expected: EXPECTED_MILLIS,
            }
        );
    }

    #[test]
    fn defaults_match_the_spec() {
        let settings = resolve_env(&[(ENV_ENABLE, "1")]).unwrap().unwrap();
        assert_eq!(
            settings.metrics_interval,
            Duration::from_millis(DEFAULT_METRICS_INTERVAL_MS)
        );
        assert_eq!(
            settings.logs_interval,
            Duration::from_millis(DEFAULT_LOGS_INTERVAL_MS)
        );
        assert_eq!(settings.logs_max_queue_size, DEFAULT_LOGS_MAX_QUEUE_SIZE);
        assert_eq!(
            settings.logs_max_export_batch_size,
            DEFAULT_LOGS_MAX_EXPORT_BATCH_SIZE
        );
        assert_eq!(settings.temporality, Temporality::Delta);
        assert_eq!(settings.service_name, DEFAULT_SERVICE_NAME);
        assert_eq!(settings.content_max_length, DEFAULT_CONTENT_MAX_LENGTH);
        assert!(settings.metrics_include_session_id);
        assert!(!settings.metrics_include_version);
        assert!(!settings.log_user_prompts);
        assert!(!settings.log_tool_details);
    }

    #[test]
    fn resource_attributes_come_from_the_env_list() {
        let settings = resolve_otlp(&[(ENV_RESOURCE_ATTRIBUTES, "team=core,env=prod")]);
        assert_eq!(
            settings.resource_attributes,
            vec![
                ("team".to_string(), "core".to_string()),
                ("env".to_string(), "prod".to_string()),
            ]
        );
    }

    #[test]
    fn lua_tables_supply_headers_and_resource_attributes() {
        let lua = TelemetryConfig {
            enabled: Some(true),
            metrics_exporter: Some("otlp".to_string()),
            protocol: Some("http/json".to_string()),
            endpoint: Some(ENDPOINT.to_string()),
            headers: Some(BTreeMap::from([("x-key".to_string(), "v".to_string())])),
            resource_attributes: Some(BTreeMap::from([("team".to_string(), "core".to_string())])),
            ..TelemetryConfig::default()
        };
        let settings = resolve(&lua, env_from(&[])).unwrap().unwrap();
        assert_eq!(
            settings.metrics.unwrap().headers,
            vec![("x-key".to_string(), "v".to_string())]
        );
        assert_eq!(
            settings.resource_attributes,
            vec![("team".to_string(), "core".to_string())]
        );
    }

    #[test]
    fn gzip_compression_applies_to_every_signal() {
        let settings = resolve_otlp(&[(ENV_COMPRESSION, "gzip"), (ENV_LOGS_EXPORTER, "otlp")]);
        assert_eq!(settings.metrics.unwrap().compression, Compression::Gzip);
        assert_eq!(settings.logs.unwrap().compression, Compression::Gzip);
    }

    #[test]
    fn generic_env_endpoint_beats_a_per_signal_lua_one() {
        let lua = TelemetryConfig {
            enabled: Some(true),
            metrics_exporter: Some("otlp".to_string()),
            protocol: Some("http/protobuf".to_string()),
            metrics_endpoint: Some("http://from-lua:4318/v1/metrics".to_string()),
            ..TelemetryConfig::default()
        };
        let settings = resolve(&lua, env_from(&[(ENV_ENDPOINT, "http://from-env:4318")]))
            .unwrap()
            .unwrap();
        assert_eq!(
            settings.metrics.unwrap().url,
            "http://from-env:4318/v1/metrics"
        );
    }

    #[test]
    fn a_per_signal_lua_endpoint_is_still_used_verbatim() {
        let lua = TelemetryConfig {
            enabled: Some(true),
            metrics_exporter: Some("otlp".to_string()),
            protocol: Some("http/protobuf".to_string()),
            metrics_endpoint: Some("http://collector/custom".to_string()),
            ..TelemetryConfig::default()
        };
        let settings = resolve(&lua, env_from(&[])).unwrap().unwrap();
        assert_eq!(settings.metrics.unwrap().url, "http://collector/custom");
    }

    #[test]
    fn sdk_disabled_wins_over_a_lua_opt_in() {
        let lua = TelemetryConfig {
            enabled: Some(true),
            ..TelemetryConfig::default()
        };
        let env = env_from(&[(ENV_SDK_DISABLED, "true"), (ENV_ENABLE, "1")]);
        assert_eq!(resolve(&lua, env).unwrap(), None);
    }

    #[test]
    fn zero_durations_are_floored() {
        let settings = resolve_otlp(&[
            (ENV_METRIC_EXPORT_INTERVAL, "0"),
            (ENV_LOGS_EXPORT_INTERVAL, "0"),
            (ENV_METRIC_EXPORT_TIMEOUT, "0"),
            (ENV_TIMEOUT, "0"),
        ]);
        let floor = Duration::from_millis(MIN_DURATION_MS);
        assert_eq!(settings.metrics_interval, floor);
        assert_eq!(settings.logs_interval, floor);
        assert_eq!(settings.metrics_export_timeout, floor);
        assert_eq!(settings.metrics.unwrap().timeout, floor);
    }

    #[test]
    fn a_repeated_exporter_is_only_built_once() {
        let settings = resolve_otlp(&[(ENV_METRICS_EXPORTER, "otlp,console,otlp")]);
        assert_eq!(
            settings.metrics_exporters,
            vec![Exporter::Otlp, Exporter::Console]
        );
    }

    #[test]
    fn an_explicit_service_name_beats_the_resource_attribute() {
        let settings = resolve_otlp(&[
            (ENV_SERVICE_NAME, "chosen"),
            (ENV_RESOURCE_ATTRIBUTES, "service.name=legacy,team=core"),
        ]);
        assert_eq!(settings.service_name, "chosen");
        assert_eq!(
            settings.resource_attributes,
            vec![("team".to_string(), "core".to_string())]
        );
    }

    #[test]
    fn the_resource_attribute_names_the_service_when_nothing_else_does() {
        let settings = resolve_otlp(&[(ENV_RESOURCE_ATTRIBUTES, "service.name=legacy")]);
        assert_eq!(settings.service_name, DEFAULT_SERVICE_NAME);
        assert_eq!(
            settings.resource_attributes,
            vec![("service.name".to_string(), "legacy".to_string())]
        );
    }

    #[test]
    fn protocol_and_compression_ignore_case() {
        let settings = resolve_otlp(&[(ENV_PROTOCOL, "HTTP/Protobuf"), (ENV_COMPRESSION, "GZIP")]);
        let metrics = settings.metrics.unwrap();
        assert_eq!(metrics.protocol, Protocol::HttpProtobuf);
        assert_eq!(metrics.compression, Compression::Gzip);
    }
}
