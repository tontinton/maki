//! The OTLP resource: who is reporting, on what machine.

use crate::attr::AttrSet;
use crate::settings::Settings;

pub const SCOPE_NAME: &str = "maki";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const SDK_LANGUAGE: &str = "rust";
const SDK_NAME: &str = "maki-otel";

pub(crate) const KEY_SERVICE_NAME: &str = "service.name";
const KEY_SERVICE_VERSION: &str = "service.version";
const KEY_SDK_NAME: &str = "telemetry.sdk.name";
const KEY_SDK_LANGUAGE: &str = "telemetry.sdk.language";
const KEY_SDK_VERSION: &str = "telemetry.sdk.version";
const KEY_OS_TYPE: &str = "os.type";
const KEY_HOST_ARCH: &str = "host.arch";

/// User attributes come last so a collector-wide label can override a default.
pub fn build(settings: &Settings) -> AttrSet {
    let mut attrs = AttrSet::new()
        .with(KEY_SERVICE_NAME, settings.service_name.as_str())
        .with(KEY_SERVICE_VERSION, VERSION)
        .with(KEY_SDK_NAME, SDK_NAME)
        .with(KEY_SDK_LANGUAGE, SDK_LANGUAGE)
        .with(KEY_SDK_VERSION, VERSION)
        .with(KEY_OS_TYPE, std::env::consts::OS)
        .with(KEY_HOST_ARCH, std::env::consts::ARCH);
    for (key, value) in &settings.resource_attributes {
        attrs.insert(key.clone(), value.as_str());
    }
    attrs
}

#[cfg(test)]
mod tests {
    use maki_config::TelemetryConfig;

    use super::*;
    use crate::attr::AttrValue;
    use crate::settings::{ENV_ENABLE, resolve};

    const CUSTOM_KEY: &str = "team";
    const CUSTOM_VALUE: &str = "core";

    fn settings_with(resource_attributes: Vec<(String, String)>) -> Settings {
        let mut settings = resolve(&TelemetryConfig::default(), |k| {
            (k == ENV_ENABLE).then(|| "1".to_string())
        })
        .unwrap()
        .unwrap();
        settings.resource_attributes = resource_attributes;
        settings
    }

    fn value<'a>(attrs: &'a AttrSet, key: &str) -> &'a AttrValue {
        attrs
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
            .expect("attribute should be present")
    }

    #[test]
    fn carries_service_and_sdk_identity() {
        let attrs = build(&settings_with(Vec::new()));
        assert_eq!(
            value(&attrs, KEY_SERVICE_NAME),
            &AttrValue::Str("maki".into())
        );
        assert_eq!(
            value(&attrs, KEY_SDK_NAME),
            &AttrValue::Str(SDK_NAME.into())
        );
        assert_eq!(
            value(&attrs, KEY_SDK_LANGUAGE),
            &AttrValue::Str(SDK_LANGUAGE.into())
        );
    }

    #[test]
    fn user_attributes_override_defaults() {
        let attrs = build(&settings_with(vec![(
            KEY_SERVICE_NAME.to_string(),
            CUSTOM_VALUE.to_string(),
        )]));
        assert_eq!(
            value(&attrs, KEY_SERVICE_NAME),
            &AttrValue::Str(CUSTOM_VALUE.into())
        );
    }

    #[test]
    fn user_attributes_are_added() {
        let attrs = build(&settings_with(vec![(
            CUSTOM_KEY.to_string(),
            CUSTOM_VALUE.to_string(),
        )]));
        assert_eq!(
            value(&attrs, CUSTOM_KEY),
            &AttrValue::Str(CUSTOM_VALUE.into())
        );
    }
}
