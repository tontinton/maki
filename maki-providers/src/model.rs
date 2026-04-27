//! Model registry with prefix-based lookup and token accounting.
//! Lookup is prefix-based: `claude-sonnet-4-20250514` matches the `claude-sonnet-4` entry,
//! so dated snapshots resolve without registry churn. `context_tokens()` sums input + output
//! + cache reads/writes because the context window limit applies to all of them combined.

use std::fmt;
use std::ops::AddAssign;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::provider::ProviderKind;
use crate::providers::{
    anthropic, copilot, deepseek, dynamic, google, mistral, ollama, openai, synthetic, zai,
};

const PER_MILLION: f64 = 1_000_000.0;

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("model must be in 'provider/model' format (e.g. anthropic/claude-sonnet-4-20250514)")]
    InvalidFormat,
    #[error("unsupported provider '{0}'")]
    UnsupportedProvider(String),
    #[error("unknown model '{0}'")]
    UnknownModel(String),
    #[error("invalid model tier '{0}' (expected: strong, medium, weak)")]
    InvalidTier(String),
    #[error("no default model for {0}/{1}")]
    NoDefault(ProviderKind, ModelTier),
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
}

impl ModelPricing {
    pub const ZERO: Self = Self {
        input: 0.0,
        output: 0.0,
        cache_write: 0.0,
        cache_read: 0.0,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFamily {
    Claude,
    Generic,
    Gemini,
    Glm,
    Gpt,
    Synthetic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    Weak,
    Medium,
    Strong,
}

impl fmt::Display for ModelTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Weak => "weak",
            Self::Medium => "medium",
            Self::Strong => "strong",
        })
    }
}

impl FromStr for ModelTier {
    type Err = ModelError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "weak" => Ok(Self::Weak),
            "medium" => Ok(Self::Medium),
            "strong" => Ok(Self::Strong),
            other => Err(ModelError::InvalidTier(other.to_string())),
        }
    }
}

pub struct ModelEntry {
    pub prefixes: &'static [&'static str],
    pub tier: ModelTier,
    pub family: ModelFamily,
    pub default: bool,
    pub pricing: ModelPricing,
    pub max_output_tokens: u32,
    pub context_window: u32,
}

fn lookup_entry<'a>(
    entries: &'a [ModelEntry],
    model_id: &str,
) -> Result<&'a ModelEntry, ModelError> {
    entries
        .iter()
        .find(|e| e.prefixes.iter().any(|p| model_id.starts_with(p)))
        .ok_or_else(|| ModelError::UnknownModel(model_id.to_string()))
}

pub fn models_for_provider(provider: ProviderKind) -> &'static [ModelEntry] {
    match provider {
        ProviderKind::Anthropic => anthropic::models(),
        ProviderKind::OpenAi => openai::models(),
        ProviderKind::Copilot => copilot::models(),
        ProviderKind::Ollama => ollama::models(),
        ProviderKind::Mistral => mistral::models(),
        ProviderKind::Google => google::models(),
        ProviderKind::Zai | ProviderKind::ZaiCodingPlan => zai::models(),
        ProviderKind::Synthetic => synthetic::models(),
        ProviderKind::DeepSeek => deepseek::models(),
    }
}

impl ModelFamily {
    pub fn supports_tool_examples(self) -> bool {
        match self {
            ModelFamily::Claude | ModelFamily::Gpt | ModelFamily::Synthetic => true,
            ModelFamily::Generic | ModelFamily::Gemini | ModelFamily::Glm => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Model {
    pub id: String,
    pub provider: ProviderKind,
    pub dynamic_slug: Option<String>,
    pub tier: ModelTier,
    pub family: ModelFamily,
    pub supports_tool_examples_override: Option<bool>,
    pub pricing: ModelPricing,
    pub max_output_tokens: u32,
    pub context_window: u32,
}

impl Model {
    pub fn supports_tool_examples(&self) -> bool {
        self.supports_tool_examples_override
            .unwrap_or_else(|| self.family.supports_tool_examples())
    }

    pub fn spec(&self) -> String {
        if let Some(slug) = &self.dynamic_slug {
            format!("{slug}/{}", self.id)
        } else {
            format!("{}/{}", self.provider, self.id)
        }
    }

    pub fn from_tier(provider: ProviderKind, tier: ModelTier) -> Result<Self, ModelError> {
        if let Some(spec) = crate::tier_map::tier_map()
            .read()
            .unwrap()
            .spec_for_tier(provider, tier)
        {
            return Self::from_spec(&spec);
        }
        let entries = models_for_provider(provider);
        let entry = entries
            .iter()
            .find(|e| e.default && e.tier == tier)
            .ok_or(ModelError::NoDefault(provider, tier))?;
        let model_id = entry.prefixes[0];
        Self::from_spec(&format!("{provider}/{model_id}"))
    }

    pub fn from_tier_dynamic(
        provider: ProviderKind,
        tier: ModelTier,
        dynamic_slug: Option<&str>,
    ) -> Result<Self, ModelError> {
        if let Some(slug) = dynamic_slug {
            if let Some(model) = dynamic::find_model_for_tier(slug, tier) {
                return Ok(model);
            }
            let mut model = Self::from_tier(provider, tier)?;
            model.dynamic_slug = Some(slug.to_string());
            return Ok(model);
        }
        Self::from_tier(provider, tier)
    }

    pub fn from_spec(spec: &str) -> Result<Self, ModelError> {
        let (provider_str, model_id) = spec.split_once('/').ok_or(ModelError::InvalidFormat)?;

        if let Ok(provider) = ProviderKind::from_str(provider_str) {
            let static_entry = lookup_entry(models_for_provider(provider), model_id).ok();
            let tier = crate::tier_map::tier_map().read().unwrap().tier_for(
                spec,
                provider,
                static_entry.map(|e| e.tier),
            );
            let (family, pricing, max_output_tokens, context_window) = match static_entry {
                Some(e) => (
                    e.family,
                    e.pricing.clone(),
                    e.max_output_tokens,
                    e.context_window,
                ),
                None => (
                    provider.family(),
                    ModelPricing::ZERO,
                    provider.fallback_max_output(),
                    provider.fallback_context_window(),
                ),
            };
            return Ok(Self {
                id: model_id.to_string(),
                provider,
                dynamic_slug: None,
                tier,
                family,
                supports_tool_examples_override: None,
                pricing,
                max_output_tokens,
                context_window,
            });
        }

        if let Some(base) = dynamic::base_for_slug(provider_str) {
            if let Some(model) = dynamic::lookup_model(provider_str, model_id) {
                return Ok(model);
            }
            let entries = models_for_provider(base);
            let entry = lookup_entry(entries, model_id)?;
            return Ok(Self {
                id: model_id.to_string(),
                provider: base,
                dynamic_slug: Some(provider_str.to_string()),
                tier: entry.tier,
                family: entry.family,
                supports_tool_examples_override: None,
                pricing: entry.pricing.clone(),
                max_output_tokens: entry.max_output_tokens,
                context_window: entry.context_window,
            });
        }

        Err(ModelError::UnsupportedProvider(provider_str.to_string()))
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Non-cached input tokens. Total input = `input + cache_read + cache_creation`.
    #[serde(rename = "input_tokens")]
    pub input: u32,
    #[serde(rename = "output_tokens")]
    pub output: u32,
    #[serde(rename = "cache_creation_input_tokens")]
    pub cache_creation: u32,
    #[serde(rename = "cache_read_input_tokens")]
    pub cache_read: u32,
}

impl TokenUsage {
    pub fn total_input(&self) -> u32 {
        self.input + self.cache_read + self.cache_creation
    }

    pub fn context_tokens(&self) -> u32 {
        self.input + self.output + self.cache_creation + self.cache_read
    }

    pub fn cost(&self, pricing: &ModelPricing) -> f64 {
        self.input as f64 * pricing.input / PER_MILLION
            + self.output as f64 * pricing.output / PER_MILLION
            + self.cache_creation as f64 * pricing.cache_write / PER_MILLION
            + self.cache_read as f64 * pricing.cache_read / PER_MILLION
    }
}

impl AddAssign for TokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        self.input += rhs.input;
        self.output += rhs.output;
        self.cache_creation += rhs.cache_creation;
        self.cache_read += rhs.cache_read;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderKind;
    use strum::IntoEnumIterator;
    use test_case::test_case;

    const TIERS: [ModelTier; 3] = [ModelTier::Weak, ModelTier::Medium, ModelTier::Strong];

    #[test_case("no-slash-here", ModelError::InvalidFormat ; "invalid_format")]
    #[test_case("foobar/gpt-4", ModelError::UnsupportedProvider("foobar".into()) ; "unsupported_provider")]
    fn from_spec_errors(spec: &str, expected: ModelError) {
        let err = Model::from_spec(spec).unwrap_err();
        assert_eq!(
            std::mem::discriminant(&err),
            std::mem::discriminant(&expected)
        );
    }

    #[test]
    fn total_input_includes_cached_tokens() {
        let usage = TokenUsage {
            input: 5_000,
            output: 1_000,
            cache_creation: 10_000,
            cache_read: 150_000,
        };
        assert_eq!(usage.total_input(), 165_000);
    }

    #[test]
    fn cost_computes_all_token_types() {
        let pricing = ModelPricing {
            input: 3.00,
            output: 15.00,
            cache_write: 3.75,
            cache_read: 0.30,
        };
        let usage = TokenUsage {
            input: 1_000_000,
            output: 100_000,
            cache_creation: 200_000,
            cache_read: 500_000,
        };
        let cost = usage.cost(&pricing);
        let expected = 3.0 + 1.5 + 0.75 + 0.15;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn spec_roundtrip() {
        for provider in ProviderKind::iter() {
            if provider.accepts_arbitrary_models() {
                continue;
            }
            let model = Model::from_tier(provider, ModelTier::Medium).unwrap();
            let round = Model::from_spec(&model.spec()).unwrap();
            assert_eq!(round.id, model.id);
            assert_eq!(round.provider, model.provider);
        }
    }

    #[test]
    fn from_tier_covers_all_providers() {
        for provider in ProviderKind::iter() {
            if provider.accepts_arbitrary_models() {
                continue;
            }
            for &tier in &TIERS {
                // DeepSeek has no Weak tier model
                if provider == ProviderKind::DeepSeek && tier == ModelTier::Weak {
                    continue;
                }
                let model = Model::from_tier(provider, tier).unwrap();
                assert_eq!(model.provider, provider);
                assert_eq!(model.tier, tier);
                assert!(model.max_output_tokens > 0);
                assert!(model.context_window >= model.max_output_tokens);
            }
        }
    }

    #[test]
    fn tier_display_roundtrip() {
        for &tier in &TIERS {
            let s = tier.to_string();
            assert_eq!(s.parse::<ModelTier>().unwrap(), tier);
        }
        assert!(matches!(
            "turbo".parse::<ModelTier>(),
            Err(ModelError::InvalidTier(_))
        ));
    }

    #[test]
    fn exactly_one_default_per_provider_tier() {
        for provider in ProviderKind::iter() {
            if provider.accepts_arbitrary_models() {
                continue;
            }
            let entries = models_for_provider(provider);
            for &tier in &TIERS {
                if provider == ProviderKind::DeepSeek && tier == ModelTier::Weak {
                    continue;
                }
                let count = entries
                    .iter()
                    .filter(|e| e.tier == tier && e.default)
                    .count();
                assert_eq!(
                    count, 1,
                    "{provider}/{tier}: expected exactly 1 default, found {count}"
                );
            }
        }
    }

    #[test_case("anthropic/claude-99-turbo", ProviderKind::Anthropic, "claude-99-turbo" ; "unknown_anthropic_model_accepted")]
    #[test_case("zai/glm-99", ProviderKind::Zai, "glm-99" ; "unknown_zai_model_accepted")]
    #[test_case("openai/gpt-99", ProviderKind::OpenAi, "gpt-99" ; "unknown_openai_model_accepted")]
    #[test_case("synthetic/hf:nonexistent", ProviderKind::Synthetic, "hf:nonexistent" ; "unknown_synthetic_model_accepted")]
    #[test_case("ollama/my-custom-model", ProviderKind::Ollama, "my-custom-model" ; "unknown_ollama_model_accepted")]
    #[test_case("deepseek/my-custom-model", ProviderKind::DeepSeek, "my-custom-model" ; "unknown_deepseek_model_accepted")]
    fn unknown_model_accepted(spec: &str, expected_provider: ProviderKind, expected_id: &str) {
        let model = Model::from_spec(spec).unwrap();
        assert_eq!(model.provider, expected_provider);
        assert_eq!(model.id, expected_id);
        assert_eq!(model.family, expected_provider.family());
    }
}
