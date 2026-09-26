use maki_config::providers::Protocol;

use crate::model::ModelFamily;
use crate::providers::aperture::DEFAULT_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, Build, CatalogDoc, GeneratedDocs, LoginConfig, ProviderSpec,
};

const SLUG: &str = "synthetic";
const DISPLAY_NAME: &str = "Synthetic";
const ENV_VAR: &str = "SYNTHETIC_API_KEY";
const BASE_URL: &str = "https://api.synthetic.new/openai/v1";
const DEFAULT_MODEL: &str = "synthetic/hf:moonshotai/Kimi-K2.5";
const LOGIN_URL: &str = "https://synthetic.new";
const FEATURES: &str = "Reasoning effort support (low/medium/high), open-weight models";

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Synthetic,
    supports_thinking: true,
    accepts_arbitrary_models: false,
    fallback_max_output: Some(32_000),
    fallback_context_window: 128_000,
    models_toml: include_str!("../../models/synthetic.toml"),
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
