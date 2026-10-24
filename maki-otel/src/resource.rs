//! The OTLP resource: who is reporting, on what machine. The SDK's detectors
//! read `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES`, and `init.lua`
//! goes on top.

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
    Resource::builder()
        .with_attributes([
            KeyValue::new(KEY_SERVICE_VERSION, VERSION),
            KeyValue::new(KEY_OS_TYPE, consts::OS),
            KeyValue::new(KEY_HOST_ARCH, consts::ARCH),
        ])
        .with_attributes(
            unnamed(&SdkProvidedResourceDetector.detect())
                .then(|| KeyValue::new(KEY_SERVICE_NAME, DEFAULT_SERVICE_NAME)),
        )
        .with_attributes(
            lua.resource_attributes
                .iter()
                .flatten()
                .map(|(key, value)| KeyValue::new(key.clone(), value.clone())),
        )
        .set(lua.service_name.clone(), |builder, name| {
            builder.with_service_name(name)
        })
        .build()
}

fn unnamed(detected: &Resource) -> bool {
    detected
        .get(&Key::from_static_str(KEY_SERVICE_NAME))
        .is_none_or(|name| name.as_str().starts_with(UNKNOWN_SERVICE))
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
    const CUSTOM_KEY: &str = "team";
    const CUSTOM_VALUE: &str = "core";

    fn value(resource: &Resource, key: &'static str) -> Option<Value> {
        resource.get(&Key::from_static_str(key))
    }

    #[test_case("unknown_service:maki", true; "sdk_fallback_with_executable")]
    #[test_case("unknown_service", true; "sdk_fallback")]
    #[test_case("billing", false; "named_in_the_environment")]
    fn maki_names_only_an_unnamed_service(detected: &str, expected: bool) {
        let resource = Resource::builder_empty()
            .with_service_name(detected.to_string())
            .build();
        assert_eq!(unnamed(&resource), expected);
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
