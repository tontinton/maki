use std::sync::{Arc, Mutex};

use maki_config::providers::Protocol;

use crate::AgentError;
use crate::model::ModelFamily;
use crate::provider::Provider;
use crate::providers::aperture::DEFAULT_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, CatalogDoc, GeneratedDocs, LoginConfig, NO_CURATED_MODELS, Native,
    ProviderSpec,
};

use super::local::{LocalEndpoint, OLLAMA};
use super::{ResolvedAuth, Timeouts};

const FEATURES: &str =
    "Local or remote inference via OLLAMA_HOST, cloud fallback via OLLAMA_API_KEY";

/// The host env var is the story here, not the key, so the line is written out
/// rather than derived from `api_key_env`.
const AUTH_DOC: &str =
    "`OLLAMA_HOST` for local/remote (e.g. `http://localhost:11434`), `OLLAMA_API_KEY` for auth";

const DISCOVERY_NOTE: &str = "This provider talks the OpenAI-compatible `/v1` API, so it also works with \
     llama.cpp's server, LocalAI, or anything else that speaks the same protocol. \
     Just point `OLLAMA_HOST` to the right address \
     (e.g. `http://localhost:8080` for llama.cpp).";

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: OLLAMA.slug,
    display_name: OLLAMA.display_name,
    api_key_env: OLLAMA.api_key_env,
    family: ModelFamily::Generic,
    supports_thinking: false,
    accepts_arbitrary_models: true,
    fallback_max_output: Some(16_384),
    fallback_context_window: 128_000,
    models_toml: NO_CURATED_MODELS,
    pricing_schedule: None,
    native: Some(Native {
        new: create,
        with_auth: create_with_auth,
        aperture: Some(ApertureRoute {
            path_prefix: DEFAULT_PATH_PREFIX,
        }),
    }),
    login: Some(LoginConfig {
        protocol: Protocol::Openai,
        default_base_url: OLLAMA.default_host,
        default_model: OLLAMA.default_model,
        plans: None,
        login_url: None,
        needs_url: true,
    }),
    docs: GeneratedDocs {
        // Docs quote the codec url, which carries the `/v1` segment the login
        // default leaves off.
        api_urls: &[OLLAMA.compat.base_url],
        features: Some(FEATURES),
        auth: AuthDoc::Custom(AUTH_DOC),
        catalog: CatalogDoc::Discovered(DISCOVERY_NOTE),
        trailing_notes: &[],
    },
};

inventory::submit!(SPEC.config_row());

fn create(timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    Ok(Box::new(LocalEndpoint::new(&OLLAMA, timeouts)?))
}

fn create_with_auth(
    auth: Arc<Mutex<ResolvedAuth>>,
    timeouts: Timeouts,
    system_prefix: Option<String>,
) -> Box<dyn Provider> {
    Box::new(LocalEndpoint::with_auth(&OLLAMA, auth, timeouts).with_system_prefix(system_prefix))
}
