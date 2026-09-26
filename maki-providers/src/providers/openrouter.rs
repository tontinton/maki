use maki_config::providers::Protocol;

use crate::model::ModelFamily;
use crate::providers::aperture::DEFAULT_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, Build, CatalogDoc, GeneratedDocs, LoginConfig, NO_CURATED_MODELS,
    ProviderSpec,
};

const SLUG: &str = "openrouter";
const DISPLAY_NAME: &str = "OpenRouter";
const ENV_VAR: &str = "OPENROUTER_API_KEY";
const BASE_URL: &str = "https://openrouter.ai/api/v1";
const DEFAULT_MODEL: &str = "openrouter/openai/gpt-5.5";
const LOGIN_URL: &str = "https://openrouter.ai/keys";
const FEATURES: &str = "300+ models from all providers, prompt caching, provider routing";

const DISCOVERY_NOTE: &str = "OpenRouter aggregates models from many providers behind a single API key. \
     Browse available models at [openrouter.ai/models](https://openrouter.ai/models). \
     Use any model ID directly (e.g. `openrouter/anthropic/claude-sonnet-4`).";

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: Some(128_000),
    fallback_context_window: 200_000,
    models_toml: NO_CURATED_MODELS,
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
        catalog: CatalogDoc::Discovered(DISCOVERY_NOTE),
        trailing_notes: &[],
    },
};

inventory::submit!(SPEC.config_row());
