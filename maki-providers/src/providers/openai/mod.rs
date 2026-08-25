pub mod auth;
mod platform;
pub(crate) mod responses;

pub use platform::OpenAi;

use crate::model::{FastPricing, ModelEntry, ModelFamily, ModelPricing, ModelTier};

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

pub(crate) const fn models() -> &'static [ModelEntry] {
    &[
        ModelEntry {
            prefixes: &["gpt-5.6-luna"],
            tier: ModelTier::Weak,
            family: ModelFamily::Gpt,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 0.20,
                output: 1.20,
                cache_write: 0.25,
                cache_read: 0.02,
                fast: Some(FastPricing {
                    input: 0.40,
                    output: 2.40,
                }),
            },
            max_output_tokens: Some(GPT_5_6_MAX_OUTPUT_TOKENS),
            context_window: GPT_5_6_CONTEXT_WINDOW,
        },
        ModelEntry {
            prefixes: &["gpt-5.6-terra"],
            tier: ModelTier::Medium,
            family: ModelFamily::Gpt,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 2.00,
                output: 12.00,
                cache_write: 2.50,
                cache_read: 0.20,
                fast: Some(FastPricing {
                    input: 4.00,
                    output: 24.00,
                }),
            },
            max_output_tokens: Some(GPT_5_6_MAX_OUTPUT_TOKENS),
            context_window: GPT_5_6_CONTEXT_WINDOW,
        },
        ModelEntry {
            prefixes: &["gpt-5.6-sol"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            vision: true,
            default: true,
            pricing: ModelPricing {
                input: 4.00,
                output: 20.00,
                cache_write: 5.00,
                cache_read: 0.40,
                fast: Some(FastPricing {
                    input: 8.00,
                    output: 40.00,
                }),
            },
            max_output_tokens: Some(GPT_5_6_MAX_OUTPUT_TOKENS),
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
            max_output_tokens: Some(128_000),
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
                fast: Some(FastPricing {
                    input: 1.50,
                    output: 9.00,
                }),
            },
            max_output_tokens: Some(128_000),
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
            max_output_tokens: Some(32_768),
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
            max_output_tokens: Some(32_768),
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
            max_output_tokens: Some(32_768),
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
            max_output_tokens: Some(100_000),
            context_window: 200_000,
        },
        ModelEntry {
            prefixes: &["gpt-5.5-pro"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gpt,
            vision: true,
            default: false,
            pricing: ModelPricing {
                input: 30.00,
                output: 180.00,
                cache_write: 0.00,
                cache_read: 0.00,
                fast: None,
            },
            max_output_tokens: Some(128_000),
            context_window: 1_050_000,
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
                fast: Some(FastPricing {
                    input: 12.50,
                    output: 75.00,
                }),
            },
            max_output_tokens: Some(128_000),
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
                fast: Some(FastPricing {
                    input: 5.00,
                    output: 30.00,
                }),
            },
            max_output_tokens: Some(128_000),
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
            max_output_tokens: Some(100_000),
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
            max_output_tokens: Some(128_000),
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
            max_output_tokens: Some(128_000),
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
            max_output_tokens: Some(128_000),
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
            max_output_tokens: Some(128_000),
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
            max_output_tokens: Some(128_000),
            context_window: 400_000,
        },
    ]
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("gpt-5.6-luna", ModelTier::Weak, 0.2, 0.02, 0.25, 1.2)]
    #[test_case("gpt-5.6-terra", ModelTier::Medium, 2.0, 0.2, 2.5, 12.0)]
    #[test_case("gpt-5.6-sol", ModelTier::Strong, 4.0, 0.4, 5.0, 20.0)]
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

    fn entry(model_id: &str) -> &'static ModelEntry {
        models()
            .iter()
            .find(|model| model.prefixes.contains(&model_id))
            .expect("model should be registered")
    }

    #[test_case("gpt-5.6-sol", 8.00, 40.00)]
    #[test_case("gpt-5.6-terra", 4.00, 24.00)]
    #[test_case("gpt-5.6-luna", 0.40, 2.40)]
    #[test_case("gpt-5.5", 12.50, 75.00)]
    #[test_case("gpt-5.4", 5.00, 30.00)]
    #[test_case("gpt-5.4-mini", 1.50, 9.00)]
    fn fast_tier_matches_published_rates(model_id: &str, input: f64, output: f64) {
        let fast = entry(model_id)
            .pricing
            .fast
            .as_ref()
            .expect("model should carry fast-tier pricing");

        assert_eq!(fast.input, input);
        assert_eq!(fast.output, output);
    }

    /// `gpt-5.5-pro` and `gpt-5.4-nano` have no published fast rate. OpenAI
    /// does sell one for `gpt-4.1`, `o3` and `gpt-5.3-codex`, but we only wire
    /// fast mode for the GPT-5.4+ line, so the flag must not reach any of them.
    #[test_case("gpt-5.5-pro")]
    #[test_case("gpt-5.4-nano")]
    #[test_case("gpt-4.1")]
    #[test_case("o3")]
    #[test_case("gpt-5.3-codex")]
    fn models_without_a_wired_fast_tier_have_none(model_id: &str) {
        assert!(entry(model_id).pricing.fast.is_none());
    }

    /// `gpt-5.5-pro` shares the `gpt-5.5` prefix but sells no fast tier, so
    /// resolution has to land on its own entry rather than `gpt-5.5`'s.
    #[test]
    fn gpt_5_5_pro_resolves_to_its_own_entry() {
        let entry = crate::model::lookup_entry(models(), "gpt-5.5-pro").unwrap();
        assert!(entry.prefixes.contains(&"gpt-5.5-pro"));
        assert!(entry.pricing.fast.is_none());
    }

    /// Cache columns are derived, not stored, so the derivation has to land on
    /// OpenAI's published fast rates for every model that has them.
    #[test_case("gpt-5.6-sol", 10.00, 0.80)]
    #[test_case("gpt-5.6-terra", 5.00, 0.40)]
    #[test_case("gpt-5.6-luna", 0.50, 0.04)]
    #[test_case("gpt-5.5", 0.00, 1.25)]
    #[test_case("gpt-5.4", 0.00, 0.50)]
    #[test_case("gpt-5.4-mini", 0.00, 0.15)]
    fn derived_fast_cache_rates_match_published_rates(
        model_id: &str,
        cache_write: f64,
        cache_read: f64,
    ) {
        use crate::model::TokenUsage;

        let pricing = &entry(model_id).pricing;
        let one_million_writes = TokenUsage {
            cache_creation: 1_000_000,
            ..Default::default()
        };
        let one_million_reads = TokenUsage {
            cache_read: 1_000_000,
            ..Default::default()
        };

        assert!((one_million_writes.cost(pricing, true) - cache_write).abs() < 1e-9);
        assert!((one_million_reads.cost(pricing, true) - cache_read).abs() < 1e-9);
    }
}
