//! Model registry with prefix-based lookup and token accounting.
//! Lookup is prefix-based: `claude-sonnet-4-20250514` matches the `claude-sonnet-4` entry,
//! so dated snapshots resolve without registry churn. `context_tokens()` sums input + output
//! + cache reads/writes because the context window limit applies to all of them combined.

use std::fmt;
use std::ops::AddAssign;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::provider::ProviderKind;
use crate::providers::{anthropic, dynamic, openai, synthetic, zai};

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

#[derive(Debug, Clone)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFamily {
    Claude,
    Glm,
    Gpt,
    Synthetic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
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

#[derive(Clone)]
pub struct ModelEntry {
    pub prefixes: &'static [&'static str],
    pub tier: ModelTier,
    pub family: ModelFamily,
    pub default: bool,
    pub pricing: ModelPricing,
    pub max_output_tokens: u32,
    pub context_window: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSet {
    Visible,
    All,
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

pub fn models_for_provider(provider: ProviderKind, set: ModelSet) -> &'static [ModelEntry] {
    match (provider, set) {
        (ProviderKind::Anthropic, _) => anthropic::models(),
        (ProviderKind::OpenAi, _) => openai::api::models(),
        (ProviderKind::OpenAiCodingPlan, set) => openai::plan_models::models(set),
        (ProviderKind::Zai | ProviderKind::ZaiCodingPlan, _) => zai::models(),
        (ProviderKind::Synthetic, _) => synthetic::models(),
    }
}

fn resolve_provider_entry(provider: ProviderKind, model_id: &str) -> Option<&'static ModelEntry> {
    let entries = models_for_provider(provider, ModelSet::All);
    lookup_entry(entries, model_id).ok()
}

impl ModelFamily {
    pub fn supports_tool_examples(self) -> bool {
        match self {
            ModelFamily::Claude | ModelFamily::Gpt | ModelFamily::Synthetic => true,
            ModelFamily::Glm => false,
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
    pub pricing: ModelPricing,
    pub max_output_tokens: u32,
    pub context_window: u32,
}

impl Model {
    pub fn spec(&self) -> String {
        if let Some(slug) = &self.dynamic_slug {
            format!("{slug}/{}", self.id)
        } else {
            format!("{}/{}", self.provider, self.id)
        }
    }

    pub fn from_tier(provider: ProviderKind, tier: ModelTier) -> Result<Self, ModelError> {
        let entries = models_for_provider(provider, ModelSet::All);
        let entry = entries
            .iter()
            .find(|e| e.default && e.tier == tier)
            .ok_or(ModelError::NoDefault(provider, tier))?;
        let model_id = entry.prefixes[0];
        Self::from_spec(&format!("{provider}/{model_id}"))
    }

    pub fn from_spec(spec: &str) -> Result<Self, ModelError> {
        let (provider_str, model_id) = spec.split_once('/').ok_or(ModelError::InvalidFormat)?;

        if let Ok(provider) = ProviderKind::from_str(provider_str) {
            if provider == ProviderKind::OpenAiCodingPlan {
                let entry = openai::plan_models::catalog_model_metadata(model_id)
                    .ok_or_else(|| ModelError::UnknownModel(model_id.to_string()))?;
                return Ok(Self {
                    id: model_id.to_string(),
                    provider,
                    dynamic_slug: None,
                    tier: entry.tier,
                    family: entry.family,
                    pricing: entry.pricing,
                    max_output_tokens: entry.max_output_tokens,
                    context_window: entry.context_window,
                });
            }
            let entry = resolve_provider_entry(provider, model_id)
                .ok_or_else(|| ModelError::UnknownModel(model_id.to_string()))?;
            return Ok(Self {
                id: model_id.to_string(),
                provider,
                dynamic_slug: None,
                tier: entry.tier,
                family: entry.family,
                pricing: entry.pricing.clone(),
                max_output_tokens: entry.max_output_tokens,
                context_window: entry.context_window,
            });
        }

        if let Some(base) = dynamic::base_for_slug(provider_str) {
            if !dynamic::resolves_model(provider_str, model_id) {
                return Err(ModelError::UnknownModel(model_id.to_string()));
            }
            let entry = dynamic::dynamic_model_metadata(provider_str, model_id)
                .or_else(|| {
                    dynamic::fallback_model_entry(provider_str)
                        .map(dynamic::DynamicModelMetadata::from_entry)
                })
                .ok_or_else(|| ModelError::UnknownModel(model_id.to_string()))?;
            return Ok(Self {
                id: model_id.to_string(),
                provider: base,
                dynamic_slug: Some(provider_str.to_string()),
                tier: entry.tier,
                family: entry.family,
                pricing: entry.pricing,
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
    use crate::providers::openai::{plan_models, reset_plan_codex_cli_version};
    use crate::set_openai_plan_codex_cli_version;
    use strum::IntoEnumIterator;
    use test_case::test_case;

    const TIERS: [ModelTier; 3] = [ModelTier::Weak, ModelTier::Medium, ModelTier::Strong];
    const TEST_CODEX_CLI_VERSION: &str = "0.118.0";

    #[test_case("no-slash-here", ModelError::InvalidFormat ; "invalid_format")]
    #[test_case("foobar/gpt-4", ModelError::UnsupportedProvider("foobar".into()) ; "unsupported_provider")]
    #[test_case("anthropic/claude-99-turbo", ModelError::UnknownModel("claude-99-turbo".into()) ; "unknown_anthropic_model")]
    #[test_case("zai/glm-99", ModelError::UnknownModel("glm-99".into()) ; "unknown_zai_model")]
    #[test_case("openai/gpt-99", ModelError::UnknownModel("gpt-99".into()) ; "unknown_openai_model")]
    #[test_case("synthetic/hf:nonexistent", ModelError::UnknownModel("hf:nonexistent".into()) ; "unknown_synthetic_model")]
    #[test_case("openai-coding-plan/gpt-99", ModelError::UnknownModel("gpt-99".into()) ; "unknown_openai_coding_plan_model")]

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
    fn openai_coding_plan_accepts_upstream_snapshot_model() {
        reset_plan_codex_cli_version();
        set_openai_plan_codex_cli_version(TEST_CODEX_CLI_VERSION);
        plan_models::reset_plan_models_for_tests();
        plan_models::cache_default_models_for_tests(
            r#"{
                "models": [
                    {"slug": "gpt-5.3-codex", "priority": 2, "context_window": 272000, "visibility": "list"}
                ]
            }"#,
        );
        let model = Model::from_spec("openai-coding-plan/gpt-5.3-codex").unwrap();

        assert_eq!(model.provider, ProviderKind::OpenAiCodingPlan);
        assert_eq!(model.id, "gpt-5.3-codex");
        assert_eq!(model.family, ModelFamily::Gpt);
    }

    #[test]
    fn openai_coding_plan_tier_defaults_match_codex() {
        let weak = Model::from_tier(ProviderKind::OpenAiCodingPlan, ModelTier::Weak).unwrap();
        let medium = Model::from_tier(ProviderKind::OpenAiCodingPlan, ModelTier::Medium).unwrap();
        let strong = Model::from_tier(ProviderKind::OpenAiCodingPlan, ModelTier::Strong).unwrap();

        assert_eq!(weak.id, "gpt-5.4-mini");
        assert_eq!(medium.id, "gpt-5.2");
        assert_eq!(strong.id, "gpt-5.4");
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
            let model = Model::from_tier(provider, ModelTier::Medium).unwrap();
            let round = Model::from_spec(&model.spec()).unwrap();
            assert_eq!(round.id, model.id);
            assert_eq!(round.provider, model.provider);
        }
    }

    #[test]
    fn from_tier_covers_all_providers() {
        for provider in ProviderKind::iter() {
            for &tier in &TIERS {
                let model = Model::from_tier(provider, tier).unwrap();
                assert_eq!(model.provider, provider);
                assert_eq!(model.tier, tier);
                assert!(model.max_output_tokens > 0);
                assert!(model.context_window > model.max_output_tokens);
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
            let entries = models_for_provider(provider, ModelSet::All);
            for &tier in &TIERS {
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
}
