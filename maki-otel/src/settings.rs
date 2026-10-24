//! maki's own telemetry switches: the variables the OpenTelemetry SDK does not
//! read, which win over `init.lua`. Everything else is left to the SDK.

use maki_config::TelemetryConfig;
use thiserror::Error;

pub const ENV_ENABLE: &str = "MAKI_ENABLE_TELEMETRY";
pub const ENV_SDK_DISABLED: &str = "OTEL_SDK_DISABLED";
pub const ENV_METRICS_EXPORTER: &str = "OTEL_METRICS_EXPORTER";
pub const ENV_LOGS_EXPORTER: &str = "OTEL_LOGS_EXPORTER";
pub const ENV_METRICS_INCLUDE_SESSION_ID: &str = "OTEL_METRICS_INCLUDE_SESSION_ID";
pub const ENV_METRICS_INCLUDE_VERSION: &str = "OTEL_METRICS_INCLUDE_VERSION";
pub const ENV_LOG_USER_PROMPTS: &str = "OTEL_LOG_USER_PROMPTS";
pub const ENV_LOG_TOOL_DETAILS: &str = "OTEL_LOG_TOOL_DETAILS";
pub const ENV_CONTENT_MAX_LENGTH: &str = "MAKI_OTEL_CONTENT_MAX_LENGTH";

pub const DEFAULT_CONTENT_MAX_LENGTH: usize = 10_240;
/// The one attribute that is on by default: a session is the unit teams slice
/// by, and one maki process only ever reports a handful of them.
pub const DEFAULT_METRICS_INCLUDE_SESSION_ID: bool = true;

const LUA_METRICS_EXPORTER: &str = "telemetry.metrics_exporter";
const LUA_LOGS_EXPORTER: &str = "telemetry.logs_exporter";

const EXPECTED_EXPORTER: &str = "otlp, console or none";
const EXPECTED_BOOL: &str = "true or false";
const EXPECTED_COUNT: &str = "a non-negative integer";

#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid telemetry setting {key}={value:?}, expected {expected}")]
pub struct InvalidSetting {
    pub key: String,
    pub value: String,
    pub expected: &'static str,
}

impl InvalidSetting {
    pub(crate) fn new(key: &str, value: &str, expected: &'static str) -> Self {
        Self {
            key: key.to_string(),
            value: value.to_string(),
            expected,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exporter {
    Otlp,
    Console,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Settings {
    pub metrics_exporters: Vec<Exporter>,
    pub logs_exporters: Vec<Exporter>,
    pub metrics_include_session_id: bool,
    pub metrics_include_version: bool,
    pub log_user_prompts: bool,
    pub log_tool_details: bool,
    pub content_max_length: usize,
}

/// `None` when telemetry is off. Env lookups go through `env` so tests never
/// touch the process environment.
pub fn resolve(
    lua: &TelemetryConfig,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<Settings>, InvalidSetting> {
    let env = |key: &'static str| {
        env(key)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(|value| (key, value))
    };
    let flag = |key, lua: Option<bool>, default| {
        env(key)
            .map(|(key, value)| parse_bool(key, &value))
            .transpose()
            .map(|flag| flag.or(lua).unwrap_or(default))
    };

    // The OTel kill switch outranks our own opt in, so an org can turn every
    // SDK off in one place without touching a checked-in `init.lua`.
    if flag(ENV_SDK_DISABLED, None, false)? || !flag(ENV_ENABLE, lua.enabled, false)? {
        return Ok(None);
    }
    let exporters = |key, lua_key, lua_value: &Option<String>| {
        env(key)
            .or_else(|| lua_value.clone().map(|value| (lua_key, value)))
            .map_or(Ok(Vec::new()), |(key, value)| parse_exporters(key, &value))
    };
    let content_max_length = match env(ENV_CONTENT_MAX_LENGTH) {
        Some((key, value)) => value
            .parse()
            .map_err(|_| InvalidSetting::new(key, &value, EXPECTED_COUNT))?,
        None => lua.content_max_length.unwrap_or(DEFAULT_CONTENT_MAX_LENGTH),
    };

    Ok(Some(Settings {
        metrics_exporters: exporters(
            ENV_METRICS_EXPORTER,
            LUA_METRICS_EXPORTER,
            &lua.metrics_exporter,
        )?,
        logs_exporters: exporters(ENV_LOGS_EXPORTER, LUA_LOGS_EXPORTER, &lua.logs_exporter)?,
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

fn parse_bool(key: &str, value: &str) -> Result<bool, InvalidSetting> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(InvalidSetting::new(key, value, EXPECTED_BOOL)),
    }
}

/// A repeated exporter would export every batch twice, so the list keeps
/// first mention only.
fn parse_exporters(key: &str, value: &str) -> Result<Vec<Exporter>, InvalidSetting> {
    let mut exporters = Vec::new();
    for part in value.split(',') {
        let exporter = match part.trim().to_ascii_lowercase().as_str() {
            "" | "none" => continue,
            "otlp" => Exporter::Otlp,
            "console" => Exporter::Console,
            _ => return Err(InvalidSetting::new(key, value, EXPECTED_EXPORTER)),
        };
        if !exporters.contains(&exporter) {
            exporters.push(exporter);
        }
    }
    Ok(exporters)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key: &str| owned.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    fn enabled_lua() -> TelemetryConfig {
        TelemetryConfig {
            enabled: Some(true),
            ..TelemetryConfig::default()
        }
    }

    fn resolve_env(pairs: &[(&str, &str)]) -> Result<Option<Settings>, InvalidSetting> {
        resolve(&TelemetryConfig::default(), env_from(pairs))
    }

    #[test]
    fn disabled_by_default() {
        assert_eq!(resolve_env(&[]).unwrap(), None);
    }

    #[test]
    fn env_disable_overrides_lua_enable() {
        assert_eq!(
            resolve(&enabled_lua(), env_from(&[(ENV_ENABLE, "0")])).unwrap(),
            None
        );
    }

    #[test]
    fn sdk_disabled_wins_over_every_opt_in() {
        let env = env_from(&[(ENV_SDK_DISABLED, "true"), (ENV_ENABLE, "1")]);
        assert_eq!(resolve(&enabled_lua(), env).unwrap(), None);
    }

    #[test]
    fn env_exporter_beats_lua() {
        let lua = TelemetryConfig {
            metrics_exporter: Some("otlp".to_string()),
            ..enabled_lua()
        };
        let settings = resolve(&lua, env_from(&[(ENV_METRICS_EXPORTER, "console")]))
            .unwrap()
            .unwrap();
        assert_eq!(settings.metrics_exporters, vec![Exporter::Console]);
    }

    #[test_case("otlp", &[Exporter::Otlp]; "otlp_only")]
    #[test_case("none", &[]; "none_is_empty")]
    #[test_case(" otlp , console ", &[Exporter::Otlp, Exporter::Console]; "whitespace_tolerant")]
    #[test_case("otlp,console,otlp", &[Exporter::Otlp, Exporter::Console]; "repeats_ignored")]
    fn parses_exporter_lists(raw: &str, expected: &[Exporter]) {
        let settings = resolve_env(&[(ENV_ENABLE, "1"), (ENV_LOGS_EXPORTER, raw)])
            .unwrap()
            .unwrap();
        assert_eq!(settings.logs_exporters, expected);
    }

    #[test]
    fn rejects_unknown_exporter() {
        let err = resolve(
            &TelemetryConfig {
                metrics_exporter: Some("prometheus".to_string()),
                ..enabled_lua()
            },
            env_from(&[]),
        )
        .unwrap_err();
        assert_eq!(
            err,
            InvalidSetting::new(LUA_METRICS_EXPORTER, "prometheus", EXPECTED_EXPORTER)
        );
    }

    #[test_case("1", true; "one")]
    #[test_case("TRUE", true; "uppercase")]
    #[test_case("off", false; "off")]
    fn parses_booleans(raw: &str, expected: bool) {
        let settings = resolve_env(&[(ENV_ENABLE, "1"), (ENV_LOG_USER_PROMPTS, raw)])
            .unwrap()
            .unwrap();
        assert_eq!(settings.log_user_prompts, expected);
    }

    #[test]
    fn rejects_non_boolean_flags() {
        let err = resolve_env(&[(ENV_ENABLE, "1"), (ENV_LOG_TOOL_DETAILS, "maybe")]).unwrap_err();
        assert_eq!(
            err,
            InvalidSetting::new(ENV_LOG_TOOL_DETAILS, "maybe", EXPECTED_BOOL)
        );
    }

    #[test]
    fn defaults() {
        let settings = resolve_env(&[(ENV_ENABLE, "1")]).unwrap().unwrap();
        assert!(settings.metrics_exporters.is_empty());
        assert!(settings.logs_exporters.is_empty());
        assert_eq!(settings.content_max_length, DEFAULT_CONTENT_MAX_LENGTH);
        assert!(settings.metrics_include_session_id);
        assert!(!settings.metrics_include_version);
        assert!(!settings.log_user_prompts);
        assert!(!settings.log_tool_details);
    }
}
