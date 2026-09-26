use maki_config::providers::Protocol;

use crate::model::ModelFamily;
use crate::providers::aperture::DEFAULT_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, Build, CatalogDoc, GeneratedDocs, LoginConfig, ProviderSpec,
};

const SLUG: &str = "regolo";
const DISPLAY_NAME: &str = "Regolo";
const ENV_VAR: &str = "REGOLO_API_KEY";
const BASE_URL: &str = "https://api.regolo.ai/v1";
const DEFAULT_MODEL: &str = "regolo/qwen3-coder-next";
const LOGIN_URL: &str = "https://dashboard.regolo.ai";
const FEATURES: &str = "EU-hosted open-weight models with tool calling. The catalogue and prices are listed live from the API";

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: false,
    fallback_max_output: Some(120_000),
    fallback_context_window: 120_000,
    models_toml: include_str!("../../models/regolo.toml"),
    pricing_schedule: None,
    build: Build::Declared,
    aperture: Some(ApertureRoute {
        path_prefix: DEFAULT_PATH_PREFIX,
    }),
    login: Some(LoginConfig {
        protocol: Protocol::Openai,
        default_base_url: BASE_URL,
        default_model: DEFAULT_MODEL,
        plans: None,
        login_url: Some(LOGIN_URL),
        needs_url: false,
    }),
    docs: GeneratedDocs {
        api_urls: &[BASE_URL],
        features: Some(FEATURES),
        auth: AuthDoc::EnvVar,
        catalog: CatalogDoc::Table,
        trailing_notes: &[],
    },
};

inventory::submit!(SPEC.config_row());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::ProviderRegistry;

    #[test]
    fn manifest_lists_the_catalogued_default_model() {
        let spec = ProviderRegistry::get(SLUG).expect("regolo is a builtin");
        assert!(
            spec.models()
                .iter()
                .any(|m| m.prefixes == ["qwen3-coder-next"])
        );
    }
}
