//! Builds the SDK providers. `init.lua` arrives without the keys whose
//! variables are set (see `overrides`), so each key left is one builder call,
//! and anything unset is up to the SDK and its environment variables.

use std::borrow::Cow;
use std::collections::HashMap;
use std::env;
use std::str::FromStr;
use std::time::Duration;

use async_compat::Compat;
use http::{HeaderMap, HeaderValue};
use maki_config::TelemetryConfig;
use opentelemetry_otlp::tonic_types::metadata::MetadataMap;
use opentelemetry_otlp::tonic_types::transport::ClientTlsConfig;
use opentelemetry_otlp::{
    Compression, LogExporter, MetricExporter, OTEL_EXPORTER_OTLP_COMPRESSION,
    OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_EXPORTER_OTLP_HEADERS, OTEL_EXPORTER_OTLP_INSECURE,
    OTEL_EXPORTER_OTLP_LOGS_COMPRESSION, OTEL_EXPORTER_OTLP_LOGS_ENDPOINT,
    OTEL_EXPORTER_OTLP_LOGS_HEADERS, OTEL_EXPORTER_OTLP_LOGS_INSECURE,
    OTEL_EXPORTER_OTLP_LOGS_PROTOCOL, OTEL_EXPORTER_OTLP_LOGS_TIMEOUT,
    OTEL_EXPORTER_OTLP_METRICS_COMPRESSION, OTEL_EXPORTER_OTLP_METRICS_ENDPOINT,
    OTEL_EXPORTER_OTLP_METRICS_HEADERS, OTEL_EXPORTER_OTLP_METRICS_INSECURE,
    OTEL_EXPORTER_OTLP_METRICS_PROTOCOL, OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE,
    OTEL_EXPORTER_OTLP_METRICS_TIMEOUT, OTEL_EXPORTER_OTLP_PROTOCOL,
    OTEL_EXPORTER_OTLP_PROTOCOL_GRPC, OTEL_EXPORTER_OTLP_PROTOCOL_HTTP_JSON,
    OTEL_EXPORTER_OTLP_PROTOCOL_HTTP_PROTOBUF, OTEL_EXPORTER_OTLP_TIMEOUT, Protocol,
    WithExportConfig, WithHttpConfig, WithTonicConfig,
};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::{
    BatchConfig, BatchConfigBuilder, BatchLogProcessor, LogExporter as SdkLogExporter,
    OTEL_BLRP_MAX_QUEUE_SIZE, OTEL_BLRP_SCHEDULE_DELAY, SdkLoggerProvider,
};
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::metrics::{
    OTEL_METRIC_EXPORT_INTERVAL, PeriodicReader, SdkMeterProvider, Temporality,
};
use percent_encoding::percent_decode_str;

use crate::InitError;
use crate::console::{ConsoleLogs, ConsoleMetrics};
use crate::resource;
use crate::settings::{Exporter, InvalidSetting, Settings};

/// Claude Code's name for `OTEL_BLRP_SCHEDULE_DELAY`, which the SDK does not
/// read, so maki does and it wins over the standard one.
pub(crate) const ENV_LOGS_EXPORT_INTERVAL: &str = "OTEL_LOGS_EXPORT_INTERVAL";

/// The SDK allocates the event queue up front, whether its size comes from
/// `init.lua` or `OTEL_BLRP_MAX_QUEUE_SIZE`, so a huge size would cost
/// gigabytes or panic at startup.
const MAX_LOGS_QUEUE_SIZE: usize = 65_536;
/// Claude Code's default, unlike the SDK's cumulative, which would keep every
/// session's series in a long-running ACP process until the SDK's limit of 2000
/// per metric.
const DEFAULT_TEMPORALITY: Temporality = Temporality::Delta;

const LUA_PROTOCOL: &str = "telemetry.protocol";
const LUA_METRICS_PROTOCOL: &str = "telemetry.metrics_protocol";
const LUA_LOGS_PROTOCOL: &str = "telemetry.logs_protocol";
const LUA_COMPRESSION: &str = "telemetry.compression";
const LUA_TEMPORALITY: &str = "telemetry.metrics_temporality";
const LUA_LOGS_MAX_QUEUE_SIZE: &str = "telemetry.logs_max_queue_size";
const LUA_METRICS_INTERVAL: &str = "telemetry.metrics_interval_ms";
const LUA_LOGS_INTERVAL: &str = "telemetry.logs_interval_ms";
const METRICS_PATH: &str = "/v1/metrics";
const LOGS_PATH: &str = "/v1/logs";
const PROTOCOLS: [&str; 3] = [
    OTEL_EXPORTER_OTLP_PROTOCOL_GRPC,
    OTEL_EXPORTER_OTLP_PROTOCOL_HTTP_PROTOBUF,
    OTEL_EXPORTER_OTLP_PROTOCOL_HTTP_JSON,
];
/// The spec's value for no compression, which the SDK accepts only from the
/// environment.
const COMPRESSION_NONE: &str = "none";
/// The only codec compiled in; `zstd` would need another SDK feature.
const COMPRESSION_GZIP: &str = "gzip";
const SCHEME_SEPARATOR: &str = "://";
const TLS_SCHEME: &str = "https";
const INSECURE_TRUE: &str = "true";
const HEADER_SEPARATOR: char = ',';
const HEADER_KEY_VALUE: char = '=';
const EXPECTED_PROTOCOL: &str = "grpc, http/protobuf or http/json";
const EXPECTED_COMPRESSION: &str = "gzip or none";
const EXPECTED_TEMPORALITY: &str = "cumulative, delta or lowmemory";
const EXPECTED_QUEUE_SIZE: &str = "at most 65536";
const EXPECTED_INTERVAL: &str = "above 0";

type Providers = (Option<SdkMeterProvider>, Option<SdkLoggerProvider>);

/// Applies a builder call only for a value that is set.
pub(crate) trait Set: Sized {
    fn set<T>(self, value: Option<T>, call: impl FnOnce(Self, T) -> Self) -> Self {
        match value {
            Some(value) => call(self, value),
            None => self,
        }
    }
}

impl<B> Set for B {}

#[derive(Clone, Copy)]
enum Signal {
    Metrics,
    Logs,
}

impl Signal {
    fn lua_protocol_key(self) -> &'static str {
        match self {
            Self::Metrics => LUA_METRICS_PROTOCOL,
            Self::Logs => LUA_LOGS_PROTOCOL,
        }
    }

    /// The signal's variable first, as the SDK reads them.
    fn env(self, var: OtlpVar) -> [&'static str; 2] {
        let own = match (self, var) {
            (Self::Metrics, OtlpVar::Protocol) => OTEL_EXPORTER_OTLP_METRICS_PROTOCOL,
            (Self::Metrics, OtlpVar::Endpoint) => OTEL_EXPORTER_OTLP_METRICS_ENDPOINT,
            (Self::Metrics, OtlpVar::Headers) => OTEL_EXPORTER_OTLP_METRICS_HEADERS,
            (Self::Metrics, OtlpVar::Timeout) => OTEL_EXPORTER_OTLP_METRICS_TIMEOUT,
            (Self::Metrics, OtlpVar::Compression) => OTEL_EXPORTER_OTLP_METRICS_COMPRESSION,
            (Self::Metrics, OtlpVar::Insecure) => OTEL_EXPORTER_OTLP_METRICS_INSECURE,
            (Self::Logs, OtlpVar::Protocol) => OTEL_EXPORTER_OTLP_LOGS_PROTOCOL,
            (Self::Logs, OtlpVar::Endpoint) => OTEL_EXPORTER_OTLP_LOGS_ENDPOINT,
            (Self::Logs, OtlpVar::Headers) => OTEL_EXPORTER_OTLP_LOGS_HEADERS,
            (Self::Logs, OtlpVar::Timeout) => OTEL_EXPORTER_OTLP_LOGS_TIMEOUT,
            (Self::Logs, OtlpVar::Compression) => OTEL_EXPORTER_OTLP_LOGS_COMPRESSION,
            (Self::Logs, OtlpVar::Insecure) => OTEL_EXPORTER_OTLP_LOGS_INSECURE,
        };
        let generic = match var {
            OtlpVar::Protocol => OTEL_EXPORTER_OTLP_PROTOCOL,
            OtlpVar::Endpoint => OTEL_EXPORTER_OTLP_ENDPOINT,
            OtlpVar::Headers => OTEL_EXPORTER_OTLP_HEADERS,
            OtlpVar::Timeout => OTEL_EXPORTER_OTLP_TIMEOUT,
            OtlpVar::Compression => OTEL_EXPORTER_OTLP_COMPRESSION,
            OtlpVar::Insecure => OTEL_EXPORTER_OTLP_INSECURE,
        };
        [own, generic]
    }

    fn sets_own(self, var: OtlpVar) -> bool {
        is_set(self.env(var)[0])
    }

    /// A generic `init.lua` key loses to the signal's own variable too, which
    /// the SDK reads over anything set in code.
    fn generic<T>(self, value: Option<T>, var: OtlpVar) -> Option<T> {
        value.filter(|_| !self.sets_own(var))
    }

    fn http_path(self) -> &'static str {
        match self {
            Self::Metrics => METRICS_PATH,
            Self::Logs => LOGS_PATH,
        }
    }
}

#[derive(Clone, Copy)]
enum OtlpVar {
    Protocol,
    Endpoint,
    Headers,
    Timeout,
    Compression,
    Insecure,
}

/// `Http(None)` lets the SDK pick between protobuf and JSON.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Transport {
    Grpc,
    Http(Option<Protocol>),
}

/// The `init.lua` OTLP keys of one signal, per-signal keys over generic ones.
struct Otlp {
    signal: Signal,
    transport: Transport,
    endpoint: Option<String>,
    headers: HashMap<String, String>,
    timeout: Option<Duration>,
    compression: Option<Compression>,
}

pub(crate) fn providers(
    settings: &Settings,
    lua: &TelemetryConfig,
) -> Result<Providers, InitError> {
    let resource = resource::build(lua);
    Ok((
        meter_provider(&settings.metrics_exporters, lua, resource.clone())?,
        logger_provider(&settings.logs_exporters, lua, resource)?,
    ))
}

fn meter_provider(
    exporters: &[Exporter],
    lua: &TelemetryConfig,
    resource: Resource,
) -> Result<Option<SdkMeterProvider>, InitError> {
    if exporters.is_empty() {
        return Ok(None);
    }
    let interval = interval(
        lua.metrics_interval_ms,
        LUA_METRICS_INTERVAL,
        &[OTEL_METRIC_EXPORT_INTERVAL],
    )?;
    let temporality = temporality(lua)?;
    let mut builder = SdkMeterProvider::builder().with_resource(resource);
    for exporter in exporters {
        builder = match exporter {
            Exporter::Console => {
                builder.with_reader(periodic_reader(ConsoleMetrics(temporality), interval))
            }
            Exporter::Otlp => {
                builder.with_reader(periodic_reader(otlp_metrics(lua, temporality)?, interval))
            }
        };
    }
    Ok(Some(builder.build()))
}

/// Set explicitly, so the console exporter, which does not read the variable,
/// gets the same one. An invalid variable falls back to the default with a
/// warning, as the SDK's own lookup does.
fn temporality(lua: &TelemetryConfig) -> Result<Temporality, InvalidSetting> {
    if let Some(value) = lua.metrics_temporality.as_deref() {
        return value
            .parse()
            .map_err(|()| InvalidSetting::new(LUA_TEMPORALITY, value, EXPECTED_TEMPORALITY));
    }
    let env = OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE;
    Ok(env::var(env)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map_or(DEFAULT_TEMPORALITY, |value| {
            value.parse().unwrap_or_else(|()| {
                tracing::warn!(
                    otel.env = env,
                    otel.value = value,
                    otel.expected = EXPECTED_TEMPORALITY,
                    "invalid metrics temporality, using delta"
                );
                DEFAULT_TEMPORALITY
            })
        }))
}

fn periodic_reader<E: PushMetricExporter>(
    exporter: E,
    interval: Option<Duration>,
) -> PeriodicReader<E> {
    PeriodicReader::builder(exporter)
        .set(interval, |builder, interval| {
            builder.with_interval(interval)
        })
        .build()
}

fn logger_provider(
    exporters: &[Exporter],
    lua: &TelemetryConfig,
    resource: Resource,
) -> Result<Option<SdkLoggerProvider>, InitError> {
    if exporters.is_empty() {
        return Ok(None);
    }
    let mut builder = SdkLoggerProvider::builder().with_resource(resource);
    for exporter in exporters {
        builder = builder.with_log_processor(match exporter {
            Exporter::Console => batch_processor(ConsoleLogs::default(), lua)?,
            Exporter::Otlp => batch_processor(otlp_logs(lua)?, lua)?,
        });
    }
    Ok(Some(builder.build()))
}

fn batch_processor<E: SdkLogExporter + 'static>(
    exporter: E,
    lua: &TelemetryConfig,
) -> Result<BatchLogProcessor, InvalidSetting> {
    Ok(BatchLogProcessor::builder(exporter)
        .with_batch_config(batch_config(lua)?)
        .build())
}

fn batch_config(lua: &TelemetryConfig) -> Result<BatchConfig, InvalidSetting> {
    let queue_size = env_or_lua(
        lua.logs_max_queue_size,
        LUA_LOGS_MAX_QUEUE_SIZE,
        &[OTEL_BLRP_MAX_QUEUE_SIZE],
    );
    if let Some((key, size)) = queue_size.filter(|&(_, size)| size > MAX_LOGS_QUEUE_SIZE) {
        return Err(InvalidSetting::new(
            key,
            &size.to_string(),
            EXPECTED_QUEUE_SIZE,
        ));
    }
    let delay = interval(
        lua.logs_interval_ms,
        LUA_LOGS_INTERVAL,
        &[ENV_LOGS_EXPORT_INTERVAL, OTEL_BLRP_SCHEDULE_DELAY],
    )?;
    Ok(BatchConfigBuilder::default()
        .set(delay, BatchConfigBuilder::with_scheduled_delay)
        .set(
            lua.logs_max_queue_size,
            BatchConfigBuilder::with_max_queue_size,
        )
        .set(
            lua.logs_max_export_batch_size,
            BatchConfigBuilder::with_max_export_batch_size,
        )
        .build())
}

/// Rejects zero, at which the SDK's export thread never sleeps and pins a
/// core.
fn interval(
    lua: Option<u64>,
    lua_key: &'static str,
    env_keys: &[&'static str],
) -> Result<Option<Duration>, InvalidSetting> {
    match env_or_lua(lua, lua_key, env_keys) {
        Some((key, 0)) => Err(InvalidSetting::new(key, "0", EXPECTED_INTERVAL)),
        found => Ok(found.map(|(_, ms)| Duration::from_millis(ms))),
    }
}

/// The first variable that parses, as the SDK skips one that does not, else
/// the `init.lua` value.
fn env_or_lua<T: FromStr>(
    lua: Option<T>,
    lua_key: &'static str,
    env_keys: &[&'static str],
) -> Option<(&'static str, T)> {
    env_keys
        .iter()
        .find_map(|&key| Some((key, env::var(key).ok()?.parse().ok()?)))
        .or_else(|| lua.map(|value| (lua_key, value)))
}

fn otlp_metrics(
    lua: &TelemetryConfig,
    temporality: Temporality,
) -> Result<MetricExporter, InitError> {
    let otlp = Otlp::new(lua, Signal::Metrics)?;
    let builder = MetricExporter::builder().with_temporality(temporality);
    Ok(match otlp.transport {
        Transport::Grpc => {
            let builder = otlp.grpc(builder.with_tonic())?;
            in_tokio(|| builder.build())?
        }
        Transport::Http(protocol) => otlp.http(builder.with_http(), protocol)?.build()?,
    })
}

fn otlp_logs(lua: &TelemetryConfig) -> Result<LogExporter, InitError> {
    let otlp = Otlp::new(lua, Signal::Logs)?;
    let builder = LogExporter::builder();
    Ok(match otlp.transport {
        Transport::Grpc => {
            let builder = otlp.grpc(builder.with_tonic())?;
            in_tokio(|| builder.build())?
        }
        Transport::Http(protocol) => otlp.http(builder.with_http(), protocol)?.build()?,
    })
}

/// tonic's channel spawns its worker onto the current tokio runtime, so a
/// gRPC exporter is built inside async-compat's.
fn in_tokio<T>(build: impl FnOnce() -> T) -> T {
    smol::block_on(Compat::new(async { build() }))
}

impl Otlp {
    fn new(lua: &TelemetryConfig, signal: Signal) -> Result<Self, InvalidSetting> {
        let (protocol, endpoint, headers, timeout_ms) = match signal {
            Signal::Metrics => (
                &lua.metrics_protocol,
                &lua.metrics_endpoint,
                &lua.metrics_headers,
                lua.metrics_timeout_ms,
            ),
            Signal::Logs => (
                &lua.logs_protocol,
                &lua.logs_endpoint,
                &lua.logs_headers,
                lua.logs_timeout_ms,
            ),
        };
        let transport = match parse_transport(signal.lua_protocol_key(), protocol.as_deref())? {
            Some(transport) => transport,
            None => parse_transport(
                LUA_PROTOCOL,
                signal
                    .generic(lua.protocol.as_deref(), OtlpVar::Protocol)
                    .filter(|_| !signal.sets_own(OtlpVar::Endpoint)),
            )?
            .unwrap_or_else(|| env_transport(signal)),
        };
        let generic_endpoint = signal
            .generic(lua.endpoint.as_ref(), OtlpVar::Endpoint)
            .map(|endpoint| match transport {
                Transport::Grpc => endpoint.clone(),
                Transport::Http(_) => {
                    format!("{}{}", endpoint.trim_end_matches('/'), signal.http_path())
                }
            });
        // The SDK reads the signal's header variable instead of the generic
        // one, so the generic one is passed in code to merge them.
        let env_headers = signal
            .sets_own(OtlpVar::Headers)
            .then(|| env::var(OTEL_EXPORTER_OTLP_HEADERS).ok())
            .flatten();
        Ok(Self {
            signal,
            transport,
            endpoint: endpoint.clone().or(generic_endpoint),
            headers: lua
                .headers
                .iter()
                .chain(headers)
                .flatten()
                .map(|(key, value)| (key.clone(), value.clone()))
                .chain(env_headers.as_deref().into_iter().flat_map(parse_headers))
                .collect(),
            timeout: timeout_ms
                .or(signal.generic(lua.timeout_ms, OtlpVar::Timeout))
                .map(Duration::from_millis),
            compression: signal
                .generic(lua.compression.as_deref(), OtlpVar::Compression)
                .map(parse_compression)
                .transpose()?
                .flatten(),
        })
    }

    fn export_config<B: WithExportConfig>(&self, builder: B) -> B {
        builder
            .set(self.endpoint.clone(), |builder, endpoint| {
                builder.with_endpoint(endpoint)
            })
            .set(self.timeout, B::with_timeout)
    }

    /// The SDK drops an invalid HTTP header without a word, so headers are
    /// checked as strictly as gRPC metadata is.
    fn http<B: WithHttpConfig + WithExportConfig>(
        self,
        builder: B,
        protocol: Option<Protocol>,
    ) -> Result<B, InitError> {
        HeaderMap::<HeaderValue>::try_from(&self.headers)?;
        Ok(self
            .export_config(builder)
            .set(protocol, B::with_protocol)
            .set(self.compression, B::with_compression)
            .with_headers(self.headers))
    }

    /// The SDK's default TLS config trusts no roots at all, so an `https://`
    /// collector would fail every handshake without the OS store. Metadata is
    /// percent-decoded here, as the HTTP exporter does with headers.
    fn grpc<B: WithTonicConfig + WithExportConfig>(self, builder: B) -> Result<B, InitError> {
        let roots = self
            .may_use_tls()
            .then(|| ClientTlsConfig::new().with_enabled_roots());
        let metadata: HashMap<_, _> = self
            .headers
            .iter()
            .map(|(key, value)| (key.clone(), percent_decoded(value)))
            .collect();
        Ok(self
            .export_config(builder)
            .set(roots, B::with_tls_config)
            .with_metadata(MetadataMap::from_headers(HeaderMap::try_from(&metadata)?))
            .set(self.compression, B::with_compression))
    }

    /// tonic loads the OS trust store as soon as it gets a TLS config, for any
    /// scheme, fails without one, and refuses one for a `unix:` socket. So
    /// roots only go to the endpoint the SDK will reach over TLS, resolved as
    /// the SDK does. The SDK's default endpoint is plain `http://`.
    fn may_use_tls(&self) -> bool {
        self.endpoint
            .clone()
            .filter(|endpoint| !endpoint.is_empty())
            .or_else(|| {
                env_value(self.signal.env(OtlpVar::Endpoint), |value| {
                    !value.is_empty()
                })
            })
            .is_some_and(|endpoint| {
                let insecure = env_value(self.signal.env(OtlpVar::Insecure), |_| true)
                    .is_some_and(|value| value.eq_ignore_ascii_case(INSECURE_TRUE));
                reached_over_tls(&endpoint, insecure)
            })
    }
}

/// The SDK keeps an explicit scheme and gives a bare `host:port` `https://`,
/// or `http://` when the insecure flag is set.
fn reached_over_tls(endpoint: &str, insecure: bool) -> bool {
    match endpoint.split_once(SCHEME_SEPARATOR) {
        Some((scheme, _)) if !scheme.contains(['/', '?', '#']) => {
            scheme.eq_ignore_ascii_case(TLS_SCHEME)
        }
        _ => !insecure,
    }
}

/// Any case, as the variables accept it.
fn parse_transport(key: &str, value: Option<&str>) -> Result<Option<Transport>, InvalidSetting> {
    value
        .map(|value| match value.to_ascii_lowercase().as_str() {
            OTEL_EXPORTER_OTLP_PROTOCOL_GRPC => Ok(Transport::Grpc),
            OTEL_EXPORTER_OTLP_PROTOCOL_HTTP_PROTOBUF => {
                Ok(Transport::Http(Some(Protocol::HttpBinary)))
            }
            OTEL_EXPORTER_OTLP_PROTOCOL_HTTP_JSON => Ok(Transport::Http(Some(Protocol::HttpJson))),
            _ => Err(InvalidSetting::new(key, value, EXPECTED_PROTOCOL)),
        })
        .transpose()
}

/// `none` leaves compression off.
fn parse_compression(value: &str) -> Result<Option<Compression>, InvalidSetting> {
    match value.to_ascii_lowercase().as_str() {
        COMPRESSION_NONE => Ok(None),
        COMPRESSION_GZIP => Ok(Some(Compression::Gzip)),
        _ => Err(InvalidSetting::new(
            LUA_COMPRESSION,
            value,
            EXPECTED_COMPRESSION,
        )),
    }
}

/// `key=value` pairs as the SDK splits them, values still percent-encoded.
fn parse_headers(raw: &str) -> impl Iterator<Item = (String, String)> {
    raw.split_terminator(HEADER_SEPARATOR).filter_map(|pair| {
        let (key, value) = pair.split_once(HEADER_KEY_VALUE)?;
        let (key, value) = (key.trim(), value.trim());
        (!key.is_empty() && !value.is_empty()).then(|| (key.to_string(), value.to_string()))
    })
}

/// An invalid escape stays as written.
fn percent_decoded(value: &str) -> String {
    percent_decode_str(value)
        .decode_utf8()
        .map_or_else(|_| value.to_string(), Cow::into_owned)
}

/// Set to anything but blanks, as the SDK reads its variables.
pub(crate) fn is_set(key: &str) -> bool {
    env::var(key).is_ok_and(|value| !value.trim().is_empty())
}

/// The SDK picks the transport from the environment only when it builds the
/// exporter itself. A gRPC exporter built that way has no tokio runtime and
/// panics, and it gets no TLS roots. So maki looks for gRPC up front, skipping
/// unknown values as the SDK does, and leaves the HTTP flavour to the SDK.
fn env_transport(signal: Signal) -> Transport {
    let grpc = env_value(signal.env(OtlpVar::Protocol), |value| {
        PROTOCOLS
            .iter()
            .any(|protocol| value.eq_ignore_ascii_case(protocol))
    })
    .is_some_and(|value| value.eq_ignore_ascii_case(OTEL_EXPORTER_OTLP_PROTOCOL_GRPC));
    if grpc {
        Transport::Grpc
    } else {
        Transport::Http(None)
    }
}

/// The first variable whose value the SDK would accept.
fn env_value(keys: [&str; 2], accept: impl Fn(&str) -> bool) -> Option<String> {
    keys.into_iter()
        .find_map(|key| env::var(key).ok().filter(|value| accept(value)))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use test_case::test_case;

    use super::*;

    const OTLP: &str = "otlp";
    const CONSOLE: &str = "console";
    const GRPC: &str = "grpc";
    const HTTP_PROTOBUF: &str = "http/protobuf";
    const ENDPOINT: &str = "http://collector:4318";
    const TIMEOUT_MS: u64 = 5_000;
    /// Read by maki and by the OpenTelemetry SDK.
    const ENV_PREFIXES: [&str; 2] = ["OTEL_", "MAKI_"];

    fn lua(protocol: &str) -> TelemetryConfig {
        TelemetryConfig {
            protocol: Some(protocol.to_string()),
            endpoint: Some(ENDPOINT.to_string()),
            ..TelemetryConfig::default()
        }
    }

    #[test_case(HTTP_PROTOBUF, "http://collector:4318/v1/metrics"; "http_appends_the_signal_path")]
    #[test_case(GRPC, ENDPOINT; "grpc_uses_it_as_given")]
    fn generic_lua_endpoint(protocol: &str, expected: &str) {
        isolate_env();
        let otlp = Otlp::new(&lua(protocol), Signal::Metrics).unwrap();
        assert_eq!(otlp.endpoint.as_deref(), Some(expected));
    }

    #[test]
    fn per_signal_lua_keys_win_and_headers_merge() {
        isolate_env();
        let config = TelemetryConfig {
            logs_protocol: Some(GRPC.to_string()),
            logs_endpoint: Some("http://logs/custom".to_string()),
            headers: Some(BTreeMap::from([
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string()),
            ])),
            logs_headers: Some(BTreeMap::from([("b".to_string(), "3".to_string())])),
            ..lua(HTTP_PROTOBUF)
        };
        let otlp = Otlp::new(&config, Signal::Logs).unwrap();
        assert_eq!(otlp.transport, Transport::Grpc);
        assert_eq!(otlp.endpoint.as_deref(), Some("http://logs/custom"));
        assert_eq!(
            otlp.headers,
            HashMap::from([
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "3".to_string()),
            ])
        );
    }

    #[test]
    fn generic_lua_keys_yield_to_a_signal_variable() {
        isolate_env();
        for env in [
            OTEL_EXPORTER_OTLP_METRICS_PROTOCOL,
            OTEL_EXPORTER_OTLP_METRICS_ENDPOINT,
            OTEL_EXPORTER_OTLP_METRICS_TIMEOUT,
            OTEL_EXPORTER_OTLP_METRICS_COMPRESSION,
        ] {
            unsafe { env::set_var(env, HTTP_PROTOBUF) };
        }
        let config = TelemetryConfig {
            timeout_ms: Some(TIMEOUT_MS),
            compression: Some(COMPRESSION_GZIP.to_string()),
            ..lua(GRPC)
        };

        let metrics = Otlp::new(&config, Signal::Metrics).unwrap();
        assert_eq!(metrics.transport, Transport::Http(None));
        assert_eq!(metrics.endpoint, None);
        assert_eq!(metrics.timeout, None);
        assert_eq!(metrics.compression, None);

        let logs = Otlp::new(&config, Signal::Logs).unwrap();
        assert_eq!(logs.transport, Transport::Grpc);
        assert_eq!(logs.endpoint.as_deref(), Some(ENDPOINT));
        assert_eq!(logs.timeout, Some(Duration::from_millis(TIMEOUT_MS)));
        assert_eq!(logs.compression, Some(Compression::Gzip));
    }

    #[test_case(Signal::Metrics, Transport::Http(None); "own_endpoint_variable_drops_it")]
    #[test_case(Signal::Logs, Transport::Grpc; "other_signal_keeps_it")]
    fn generic_lua_protocol_with_a_signal_endpoint_variable(signal: Signal, expected: Transport) {
        isolate_env();
        unsafe { env::set_var(OTEL_EXPORTER_OTLP_METRICS_ENDPOINT, ENDPOINT) };
        assert_eq!(Otlp::new(&lua(GRPC), signal).unwrap().transport, expected);
    }

    /// The SDK reads either header variable, never both.
    #[test_case(Signal::Metrics, &[("authorization", "Bearer%20x")]; "merged_under_the_signal_variable")]
    #[test_case(Signal::Logs, &[]; "left_to_the_sdk")]
    fn generic_header_variable(signal: Signal, expected: &[(&str, &str)]) {
        isolate_env();
        unsafe {
            env::set_var(OTEL_EXPORTER_OTLP_HEADERS, "authorization=Bearer%20x");
            env::set_var(OTEL_EXPORTER_OTLP_METRICS_HEADERS, "x-team=core");
        }
        let otlp = Otlp::new(&TelemetryConfig::default(), signal).unwrap();
        let expected: HashMap<_, _> = expected
            .iter()
            .map(|&(key, value)| (key.to_string(), value.to_string()))
            .collect();
        assert_eq!(otlp.headers, expected);
    }

    #[test_case("a=1, b = 2 ,=3,c=,d", &[("a", "1"), ("b", "2")]; "skips_what_the_sdk_skips")]
    #[test_case("", &[]; "empty")]
    fn parses_headers_like_the_sdk(raw: &str, expected: &[(&str, &str)]) {
        let expected: Vec<_> = expected
            .iter()
            .map(|&(key, value)| (key.to_string(), value.to_string()))
            .collect();
        assert_eq!(parse_headers(raw).collect::<Vec<_>>(), expected);
    }

    #[test_case("Bearer%20x", "Bearer x"; "escape")]
    #[test_case("100%", "100%"; "invalid_escape_stays")]
    #[test_case("plain", "plain"; "plain")]
    fn percent_decodes_metadata(raw: &str, expected: &str) {
        assert_eq!(percent_decoded(raw), expected);
    }

    #[test_case("GRPC", Transport::Grpc; "grpc")]
    #[test_case("HTTP/JSON", Transport::Http(Some(Protocol::HttpJson)); "http_json")]
    fn lua_protocol_in_any_case(raw: &str, expected: Transport) {
        assert_eq!(parse_transport(LUA_PROTOCOL, Some(raw)), Ok(Some(expected)));
    }

    #[test]
    fn lua_compression_in_any_case() {
        assert_eq!(parse_compression("GZIP"), Ok(Some(Compression::Gzip)));
    }

    #[test_case(None, None, Temporality::Delta; "delta_by_default")]
    #[test_case(None, Some("Cumulative"), Temporality::Cumulative; "variable")]
    #[test_case(None, Some("sometimes"), Temporality::Delta; "invalid_variable_falls_back")]
    #[test_case(Some("lowmemory"), None, Temporality::LowMemory; "lua")]
    fn temporality_is(lua: Option<&str>, env: Option<&str>, expected: Temporality) {
        isolate_env();
        if let Some(env) = env {
            unsafe { env::set_var(OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE, env) };
        }
        let config = TelemetryConfig {
            metrics_temporality: lua.map(str::to_string),
            ..TelemetryConfig::default()
        };
        assert_eq!(temporality(&config), Ok(expected));
    }

    #[test_case(&[(OTEL_BLRP_SCHEDULE_DELAY, "500")], Ok(Some(500)); "standard_variable")]
    #[test_case(&[(ENV_LOGS_EXPORT_INTERVAL, "250"), (OTEL_BLRP_SCHEDULE_DELAY, "500")], Ok(Some(250)); "claude_code_variable_first")]
    #[test_case(&[(ENV_LOGS_EXPORT_INTERVAL, "0")], Err(ENV_LOGS_EXPORT_INTERVAL); "zero_is_rejected")]
    fn logs_interval(env: &[(&str, &str)], expected: Result<Option<u64>, &str>) {
        isolate_env();
        for (key, value) in env {
            unsafe { env::set_var(key, value) };
        }
        let result = interval(
            None,
            LUA_LOGS_INTERVAL,
            &[ENV_LOGS_EXPORT_INTERVAL, OTEL_BLRP_SCHEDULE_DELAY],
        );
        assert_eq!(
            result.map(|delay| delay.map(|delay| delay.as_millis() as u64)),
            expected.map_err(|key| InvalidSetting::new(key, "0", EXPECTED_INTERVAL))
        );
    }

    #[test_case("http://collector:4317", false, false; "http")]
    #[test_case("unix:///run/otel.sock", false, false; "unix_socket")]
    #[test_case("https://collector:4317", false, true; "https")]
    #[test_case("HTTPS://collector:4317", false, true; "https_uppercase")]
    #[test_case("https://collector:4317", true, true; "https_wins_over_insecure")]
    #[test_case("collector:4317", false, true; "schemeless")]
    #[test_case("collector:4317", true, false; "schemeless_insecure")]
    #[test_case("collector:4317/a://b", true, false; "separator_after_a_path")]
    fn grpc_roots_only_where_the_sdk_uses_tls(endpoint: &str, insecure: bool, expected: bool) {
        assert_eq!(reached_over_tls(endpoint, insecure), expected);
    }

    #[test]
    fn rejects_an_invalid_http_header() {
        let config = TelemetryConfig {
            headers: Some(BTreeMap::from([(
                "authorization".to_string(),
                "Bearer a\nb".to_string(),
            )])),
            ..lua(HTTP_PROTOBUF)
        };
        let otlp = Otlp::new(&config, Signal::Metrics).unwrap();
        assert!(matches!(
            otlp.http(MetricExporter::builder().with_http(), None),
            Err(InitError::Headers(_))
        ));
    }

    #[test]
    fn lua_compression_none_leaves_it_unset() {
        let config = TelemetryConfig {
            compression: Some(COMPRESSION_NONE.to_string()),
            ..lua(GRPC)
        };
        assert_eq!(Otlp::new(&config, Signal::Logs).unwrap().compression, None);
    }

    #[test]
    fn rejects_compression_that_is_not_compiled_in() {
        let config = TelemetryConfig {
            compression: Some("zstd".to_string()),
            ..lua(GRPC)
        };
        let err = Otlp::new(&config, Signal::Logs).err().unwrap();
        assert_eq!(
            err,
            InvalidSetting::new(LUA_COMPRESSION, "zstd", EXPECTED_COMPRESSION)
        );
    }

    #[test]
    fn rejects_an_unknown_lua_protocol() {
        let err = Otlp::new(&lua("udp"), Signal::Metrics).err().unwrap();
        assert_eq!(
            err,
            InvalidSetting::new(LUA_PROTOCOL, "udp", EXPECTED_PROTOCOL)
        );
    }

    #[test]
    fn rejects_a_queue_the_sdk_cannot_allocate() {
        let size = MAX_LOGS_QUEUE_SIZE + 1;
        let config = TelemetryConfig {
            logs_max_queue_size: Some(size),
            ..TelemetryConfig::default()
        };
        assert_eq!(
            batch_config(&config).err().unwrap(),
            InvalidSetting::new(
                LUA_LOGS_MAX_QUEUE_SIZE,
                &size.to_string(),
                EXPECTED_QUEUE_SIZE
            )
        );
    }

    #[test]
    fn rejects_a_zero_logs_interval() {
        isolate_env();
        let config = TelemetryConfig {
            logs_interval_ms: Some(0),
            ..TelemetryConfig::default()
        };
        assert_eq!(
            batch_config(&config).err().unwrap(),
            InvalidSetting::new(LUA_LOGS_INTERVAL, "0", EXPECTED_INTERVAL)
        );
    }

    #[test]
    fn rejects_a_zero_metrics_interval() {
        let config = TelemetryConfig {
            metrics_interval_ms: Some(0),
            ..TelemetryConfig::default()
        };
        let result = meter_provider(&[Exporter::Console], &config, Resource::builder().build());
        assert!(matches!(
            result,
            Err(InitError::Settings(err))
                if err == InvalidSetting::new(LUA_METRICS_INTERVAL, "0", EXPECTED_INTERVAL)
        ));
    }

    /// The SDK and `batch_config` read the developer's shell, so its
    /// telemetry variables go first. Sound because nextest runs each test in
    /// its own process, before any other thread starts.
    fn isolate_env() {
        for (key, _) in env::vars_os() {
            if key
                .to_str()
                .is_some_and(|key| ENV_PREFIXES.iter().any(|prefix| key.starts_with(prefix)))
            {
                unsafe { env::remove_var(key) };
            }
        }
    }

    /// gRPC panics at build time without a tokio runtime, and only a live
    /// collector would notice otherwise.
    #[test_case(OTLP, GRPC, "http://127.0.0.1:9"; "grpc")]
    #[test_case(OTLP, GRPC, "https://127.0.0.1:9"; "grpc_tls")]
    #[test_case(OTLP, GRPC, "unix:///tmp/maki-otel.sock"; "grpc_unix_socket")]
    #[test_case(OTLP, HTTP_PROTOBUF, "https://127.0.0.1:9"; "http_tls")]
    #[test_case(CONSOLE, GRPC, ""; "console")]
    fn providers_build_for_every_exporter(exporter: &str, protocol: &str, endpoint: &str) {
        isolate_env();
        let config = TelemetryConfig {
            enabled: Some(true),
            metrics_exporter: Some(exporter.to_string()),
            logs_exporter: Some(exporter.to_string()),
            protocol: Some(protocol.to_string()),
            endpoint: Some(endpoint.to_string()),
            ..TelemetryConfig::default()
        };
        let settings = crate::settings::resolve(&config, |_| None)
            .expect("settings should resolve")
            .expect("telemetry should be enabled");

        let (metrics, logs) = providers(&settings, &config).expect("providers should build");

        metrics
            .expect("metrics provider")
            .shutdown()
            .expect("metrics shutdown");
        logs.expect("logs provider")
            .shutdown()
            .expect("logs shutdown");
    }
}
