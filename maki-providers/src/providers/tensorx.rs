use maki_config::providers::Protocol;

use crate::model::ModelFamily;
use crate::providers::aperture::DEFAULT_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, Build, CatalogDoc, GENERIC_DISCOVERY_NOTE, GeneratedDocs, LoginConfig,
    NO_CURATED_MODELS, ProviderSpec,
};

const SLUG: &str = "tensorx";
const DISPLAY_NAME: &str = "TensorX";
const ENV_VAR: &str = "TENSORX_API_KEY";
const BASE_URL: &str = "https://api.tensorx.ai/v1";
const DEFAULT_MODEL: &str = "tensorx/z-ai/glm-5.2";
const LOGIN_URL: &str = "https://tensorx.ai";
const FEATURES: &str = "Open-weight models, zero data retention, prompt caching";

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: None,
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
        catalog: CatalogDoc::Discovered(GENERIC_DISCOVERY_NOTE),
        trailing_notes: &[],
    },
};

inventory::submit!(SPEC.config_row());
