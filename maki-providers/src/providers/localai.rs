use std::sync::{Arc, Mutex};

use maki_config::providers::Protocol;

use crate::AgentError;
use crate::model::ModelFamily;
use crate::provider::Provider;
use crate::providers::aperture::DEFAULT_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, Build, CatalogDoc, GeneratedDocs, LoginConfig, NO_CURATED_MODELS,
    Native, ProviderSpec,
};

use super::local::{LOCALAI, LocalEndpoint};
use super::{ResolvedAuth, Timeouts};

/// `KeyPool::resolve` failing is not fatal, so the key stays
/// optional: most instances need none, but a proxy may demand one.
const AUTH_DOC: &str = "`LOCALAI_HOST` for local/remote (e.g. `http://localhost:8080`); \
     most instances need no key, but set `LOCALAI_API_KEY` for one behind a proxy";

const FEATURES: &str =
    "Local or remote inference via LOCALAI_HOST, set optional key via LOCALAI_API_KEY";

const DISCOVERY_NOTE: &str = "LocalAI exposes its full model table over the OpenAI-compatible `/v1` API; its \
    `/v1/models/capabilities` endpoint additionally publishes `context_size` plus \
    capabilities/modalities. Older instances fall back to the Ollama-compat \
    `/api/tags` + `/api/show`. This provider also works with llama.cpp's server, \
    which speaks the same protocol.";

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: LOCALAI.slug,
    display_name: LOCALAI.display_name,
    api_key_env: LOCALAI.api_key_env,
    family: ModelFamily::Generic,
    supports_thinking: true,
    supports_deferred_tools: false,
    accepts_arbitrary_models: true,
    fallback_max_output: None,
    fallback_context_window: 128_000,
    models_toml: NO_CURATED_MODELS,
    pricing_schedule: None,
    build: Build::Native(Native {
        new: create,
        with_auth: create_with_auth,
    }),
    aperture: Some(ApertureRoute {
        path_prefix: DEFAULT_PATH_PREFIX,
    }),
    login: Some(LoginConfig {
        protocol: Protocol::Openai,
        default_base_url: LOCALAI.default_host,
        default_model: LOCALAI.default_model,
        plans: None,
        login_url: None,
        needs_url: true,
    }),
    docs: GeneratedDocs {
        // Docs quote the codec url, which carries the `/v1` segment the login
        // default leaves off.
        api_urls: &[super::local::LOCALAI_BASE_URL],
        auth: AuthDoc::Custom(AUTH_DOC),
        features: Some(FEATURES),
        catalog: CatalogDoc::Discovered(DISCOVERY_NOTE),
        trailing_notes: &[],
    },
};

inventory::submit!(SPEC.config_row());

fn create(timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    Ok(Box::new(LocalEndpoint::new(&LOCALAI, timeouts)?))
}

fn create_with_auth(
    auth: Arc<Mutex<ResolvedAuth>>,
    timeouts: Timeouts,
    system_prefix: Option<String>,
) -> Box<dyn Provider> {
    Box::new(LocalEndpoint::with_auth(&LOCALAI, auth, timeouts).with_system_prefix(system_prefix))
}
