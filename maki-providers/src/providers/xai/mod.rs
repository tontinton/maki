pub mod auth;
pub(crate) mod catalog;
mod platform;

pub use platform::Xai;

use std::sync::{Arc, Mutex};

use maki_config::providers::Protocol;

use crate::AgentError;
use crate::model::{ModelEntry, ModelFamily, ModelPricing, ModelTier};
use crate::provider::Provider;
use crate::providers::{ResolvedAuth, Timeouts};
use crate::spec::{AuthDoc, CatalogDoc, GeneratedDocs, LoginConfig, Native, ProviderSpec};

const GROK_CONTEXT_WINDOW: u32 = 500_000;
const GROK_4_3_CONTEXT_WINDOW: u32 = 1_000_000;
const GROK_MAX_OUTPUT_TOKENS: u32 = 131_072;

pub(crate) const SLUG: &str = "xai";
const DISPLAY_NAME: &str = "xAI";
const BASE_URL: &str = "https://api.x.ai/v1";
const DEFAULT_MODEL: &str = "xai/grok-4.6";
const LOGIN_URL: &str = "https://console.x.ai";
const FEATURES: &str =
    "OAuth login, account-specific model catalog, Grok reasoning (low/medium/high/xhigh)";
const AUTH_NOTE: &str = "(also supports OAuth via `maki auth login xai`)";

const OAUTH_NOTE: &str = r#"OAuth uses the same first-party xAI client as the official Grok CLI (`maki auth login xai`). Browser login (PKCE) is the desktop default; device code is recommended over SSH or in a container. Tokens refresh automatically. After login, Maki fetches your account catalog from `GET /v1/models-v2` on the Grok CLI proxy and caches it for 15 minutes. `XAI_BASE_URL` only redirects the public API-key endpoint, never the OAuth proxy.

If `~/.grok/auth.json` already exists, login offers to reuse it without writing that file."#;

/// No Aperture route: the OAuth CLI proxy is not a clean gateway target.
pub(crate) const SPEC: ProviderSpec = ProviderSpec {
    slug: SLUG,
    display_name: DISPLAY_NAME,
    api_key_env: auth::API_KEY_ENV,
    family: ModelFamily::Generic,
    supports_thinking: true,
    accepts_arbitrary_models: true,
    fallback_max_output: Some(GROK_MAX_OUTPUT_TOKENS),
    fallback_context_window: 500_000,
    models: models(),
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
        api_urls: &[BASE_URL, auth::CLI_BASE_URL],
        features: Some(FEATURES),
        auth: AuthDoc::EnvVarWith(AUTH_NOTE),
        catalog: CatalogDoc::Table,
        trailing_notes: &[OAUTH_NOTE],
    },
};

fn create(timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    Ok(Box::new(Xai::new(timeouts)?))
}

fn create_with_auth(
    auth: Arc<Mutex<ResolvedAuth>>,
    timeouts: Timeouts,
    system_prefix: Option<String>,
) -> Box<dyn Provider> {
    Box::new(Xai::with_auth(auth, timeouts).with_system_prefix(system_prefix))
}

inventory::submit!(SPEC.config_row());

pub(crate) const fn models() -> &'static [ModelEntry] {
    const MODELS: &[ModelEntry] = &[
        ModelEntry {
            prefixes: &["grok-4.6"],
            tier: ModelTier::Strong,
            family: ModelFamily::Generic,
            vision: true,
            default: true,
            pricing: ModelPricing::per_million(2.00, 6.00, 0.00, 0.50),
            max_output_tokens: Some(GROK_MAX_OUTPUT_TOKENS),
            context_window: GROK_CONTEXT_WINDOW,
        },
        ModelEntry {
            prefixes: &["grok-4.5"],
            tier: ModelTier::Strong,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_million(2.00, 6.00, 0.00, 0.50),
            max_output_tokens: Some(GROK_MAX_OUTPUT_TOKENS),
            context_window: GROK_CONTEXT_WINDOW,
        },
        ModelEntry {
            prefixes: &["grok-4.3"],
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            vision: true,
            default: true,
            pricing: ModelPricing::per_million(1.25, 2.50, 0.00, 0.20),
            max_output_tokens: Some(GROK_MAX_OUTPUT_TOKENS),
            context_window: GROK_4_3_CONTEXT_WINDOW,
        },
    ];
    MODELS
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("grok-4.6", ModelTier::Strong, 2.0, 6.0, 0.5, GROK_CONTEXT_WINDOW)]
    #[test_case("grok-4.5", ModelTier::Strong, 2.0, 6.0, 0.5, GROK_CONTEXT_WINDOW)]
    #[test_case("grok-4.3", ModelTier::Medium, 1.25, 2.5, 0.2, GROK_4_3_CONTEXT_WINDOW)]
    fn curated_models_have_expected_metadata(
        model_id: &str,
        tier: ModelTier,
        input: f64,
        output: f64,
        cache_read: f64,
        context_window: u32,
    ) {
        let model = models()
            .iter()
            .find(|model| model.prefixes.contains(&model_id))
            .expect("curated xAI model should be registered");

        assert_eq!(model.tier, tier);
        assert!(model.vision);
        assert_eq!(model.context_window, context_window);
        assert_eq!(model.max_output_tokens, Some(GROK_MAX_OUTPUT_TOKENS));
        assert_eq!(model.pricing.input, input);
        assert_eq!(model.pricing.output, output);
        assert_eq!(model.pricing.cache_read, cache_read);
    }
}
