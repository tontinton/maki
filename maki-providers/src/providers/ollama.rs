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

const CONTEXT_WINDOW_NOTE: &str = "#### Context window\n\n\
     Ollama picks the context window when it loads a model, from available VRAM unless a Modelfile \
     `num_ctx` or `OLLAMA_CONTEXT_LENGTH` sets it. That is often less than the model supports. \
     Before every request maki reads the window from `/api/ps` and compacts to fit, so Ollama does not \
     cut the start of a long chat. A model that is not loaded yet starts from its Modelfile `num_ctx`, \
     or from the trained length without one.\n\n\
     For more room, raise the window on the server, where the memory is: `OLLAMA_CONTEXT_LENGTH` \
     or a Modelfile `num_ctx`. maki does not set it per request, because a larger window reloads \
     the model and can run out of VRAM mid-session.";

const THINKING_NOTE: &str = "#### Thinking\n\n\
     maki reads from `/api/show` whether each model thinks and which levels it takes. \
     `/thinking` lists every level, and a level the model does not take is dimmed and names the \
     level it sends instead: the closest one below, or else the lowest. A model that can only turn \
     thinking on or off shows `medium`, the level Ollama reads any request as. `adaptive` sends \
     `medium`, snapped the same way, and `off` sends `none` to a model that can turn thinking off.";

pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: OLLAMA.slug,
    display_name: OLLAMA.display_name,
    api_key_env: OLLAMA.api_key_env,
    family: ModelFamily::Generic,
    supports_thinking: false,
    supports_deferred_tools: false,
    sends_thinking: |model, choice| OLLAMA.sends_thinking(model, choice),
    thinking_fields_fallback: |_| OLLAMA.fields_fallback(),
    accepts_arbitrary_models: true,
    fallback_max_output: Some(16_384),
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
        default_base_url: OLLAMA.default_host,
        default_model: OLLAMA.default_model,
        plans: None,
        login_url: None,
        needs_url: true,
    }),
    docs: GeneratedDocs {
        // Docs quote the codec url, which carries the `/v1` segment the login
        // default leaves off.
        api_urls: &[super::local::OLLAMA_BASE_URL],
        features: Some(FEATURES),
        auth: AuthDoc::Custom(AUTH_DOC),
        catalog: CatalogDoc::Discovered(DISCOVERY_NOTE),
        trailing_notes: &[CONTEXT_WINDOW_NOTE, THINKING_NOTE],
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
