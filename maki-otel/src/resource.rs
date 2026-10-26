//! The OTLP resource: who is reporting, on what machine. The SDK's detectors
//! read `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES`, and `init.lua`
//! goes on top, except for a service the environment names.

use std::env::consts;

use maki_config::TelemetryConfig;
use opentelemetry::{InstrumentationScope, Key, KeyValue};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::resource::{ResourceDetector, SdkProvidedResourceDetector};

use crate::export::Set;

pub const SCOPE_NAME: &str = "maki";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_SERVICE_NAME: &str = "maki";

const KEY_SERVICE_NAME: &str = "service.name";
const KEY_SERVICE_VERSION: &str = "service.version";
const KEY_OS_TYPE: &str = "os.type";
const KEY_HOST_ARCH: &str = "host.arch";
/// What the SDK calls a service nobody named, optionally followed by
/// `:<executable>`.
const UNKNOWN_SERVICE: &str = "unknown_service";

pub fn build(lua: &TelemetryConfig) -> Resource {
    let env_name = service_name(&SdkProvidedResourceDetector.detect());
    Resource::builder()
        .with_attributes([
            KeyValue::new(KEY_SERVICE_VERSION, VERSION),
            KeyValue::new(KEY_OS_TYPE, consts::OS),
            KeyValue::new(KEY_HOST_ARCH, consts::ARCH),
        ])
        .with_attributes(
            env_name
                .is_none()
                .then(|| KeyValue::new(KEY_SERVICE_NAME, DEFAULT_SERVICE_NAME)),
        )
        .with_attributes(
            lua.resource_attributes
                .iter()
                .flatten()
                .map(|(key, value)| KeyValue::new(key.clone(), value.clone())),
        )
        .set(
            env_name.or_else(|| lua.service_name.clone()),
            |builder, name| builder.with_service_name(name),
        )
        .build()
}

/// The name from `OTEL_SERVICE_NAME` or a `service.name` in
/// `OTEL_RESOURCE_ATTRIBUTES`, unless the SDK fell back to its own.
fn service_name(detected: &Resource) -> Option<String> {
    detected
        .get(&Key::from_static_str(KEY_SERVICE_NAME))
        .map(|name| name.as_str().into_owned())
        .filter(|name| !name.starts_with(UNKNOWN_SERVICE))
}

pub(crate) fn scope() -> InstrumentationScope {
    InstrumentationScope::builder(SCOPE_NAME)
        .with_version(VERSION)
        .build()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use opentelemetry::Value;
    use test_case::test_case;

    use super::*;

    const LUA_NAME: &str = "from-lua";
    const ENV_NAME: &str = "billing";
    const CUSTOM_KEY: &str = "team";
    const CUSTOM_VALUE: &str = "core";

    fn value(resource: &Resource, key: &'static str) -> Option<Value> {
        resource.get(&Key::from_static_str(key))
    }

    #[test_case("unknown_service:maki", None; "sdk_fallback_with_executable")]
    #[test_case("unknown_service", None; "sdk_fallback")]
    #[test_case(ENV_NAME, Some(ENV_NAME); "named_in_the_environment")]
    fn takes_only_a_service_the_environment_names(detected: &str, expected: Option<&str>) {
        let resource = Resource::builder_empty()
            .with_service_name(detected.to_string())
            .build();
        assert_eq!(service_name(&resource).as_deref(), expected);
    }

    #[test_case("OTEL_SERVICE_NAME", ENV_NAME; "service_name")]
    #[test_case("OTEL_RESOURCE_ATTRIBUTES", "service.name=billing"; "resource_attributes")]
    fn a_service_the_environment_names_beats_lua(env: &str, raw: &str) {
        // Sound because nextest runs each test in its own process.
        unsafe { std::env::set_var(env, raw) };
        let resource = build(&TelemetryConfig {
            service_name: Some(LUA_NAME.to_string()),
            resource_attributes: Some(BTreeMap::from([(
                KEY_SERVICE_NAME.to_string(),
                LUA_NAME.to_string(),
            )])),
            ..TelemetryConfig::default()
        });
        assert_eq!(value(&resource, KEY_SERVICE_NAME), Some(ENV_NAME.into()));
    }

    #[test]
    fn lua_service_name_beats_lua_attributes() {
        let resource = build(&TelemetryConfig {
            service_name: Some(LUA_NAME.to_string()),
            resource_attributes: Some(BTreeMap::from([
                (KEY_SERVICE_NAME.to_string(), CUSTOM_VALUE.to_string()),
                (CUSTOM_KEY.to_string(), CUSTOM_VALUE.to_string()),
            ])),
            ..TelemetryConfig::default()
        });
        assert_eq!(value(&resource, KEY_SERVICE_NAME), Some(LUA_NAME.into()));
        assert_eq!(value(&resource, CUSTOM_KEY), Some(CUSTOM_VALUE.into()));
        assert_eq!(value(&resource, KEY_SERVICE_VERSION), Some(VERSION.into()));
    }
}
