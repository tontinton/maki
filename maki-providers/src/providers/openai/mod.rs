pub mod auth;
mod platform;
pub(crate) mod responses;

pub use platform::OpenAi;

use std::sync::{Arc, Mutex};

use maki_config::providers::Protocol;

use crate::AgentError;
use crate::model::ModelFamily;
use crate::provider::Provider;
use crate::providers::{ResolvedAuth, Timeouts};
use crate::spec::{AuthDoc, CatalogDoc, GeneratedDocs, LoginConfig, Native, ProviderSpec};

pub(crate) const SLUG: &str = "openai";
const DISPLAY_NAME: &str = "OpenAI";
const ENV_VAR: &str = "OPENAI_API_KEY";
const BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL: &str = "openai/gpt-5.5";
const LOGIN_URL: &str = "https://platform.openai.com/api-keys";
const AUTH_NOTE: &str = "(also supports OAuth via `maki auth login openai`)";

const OAUTH_NOTE: &str = r#"`maki auth login openai` offers browser login (PKCE, callback on `localhost:1455`) and device code login. Browser is the desktop default; device code is recommended over SSH or in a container. Tokens refresh automatically.

With ChatGPT OAuth the model list comes from the Codex backend's own `/models` endpoint, so a model your plan gains shows up without a Maki update, with the context window and reasoning levels the backend declares for it. The table above is the offline fallback. The endpoint hides models newer than the Codex CLI version Maki reports, so a brand new release can lag until that version is bumped."#;

/// Routing to the native OpenAI provider would take its Codex responses-API
/// path for `gpt-*-codex` models and bypass the gateway, so Aperture has no
/// route here.
pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: ENV_VAR,
    family: ModelFamily::Gpt,
    supports_thinking: true,
    accepts_arbitrary_models: false,
    fallback_max_output: Some(100_000),
    fallback_context_window: 200_000,
    models_toml: include_str!("../../../models/openai.toml"),
    pricing_schedule: None,
    native: Some(Native {
        new: create,
        with_auth: create_with_auth,
        aperture: None,
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
        features: None,
        auth: AuthDoc::EnvVarWith(AUTH_NOTE),
        catalog: CatalogDoc::Table,
        trailing_notes: &[OAUTH_NOTE],
    },
};

fn create(timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    Ok(Box::new(OpenAi::new(timeouts)?))
}

fn create_with_auth(
    auth: Arc<Mutex<ResolvedAuth>>,
    timeouts: Timeouts,
    system_prefix: Option<String>,
) -> Box<dyn Provider> {
    Box::new(OpenAi::with_auth(auth, timeouts).with_system_prefix(system_prefix))
}

inventory::submit!(SPEC.config_row());
