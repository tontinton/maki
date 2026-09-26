use maki_config::providers::{Protocol, ProviderPlan};

use crate::model::ModelFamily;
use crate::providers::aperture::DEFAULT_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, Build, CatalogDoc, GeneratedDocs, LoginConfig, ProviderSpec,
};

const SLUG: &str = "mistral";
const DISPLAY_NAME: &str = "Mistral";
const ENV_VAR: &str = "MISTRAL_API_KEY";
const BASE_URL: &str = "https://api.mistral.ai/v1";
const DEFAULT_MODEL: &str = "mistral/mistral-medium-latest";
const CODING_MODEL: &str = "mistral/mistral-vibe-cli-latest";
const LOGIN_URL: &str = "https://admin.mistral.ai/organization/api-keys";

const PLANS: &[(&str, ProviderPlan)] = &[
    (
        "standard",
        ProviderPlan {
            display_name: "Standard",
            base_url: BASE_URL,
            default_model: Some(DEFAULT_MODEL),
            login_url: None,
        },
    ),
    (
        "coding",
        ProviderPlan {
            display_name: "Vibe / Coding",
            base_url: BASE_URL,
            default_model: Some(CODING_MODEL),
            login_url: Some("https://console.mistral.ai/codestral/cli"),
        },
    ),
];

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: None,
    fallback_context_window: 128_000,
    models_toml: include_str!("../../models/mistral.toml"),
    pricing_schedule: None,
    build: Build::Declared,
    aperture: Some(ApertureRoute {
        path_prefix: DEFAULT_PATH_PREFIX,
    }),
    login: Some(LoginConfig {
        protocol: Protocol::Openai,
        default_base_url: BASE_URL,
        default_model: DEFAULT_MODEL,
        plans: Some(PLANS),
        login_url: Some(LOGIN_URL),
        needs_url: false,
    }),
    docs: GeneratedDocs {
        api_urls: &[BASE_URL],
        features: None,
        auth: AuthDoc::EnvVar,
        catalog: CatalogDoc::Table,
        trailing_notes: &[],
    },
};

inventory::submit!(SPEC.config_row());
