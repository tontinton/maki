pub mod auth;
mod platform;
pub(crate) mod responses;

pub use platform::OpenAi;

use std::sync::{Arc, Mutex};

use maki_config::providers::Protocol;

use crate::AgentError;
use crate::model::{ModelEntry, ModelFamily, ModelPricing, ModelTier};
use crate::provider::Provider;
use crate::providers::{ResolvedAuth, Timeouts};
use crate::spec::{AuthDoc, CatalogDoc, GeneratedDocs, LoginConfig, Native, ProviderSpec};

const GPT_5_6_CONTEXT_WINDOW: u32 = 372_000;
const GPT_5_6_MAX_OUTPUT_TOKENS: u32 = 128_000;
const GPT_6_CONTEXT_WINDOW: u32 = 1_050_000;
const GPT_6_MAX_OUTPUT_TOKENS: u32 = 128_000;

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

pub(crate) const fn models() -> &'static [ModelEntry] {
    const MODELS: &[ModelEntry] = &[
        ModelEntry {
            prefixes: &["gpt-5.6-luna"],
            tier: ModelTier::Weak,
            family: ModelFamily::Gpt,
            vision: true,
            default: true,
            pricing: ModelPricing::per_million(1.00, 6.00, 1.25, 0.10),
            max_output_tokens: Some(GPT_5_6_MAX_OUTPUT_TOKENS),
            context_window: GPT_5_6_CONTEXT_WINDOW,
        },
        ModelEntry {
            prefixes: &["gpt-5.6-terra"],
            tier: ModelTier::Medium,
            family: ModelFamily::Gpt,
            vision: true,
            default: true,
            pricing: ModelPricing::per_million(2.50, 15.00, 3.125, 0.25),
            max_output_tokens: Some(GPT_5_6_MAX_OUTPUT_TOKENS),
            context_window: GPT_5_6_CONTEXT_WINDOW,
        },
        ModelEntry {
            prefixes: &["gpt-5.6-sol"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            vision: true,
            default: true,
            pricing: ModelPricing::per_million(5.00, 30.00, 6.25, 0.50),
            max_output_tokens: Some(GPT_5_6_MAX_OUTPUT_TOKENS),
            context_window: GPT_5_6_CONTEXT_WINDOW,
        },
        ModelEntry {
            prefixes: &["gpt-6-astra"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing::per_million(10.00, 50.00, 12.50, 1.00),
            max_output_tokens: Some(GPT_6_MAX_OUTPUT_TOKENS),
            context_window: GPT_6_CONTEXT_WINDOW,
        },
        ModelEntry {
            prefixes: &["gpt-5.4-nano"],
            tier: ModelTier::Weak,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing::per_million(0.20, 1.25, 0.00, 0.02),
            max_output_tokens: Some(128_000),
            context_window: 400_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.4-mini"],
            tier: ModelTier::Weak,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing::per_million(0.75, 4.50, 0.00, 0.075),
            max_output_tokens: Some(128_000),
            context_window: 400_000,
        },
        ModelEntry {
            prefixes: &["gpt-4.1-nano"],
            tier: ModelTier::Weak,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing::per_million(0.10, 0.40, 0.00, 0.025),
            max_output_tokens: Some(32_768),
            context_window: 1_047_576,
        },
        ModelEntry {
            prefixes: &["gpt-4.1-mini"],
            tier: ModelTier::Medium,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing::per_million(0.40, 1.60, 0.00, 0.10),
            max_output_tokens: Some(32_768),
            context_window: 1_047_576,
        },
        ModelEntry {
            prefixes: &["gpt-4.1"],
            tier: ModelTier::Medium,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing::per_million(2.00, 8.00, 0.00, 0.50),
            max_output_tokens: Some(32_768),
            context_window: 1_047_576,
        },
        ModelEntry {
            prefixes: &["o4-mini"],
            tier: ModelTier::Medium,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing::per_million(1.10, 4.40, 0.00, 0.275),
            max_output_tokens: Some(100_000),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.5"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing::per_million(5.00, 30.00, 0.00, 0.50),
            max_output_tokens: Some(128_000),
            context_window: 1_050_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.4"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing::per_million(2.50, 15.00, 0.00, 0.25),
            max_output_tokens: Some(128_000),
            context_window: 1_050_000,
        },
        ModelEntry {
            prefixes: &["o3"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing::per_million(2.00, 8.00, 0.00, 1.00),
            max_output_tokens: Some(100_000),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.3-codex"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing::per_million(1.75, 14.00, 0.00, 0.175),
            max_output_tokens: Some(128_000),
            context_window: 400_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.2-codex"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing::per_million(1.75, 14.00, 0.00, 0.175),
            max_output_tokens: Some(128_000),
            context_window: 400_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.1-codex-mini"],
            tier: ModelTier::Medium,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing::per_million(0.25, 2.00, 0.00, 0.025),
            max_output_tokens: Some(128_000),
            context_window: 400_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.1-codex-max"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing::per_million(1.25, 10.00, 0.00, 0.125),
            max_output_tokens: Some(128_000),
            context_window: 400_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.1-codex"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing::per_million(1.25, 10.00, 0.00, 0.125),
            max_output_tokens: Some(128_000),
            context_window: 400_000,
        },
    ];
    MODELS
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("gpt-5.6-luna", ModelTier::Weak, 1.0, 0.1, 1.25, 6.0)]
    #[test_case("gpt-5.6-terra", ModelTier::Medium, 2.5, 0.25, 3.125, 15.0)]
    #[test_case("gpt-5.6-sol", ModelTier::Strong, 5.0, 0.5, 6.25, 30.0)]
    fn gpt_5_6_models_have_expected_tier_and_short_context_pricing(
        model_id: &str,
        tier: ModelTier,
        input: f64,
        cache_read: f64,
        cache_write: f64,
        output: f64,
    ) {
        let model = models()
            .iter()
            .find(|model| model.prefixes.contains(&model_id))
            .expect("GPT-5.6 model should be registered");

        assert_eq!(model.tier, tier);
        assert_eq!(model.context_window, GPT_5_6_CONTEXT_WINDOW);
        assert_eq!(model.pricing.input, input);
        assert_eq!(model.pricing.cache_read, cache_read);
        assert_eq!(model.pricing.cache_write, cache_write);
        assert_eq!(model.pricing.output, output);
    }

    #[test]
    fn gpt_6_astra_is_a_strong_vision_model_with_full_context() {
        let model = models()
            .iter()
            .find(|model| model.prefixes.contains(&"gpt-6-astra"))
            .expect("GPT-6 Astra should be registered");

        assert_eq!(model.tier, ModelTier::Strong);
        assert!(model.vision);
        assert_eq!(model.context_window, GPT_6_CONTEXT_WINDOW);
        assert_eq!(model.max_output_tokens, Some(GPT_6_MAX_OUTPUT_TOKENS));
        assert_eq!(model.pricing.input, 10.0);
        assert_eq!(model.pricing.cache_read, 1.0);
        assert_eq!(model.pricing.cache_write, 12.5);
        assert_eq!(model.pricing.output, 50.0);
    }
}
