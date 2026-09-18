use std::sync::{Arc, Mutex};

use maki_config::providers::Protocol;

use crate::AgentError;
use crate::model::{ModelEntry, ModelFamily};
use crate::provider::Provider;
use crate::providers::aperture::DEFAULT_PATH_PREFIX;
use crate::spec::{
    ApertureRoute, AuthDoc, CatalogDoc, GeneratedDocs, LoginConfig, Native, ProviderSpec,
};

use super::local::{LLAMACPP, LocalEndpoint};
use super::{ResolvedAuth, Timeouts};

const FEATURES: &str =
    "Local or remote inference via LLAMA_CPP_HOST, set optional key via LLAMA_CPP_API_KEY";

const DISCOVERY_NOTE: &str = "Connects to any OpenAI-compatible `/v1` endpoint. Point `LLAMA_CPP_HOST` \
     to your server address (defaults to `http://localhost:8080`).";

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: LLAMACPP.slug,
    display_name: LLAMACPP.display_name,
    api_key_env: LLAMACPP.api_key_env,
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: None,
    fallback_context_window: 128_000,
    models: models(),
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
        default_base_url: LLAMACPP.default_host,
        default_model: LLAMACPP.default_model,
        plans: None,
        login_url: None,
        needs_url: true,
    }),
    docs: GeneratedDocs {
        // Docs quote the codec url, which carries the `/v1` segment the login
        // default leaves off.
        api_urls: &[LLAMACPP.compat.base_url],
        features: Some(FEATURES),
        auth: AuthDoc::EnvVar,
        catalog: CatalogDoc::Discovered(DISCOVERY_NOTE),
        trailing_notes: &[],
    },
};

inventory::submit!(SPEC.config_row());

fn create(timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    Ok(Box::new(LocalEndpoint::new(&LLAMACPP, timeouts)?))
}

fn create_with_auth(
    auth: Arc<Mutex<ResolvedAuth>>,
    timeouts: Timeouts,
    system_prefix: Option<String>,
) -> Box<dyn Provider> {
    Box::new(LocalEndpoint::with_auth(&LLAMACPP, auth, timeouts).with_system_prefix(system_prefix))
}

pub(crate) const fn models() -> &'static [ModelEntry] {
    &[]
}
