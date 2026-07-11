pub mod auth;
mod platform;
pub(crate) mod responses;

pub use platform::OpenAi;

use crate::model::{ModelEntry, ModelFamily, ModelPricing, ModelTier};

const GPT_5_6_CONTEXT_WINDOW: u32 = 372_000;
const GPT_5_6_MAX_OUTPUT_TOKENS: u32 = 128_000;

inventory::submit!(maki_config::providers::BuiltInProvider {
    slug: "openai",
    display_name: "OpenAI",
    protocol: maki_config::providers::Protocol::Openai,
    default_base_url: "https://api.openai.com/v1",
    default_api_key_env: "OPENAI_API_KEY",
    default_model: "openai/gpt-5.5",
    plans: None,
    login_url: Some("https://platform.openai.com/api-keys"),
    needs_url: false,
});

pub(crate) fn models() -> &'static [ModelEntry] {
    &[
        ModelEntry {
            prefixes: &["gpt-5.6-luna"],
            tier: ModelTier::Weak,
            family: ModelFamily::Gpt,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 1.00,
                output: 6.00,
                cache_write: 1.25,
                cache_read: 0.10,
                fast: None,
            },
            max_output_tokens: GPT_5_6_MAX_OUTPUT_TOKENS,
            context_window: GPT_5_6_CONTEXT_WINDOW,
        },
        ModelEntry {
            prefixes: &["gpt-5.6-terra"],
            tier: ModelTier::Medium,
            family: ModelFamily::Gpt,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 2.50,
                output: 15.00,
                cache_write: 3.125,
                cache_read: 0.25,
                fast: None,
            },
            max_output_tokens: GPT_5_6_MAX_OUTPUT_TOKENS,
            context_window: GPT_5_6_CONTEXT_WINDOW,
        },
        ModelEntry {
            prefixes: &["gpt-5.6-sol"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 5.00,
                output: 30.00,
                cache_write: 6.25,
                cache_read: 0.50,
                fast: None,
            },
            max_output_tokens: GPT_5_6_MAX_OUTPUT_TOKENS,
            context_window: GPT_5_6_CONTEXT_WINDOW,
        },
        ModelEntry {
            prefixes: &["gpt-5.4-nano"],
            tier: ModelTier::Weak,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.20,
                output: 1.25,
                cache_write: 0.00,
                cache_read: 0.02,
                fast: None,
            },
            max_output_tokens: 128_000,
            context_window: 400_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.4-mini"],
            tier: ModelTier::Weak,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.75,
                output: 4.50,
                cache_write: 0.00,
                cache_read: 0.075,
                fast: None,
            },
            max_output_tokens: 128_000,
            context_window: 400_000,
        },
        ModelEntry {
            prefixes: &["gpt-4.1-nano"],
            tier: ModelTier::Weak,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.10,
                output: 0.40,
                cache_write: 0.00,
                cache_read: 0.025,
                fast: None,
            },
            max_output_tokens: 32_768,
            context_window: 1_047_576,
        },
        ModelEntry {
            prefixes: &["gpt-4.1-mini"],
            tier: ModelTier::Medium,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.40,
                output: 1.60,
                cache_write: 0.00,
                cache_read: 0.10,
                fast: None,
            },
            max_output_tokens: 32_768,
            context_window: 1_047_576,
        },
        ModelEntry {
            prefixes: &["gpt-4.1"],
            tier: ModelTier::Medium,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 2.00,
                output: 8.00,
                cache_write: 0.00,
                cache_read: 0.50,
                fast: None,
            },
            max_output_tokens: 32_768,
            context_window: 1_047_576,
        },
        ModelEntry {
            prefixes: &["o4-mini"],
            tier: ModelTier::Medium,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 1.10,
                output: 4.40,
                cache_write: 0.00,
                cache_read: 0.275,
                fast: None,
            },
            max_output_tokens: 100_000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.5"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 5.00,
                output: 30.00,
                cache_write: 0.00,
                cache_read: 0.50,
                fast: None,
            },
            max_output_tokens: 128_000,
            context_window: 1_050_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.4"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 2.50,
                output: 15.00,
                cache_write: 0.00,
                cache_read: 0.25,
                fast: None,
            },
            max_output_tokens: 128_000,
            context_window: 1_050_000,
        },
        ModelEntry {
            prefixes: &["o3"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 2.00,
                output: 8.00,
                cache_write: 0.00,
                cache_read: 1.00,
                fast: None,
            },
            max_output_tokens: 100_000,
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.3-codex"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 1.75,
                output: 14.00,
                cache_write: 0.00,
                cache_read: 0.175,
                fast: None,
            },
            max_output_tokens: 128_000,
            context_window: 400_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.2-codex"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 1.75,
                output: 14.00,
                cache_write: 0.00,
                cache_read: 0.175,
                fast: None,
            },
            max_output_tokens: 128_000,
            context_window: 400_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.1-codex-mini"],
            tier: ModelTier::Medium,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 0.25,
                output: 2.00,
                cache_write: 0.00,
                cache_read: 0.025,
                fast: None,
            },
            max_output_tokens: 128_000,
            context_window: 400_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.1-codex-max"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 1.25,
                output: 10.00,
                cache_write: 0.00,
                cache_read: 0.125,
                fast: None,
            },
            max_output_tokens: 128_000,
            context_window: 400_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.1-codex"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 1.25,
                output: 10.00,
                cache_write: 0.00,
                cache_read: 0.125,
                fast: None,
            },
            max_output_tokens: 128_000,
            context_window: 400_000,
        },
    ]
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
}
