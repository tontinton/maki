pub mod auth;
pub(crate) mod catalog;
mod platform;

pub use platform::Xai;

use crate::model::{ModelEntry, ModelFamily, ModelPricing, ModelTier};

const GROK_CONTEXT_WINDOW: u32 = 500_000;
const GROK_4_3_CONTEXT_WINDOW: u32 = 1_000_000;
const GROK_MAX_OUTPUT_TOKENS: u32 = 131_072;

inventory::submit!(maki_config::providers::BuiltInProvider {
    slug: "xai",
    display_name: "xAI",
    protocol: maki_config::providers::Protocol::Openai,
    default_base_url: "https://api.x.ai/v1",
    default_api_key_env: auth::API_KEY_ENV,
    default_model: "xai/grok-4.6",
    plans: None,
    login_url: Some("https://console.x.ai"),
    needs_url: false,
});

pub(crate) const fn models() -> &'static [ModelEntry] {
    const MODELS: &[ModelEntry] = &[
        ModelEntry {
            prefixes: &["grok-4.6"],
            tier: ModelTier::Strong,
            family: ModelFamily::Generic,
            vision: true,
            default: true,
            pricing: ModelPricing::per_token(2.00, 6.00, 0.00, 0.50),
            max_output_tokens: Some(GROK_MAX_OUTPUT_TOKENS),
            context_window: GROK_CONTEXT_WINDOW,
        },
        ModelEntry {
            prefixes: &["grok-4.5"],
            tier: ModelTier::Strong,
            family: ModelFamily::Generic,
            vision: true,
            default: false,
            pricing: ModelPricing::per_token(2.00, 6.00, 0.00, 0.50),
            max_output_tokens: Some(GROK_MAX_OUTPUT_TOKENS),
            context_window: GROK_CONTEXT_WINDOW,
        },
        ModelEntry {
            prefixes: &["grok-4.3"],
            tier: ModelTier::Medium,
            family: ModelFamily::Generic,
            vision: true,
            default: true,
            pricing: ModelPricing::per_token(1.25, 2.50, 0.00, 0.20),
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
