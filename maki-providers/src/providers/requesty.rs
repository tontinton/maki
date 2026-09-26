use maki_config::providers::Protocol;

use crate::model::ModelFamily;
use crate::providers::aperture::DEFAULT_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, Build, CatalogDoc, GeneratedDocs, LoginConfig, NO_CURATED_MODELS,
    ProviderSpec,
};

const SLUG: &str = "requesty";
const DISPLAY_NAME: &str = "Requesty";
const ENV_VAR: &str = "REQUESTY_API_KEY";
const BASE_URL: &str = "https://router.requesty.ai/v1";
const DEFAULT_MODEL: &str = "requesty/openai/gpt-5.5";
const LOGIN_URL: &str = "https://app.requesty.ai/api-keys";
const FEATURES: &str = "700+ models behind one key, curated managed routing policies, EU region via `REQUESTY_BASE_URL`";

const DISCOVERY_NOTE: &str = "Requesty routes 700+ models from many providers behind a single API key. \
     Models are listed live from the API: curated managed policies first \
     (short ids such as `requesty/claude-sonnet-4-5` or `requesty/gpt-5.4-mini`, \
     `@eu` variants route only through EU providers), then the full \
     `<vendor>/<model>` catalog (e.g. `requesty/openai/gpt-4o-mini`). \
     Get a key at [app.requesty.ai/api-keys](https://app.requesty.ai/api-keys). \
     Set `REQUESTY_BASE_URL=https://router.eu.requesty.ai/v1` to keep all \
     traffic in the EU.";

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
