//! Builds the SDK providers. Each `init.lua` key is one builder call when it
//! is set, so it wins over the matching `OTEL_*` variable, and anything left
//! unset is up to the SDK and its environment variables.

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
    Compression, LogExporter, MetricExporter, OTEL_EXPORTER_OTLP_ENDPOINT,
    OTEL_EXPORTER_OTLP_INSECURE, OTEL_EXPORTER_OTLP_LOGS_ENDPOINT,
    OTEL_EXPORTER_OTLP_LOGS_INSECURE, OTEL_EXPORTER_OTLP_LOGS_PROTOCOL,
    OTEL_EXPORTER_OTLP_METRICS_ENDPOINT, OTEL_EXPORTER_OTLP_METRICS_INSECURE,
    OTEL_EXPORTER_OTLP_METRICS_PROTOCOL, OTEL_EXPORTER_OTLP_PROTOCOL,
    OTEL_EXPORTER_OTLP_PROTOCOL_GRPC, OTEL_EXPORTER_OTLP_PROTOCOL_HTTP_JSON,
    OTEL_EXPORTER_OTLP_PROTOCOL_HTTP_PROTOBUF, Protocol, WithExportConfig, WithHttpConfig,
    WithTonicConfig,
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

use crate::InitError;
use crate::console::{ConsoleLogs, ConsoleMetrics};
use crate::resource;
use crate::settings::{Exporter, InvalidSetting, Settings};

/// The SDK allocates the event queue up front, whether its size comes from
/// `init.lua` or `OTEL_BLRP_MAX_QUEUE_SIZE`, so a huge size would cost
/// gigabytes or panic at startup.
const MAX_LOGS_QUEUE_SIZE: usize = 65_536;

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
    fn protocol_env(self) -> [&'static str; 2] {
        match self {
            Self::Metrics => [
                OTEL_EXPORTER_OTLP_METRICS_PROTOCOL,
                OTEL_EXPORTER_OTLP_PROTOCOL,
            ],
            Self::Logs => [
                OTEL_EXPORTER_OTLP_LOGS_PROTOCOL,
                OTEL_EXPORTER_OTLP_PROTOCOL,
            ],
        }
    }

    fn endpoint_env(self) -> [&'static str; 2] {
        match self {
            Self::Metrics => [
                OTEL_EXPORTER_OTLP_METRICS_ENDPOINT,
                OTEL_EXPORTER_OTLP_ENDPOINT,
            ],
            Self::Logs => [
                OTEL_EXPORTER_OTLP_LOGS_ENDPOINT,
                OTEL_EXPORTER_OTLP_ENDPOINT,
            ],
        }
    }

    fn insecure_env(self) -> [&'static str; 2] {
        match self {
            Self::Metrics => [
                OTEL_EXPORTER_OTLP_METRICS_INSECURE,
                OTEL_EXPORTER_OTLP_INSECURE,
            ],
            Self::Logs => [
                OTEL_EXPORTER_OTLP_LOGS_INSECURE,
                OTEL_EXPORTER_OTLP_INSECURE,
            ],
        }
    }

    fn http_path(self) -> &'static str {
        match self {
            Self::Metrics => METRICS_PATH,
            Self::Logs => LOGS_PATH,
        }
    }
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
    reject_zero_interval(
        lua.metrics_interval_ms,
        LUA_METRICS_INTERVAL,
        OTEL_METRIC_EXPORT_INTERVAL,
    )?;
    let temporality = lua
        .metrics_temporality
        .as_deref()
        .map(|value| {
            value
                .parse::<Temporality>()
                .map_err(|()| InvalidSetting::new(LUA_TEMPORALITY, value, EXPECTED_TEMPORALITY))
        })
        .transpose()?;
    let interval = lua.metrics_interval_ms.map(Duration::from_millis);
    let mut builder = SdkMeterProvider::builder().with_resource(resource);
    for exporter in exporters {
        builder = match exporter {
            Exporter::Console => builder.with_reader(periodic_reader(
                ConsoleMetrics(temporality.unwrap_or_default()),
                interval,
            )),
            Exporter::Otlp => {
                builder.with_reader(periodic_reader(otlp_metrics(lua, temporality)?, interval))
            }
        };
    }
    Ok(Some(builder.build()))
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
    let queue_size = lua_or_env(
        lua.logs_max_queue_size,
        LUA_LOGS_MAX_QUEUE_SIZE,
        OTEL_BLRP_MAX_QUEUE_SIZE,
    );
    if let Some((key, size)) = queue_size.filter(|&(_, size)| size > MAX_LOGS_QUEUE_SIZE) {
        return Err(InvalidSetting::new(
            key,
            &size.to_string(),
            EXPECTED_QUEUE_SIZE,
        ));
    }
    reject_zero_interval(
        lua.logs_interval_ms,
        LUA_LOGS_INTERVAL,
        OTEL_BLRP_SCHEDULE_DELAY,
    )?;
    Ok(BatchConfigBuilder::default()
        .set(
            lua.logs_interval_ms.map(Duration::from_millis),
            |builder, delay| builder.with_scheduled_delay(delay),
        )
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

/// At zero the SDK's export thread never sleeps and pins a core.
fn reject_zero_interval(
    lua: Option<u64>,
    lua_key: &'static str,
    env_key: &'static str,
) -> Result<(), InvalidSetting> {
    match lua_or_env(lua, lua_key, env_key) {
        Some((key, 0)) => Err(InvalidSetting::new(key, "0", EXPECTED_INTERVAL)),
        _ => Ok(()),
    }
}

/// The `init.lua` value, else the variable's when it parses, as the SDK
/// picks them.
fn lua_or_env<T: FromStr>(
    lua: Option<T>,
    lua_key: &'static str,
    env_key: &'static str,
) -> Option<(&'static str, T)> {
    lua.map(|value| (lua_key, value)).or_else(|| {
        let value = env::var(env_key).ok()?.parse().ok()?;
        Some((env_key, value))
    })
}

fn otlp_metrics(
    lua: &TelemetryConfig,
    temporality: Option<Temporality>,
) -> Result<MetricExporter, InitError> {
    let otlp = Otlp::new(lua, Signal::Metrics)?;
    let builder = MetricExporter::builder().set(temporality, |builder, temporality| {
        builder.with_temporality(temporality)
    });
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
        let transport = match parse_transport(signal.lua_protocol_key(), protocol)? {
            Some(transport) => transport,
            None => parse_transport(LUA_PROTOCOL, &lua.protocol)?
                .unwrap_or_else(|| env_transport(signal)),
        };
        let generic_endpoint = lua.endpoint.as_ref().map(|endpoint| match transport {
            Transport::Grpc => endpoint.clone(),
            Transport::Http(_) => {
                format!("{}{}", endpoint.trim_end_matches('/'), signal.http_path())
            }
        });
        Ok(Self {
            signal,
            transport,
            endpoint: endpoint.clone().or(generic_endpoint),
            headers: headers
                .as_ref()
                .or(lua.headers.as_ref())
                .into_iter()
                .flatten()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            timeout: timeout_ms.or(lua.timeout_ms).map(Duration::from_millis),
            compression: lua
                .compression
                .as_deref()
                .filter(|&value| value != COMPRESSION_NONE)
                .map(|value| match value {
                    COMPRESSION_GZIP => Ok(Compression::Gzip),
                    _ => Err(InvalidSetting::new(
                        LUA_COMPRESSION,
                        value,
                        EXPECTED_COMPRESSION,
                    )),
                })
                .transpose()?,
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
    /// collector would fail every handshake without the OS store.
    fn grpc<B: WithTonicConfig + WithExportConfig>(self, builder: B) -> Result<B, InitError> {
        let roots = self
            .may_use_tls()
            .then(|| ClientTlsConfig::new().with_enabled_roots());
        Ok(self
            .export_config(builder)
            .set(roots, B::with_tls_config)
            .with_metadata(MetadataMap::from_headers(HeaderMap::try_from(
                &self.headers,
            )?))
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
            .or_else(|| env_value(self.signal.endpoint_env(), |value| !value.is_empty()))
            .is_some_and(|endpoint| {
                let insecure = env_value(self.signal.insecure_env(), |_| true)
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

fn parse_transport(key: &str, value: &Option<String>) -> Result<Option<Transport>, InvalidSetting> {
    value
        .as_deref()
        .map(|value| match value {
            OTEL_EXPORTER_OTLP_PROTOCOL_GRPC => Ok(Transport::Grpc),
            OTEL_EXPORTER_OTLP_PROTOCOL_HTTP_PROTOBUF => {
                Ok(Transport::Http(Some(Protocol::HttpBinary)))
            }
            OTEL_EXPORTER_OTLP_PROTOCOL_HTTP_JSON => Ok(Transport::Http(Some(Protocol::HttpJson))),
            _ => Err(InvalidSetting::new(key, value, EXPECTED_PROTOCOL)),
        })
        .transpose()
}

/// The SDK picks the transport from the environment only when it builds the
/// exporter itself. A gRPC exporter built that way has no tokio runtime and
/// panics, and it gets no TLS roots. So maki looks for gRPC up front, skipping
/// unknown values as the SDK does, and leaves the HTTP flavour to the SDK.
fn env_transport(signal: Signal) -> Transport {
    let grpc = env_value(signal.protocol_env(), |value| {
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
        let otlp = Otlp::new(&lua(protocol), Signal::Metrics).unwrap();
        assert_eq!(otlp.endpoint.as_deref(), Some(expected));
    }

    #[test]
    fn per_signal_lua_keys_replace_generic_ones() {
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
            HashMap::from([("b".to_string(), "3".to_string())])
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
