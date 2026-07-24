//! Model registry with prefix-based lookup and token accounting.
//! Lookup is prefix-based: `claude-sonnet-4-20250514` matches the `claude-sonnet-4` entry,
//! so dated snapshots resolve without registry churn. `context_tokens()` sums input + output
//! + cache reads/writes because the context window limit applies to all of them combined.

use std::any::Any;
use std::fmt;
use std::ops::AddAssign;
use std::str::FromStr;
use std::sync::Arc;

use maki_storage::sessions::{MIN_THINKING_BUDGET, StoredTokenUsage};
use serde::{Deserialize, Serialize};

use crate::manifest::{ManifestRegistry, ProviderManifest};
use crate::model_registry::model_registry;
use crate::providers::{anthropic, custom, dynamic};

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
    NoDefault(String, ModelTier),
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
    /// Anthropic fast mode charges a premium that differs per model. `None`
    /// means the model has no fast tier, so asking for fast mode quietly falls
    /// back to standard rates instead of overcharging.
    #[serde(default)]
    pub fast: Option<FastPricing>,
}

/// Metadata discovered at runtime from a provider's `/models` endpoint.
/// All fields optional -- most providers only return an ID.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub pricing: Option<ModelPricing>,
    pub supports_thinking: Option<bool>,
    pub supports_vision: Option<bool>,
    /// Store of additional metadata from the provider.
    pub provider_info: Option<Arc<dyn Any + Send + Sync>>,
}

impl ModelInfo {
    pub fn id_only(id: String) -> Self {
        Self {
            id,
            context_window: None,
            max_output_tokens: None,
            pricing: None,
            supports_thinking: None,
            supports_vision: None,
            provider_info: None,
        }
    }
}

/// Cache rates are missing on purpose: Anthropic derives them from `input` with
/// the same multipliers it uses for standard pricing, so storing them would just
/// invite the two copies to drift apart.
#[derive(Debug, Clone, Deserialize)]
pub struct FastPricing {
    pub input: f64,
    pub output: f64,
}

impl ModelPricing {
    pub const ZERO: Self = Self {
        input: 0.0,
        output: 0.0,
        cache_write: 0.0,
        cache_read: 0.0,
        fast: None,
    };

    pub fn is_zero(&self) -> bool {
        self.input == 0.0 && self.output == 0.0 && self.cache_write == 0.0 && self.cache_read == 0.0
    }

    /// Cache multipliers Anthropic applies on top of the base input rate.
    const CACHE_WRITE_MULTIPLIER: f64 = 1.25;
    const CACHE_READ_MULTIPLIER: f64 = 0.10;
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
    Compaction,
}

impl fmt::Display for ModelTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Weak => "weak",
            Self::Medium => "medium",
            Self::Strong => "strong",
            Self::Compaction => "compaction",
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
            "compaction" => Ok(Self::Compaction),
            other => Err(ModelError::InvalidTier(other.to_string())),
        }
    }
}

impl From<maki_config::providers::Tier> for ModelTier {
    fn from(t: maki_config::providers::Tier) -> Self {
        use maki_config::providers::Tier;
        match t {
            Tier::Weak => Self::Weak,
            Tier::Medium => Self::Medium,
            Tier::Strong => Self::Strong,
            Tier::Compaction => Self::Compaction,
        }
    }
}

#[derive(Debug)]
pub struct ModelEntry {
    pub prefixes: &'static [&'static str],
    pub tier: ModelTier,
    pub family: ModelFamily,
    /// Gates vision-only tools (`view_image`) and image blocks at request time.
    pub vision: bool,
    pub default: bool,
    pub pricing: ModelPricing,
    pub max_output_tokens: u32,
    pub context_window: u32,
}

pub(crate) fn lookup_entry<'a>(
    entries: &'a [ModelEntry],
    model_id: &str,
) -> Result<&'a ModelEntry, ModelError> {
    entries
        .iter()
        .flat_map(|e| e.prefixes.iter().map(move |p| (p, e)))
        .filter(|(p, _)| model_id.starts_with(*p))
        .max_by_key(|(p, _)| p.len())
        .map(|(_, e)| e)
        .ok_or_else(|| ModelError::UnknownModel(model_id.to_string()))
}

impl ModelFamily {
    pub fn supports_tool_examples(self) -> bool {
        match self {
            ModelFamily::Claude | ModelFamily::Gpt | ModelFamily::Synthetic => true,
            ModelFamily::Generic | ModelFamily::Gemini | ModelFamily::Glm => false,
        }
    }

    /// Fallback for models missing from the static tables; per-model truth
    /// lives in `ModelEntry::vision`.
    pub fn supports_vision(self) -> bool {
        matches!(self, Self::Claude | Self::Gpt | Self::Gemini)
    }
}

const FAST_PROVIDER: &str = "anthropic";

#[derive(Debug, Clone)]
pub struct Model {
    pub id: String,
    pub provider: Arc<str>,
    pub tier: ModelTier,
    pub family: ModelFamily,
    pub supports_tool_examples_override: Option<bool>,
    pub supports_thinking_override: Option<bool>,
    pub supports_vision_override: Option<bool>,
    pub pricing: ModelPricing,
    /// `None` when unknown, see [`ProviderKind::fallback_max_output`].
    pub max_output_tokens: Option<u32>,
    pub context_window: u32,
}

impl Model {
    /// When no static entry matches (a freshly released model the table has not
    /// caught up to yet), fall back to the provider defaults so it still resolves.
    fn from_base(manifest: &ProviderManifest, slug: &str, model_id: &str) -> Self {
        let static_entry = lookup_entry(manifest.models, model_id).ok();
        let spec = format!("{slug}/{model_id}");
        // Discovery keys `known_models` by the builtin slug, so a dynamic or
        // custom slug reads positional tiers and metadata through its base.
        let tier = model_registry().read().unwrap().tier_for(
            &spec,
            manifest.slug,
            static_entry.map(|e| e.tier),
        );
        let (family, pricing, max_output_tokens, context_window) = match static_entry {
            Some(e) => (
                e.family,
                e.pricing.clone(),
                Some(e.max_output_tokens),
                anthropic::shared::long_context_window(model_id).unwrap_or(e.context_window),
            ),
            None => {
                let guard = model_registry().read().unwrap();
                let discovered = guard.discovered(manifest.slug, model_id);
                (
                    manifest.family,
                    discovered
                        .and_then(|d| d.pricing.clone())
                        .unwrap_or_default(),
                    discovered
                        .and_then(|d| d.max_output_tokens)
                        .or(manifest.fallback_max_output),
                    discovered
                        .and_then(|d| d.context_window)
                        .unwrap_or(manifest.fallback_context_window),
                )
            }
        };
        Self {
            id: model_id.to_string(),
            provider: Arc::from(slug),
            tier,
            family,
            supports_tool_examples_override: None,
            supports_thinking_override: None,
            supports_vision_override: None,
            pricing,
            max_output_tokens,
            context_window,
        }
    }

    pub fn supports_thinking(&self) -> bool {
        if let Some(thinking) = self.supports_thinking_override {
            return thinking;
        }
        // Discovery keys `known_models` by the builtin slug; resolve dynamic
        // and custom slugs through their base manifest before looking up.
        let Some(manifest) = ManifestRegistry::for_slug(&self.provider) else {
            return false;
        };
        model_registry()
            .read()
            .unwrap()
            .discovered(manifest.slug, &self.id)
            .and_then(|d| d.supports_thinking)
            .unwrap_or(manifest.supports_thinking)
    }

    pub fn supports_vision(&self) -> bool {
        if let Some(vision) = self.supports_vision_override {
            return vision;
        }
        let manifest = ManifestRegistry::for_slug(&self.provider);
        manifest
            .and_then(|m| {
                model_registry()
                    .read()
                    .unwrap()
                    .discovered(m.slug, &self.id)
                    .and_then(|d| d.supports_vision)
            })
            .or_else(|| {
                manifest
                    .and_then(|m| lookup_entry(m.models, &self.id).ok())
                    .map(|e| e.vision)
            })
            .unwrap_or_else(|| self.family.supports_vision())
    }

    pub fn supports_tool_examples(&self) -> bool {
        self.supports_tool_examples_override
            .unwrap_or_else(|| self.family.supports_tool_examples())
    }

    /// Half the output window, so the answer always has room after the
    /// thinking. `None` when the window is unknown: callers must then let
    /// budgets through unclamped. Providers cap further only where the API
    /// documents a hard limit (currently just Google).
    pub fn max_thinking_budget(&self) -> Option<u32> {
        self.max_output_tokens
            .map(|n| (n / 2).max(MIN_THINKING_BUDGET))
    }

    /// A model supports fast mode exactly when it carries fast-tier pricing, so
    /// capability and billing can never disagree. The provider gate keeps fast
    /// mode to Anthropic-based providers, resolved through the base manifest so
    /// oauth scripts keep it; Bedrock separately ignores `opts.fast` at request
    /// time.
    pub fn supports_fast(&self) -> bool {
        self.pricing.fast.is_some()
            && ManifestRegistry::for_slug(&self.provider).is_some_and(|m| m.slug == FAST_PROVIDER)
    }

    pub fn spec(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }

    pub fn provider_display_name(&self) -> &'static str {
        ManifestRegistry::for_slug(&self.provider).map_or("Unknown", |m| m.display_name)
    }

    pub fn from_tier(slug: &str, tier: ModelTier) -> Result<Self, ModelError> {
        if let Some(spec) = model_registry().read().unwrap().spec_for_tier(slug, tier) {
            return Self::from_spec(&spec);
        }
        let entry = ManifestRegistry::find_default_for_tier(slug, tier)
            .ok_or_else(|| ModelError::NoDefault(slug.to_string(), tier))?;
        let model_id = entry.prefixes[0];
        Self::from_spec(&format!("{slug}/{model_id}"))
    }

    pub fn from_tier_dynamic(slug: &str, tier: ModelTier) -> Result<Self, ModelError> {
        if let Some(model) = dynamic::find_model_for_tier(slug, tier) {
            return Ok(model);
        }
        // One providers.toml read, three answers: a model declared at this tier,
        // the provider exists but declares nothing here (inherit the base
        // protocol default under the custom slug, keeping its tier and pricing),
        // or no such provider.
        match custom::resolve_tier(slug, tier) {
            custom::TierLookup::Model(model) => return Ok(model),
            custom::TierLookup::NoModelForTier(base) => {
                let manifest = ManifestRegistry::get(&base.to_string())
                    .ok_or_else(|| ModelError::NoDefault(slug.to_string(), tier))?;
                let entry = manifest
                    .models
                    .iter()
                    .find(|e| e.default && e.tier == tier)
                    .ok_or_else(|| ModelError::NoDefault(slug.to_string(), tier))?;
                return Ok(Self::from_base(manifest, slug, entry.prefixes[0]));
            }
            custom::TierLookup::Unknown => {}
        }
        // Builtin or dynamic slug: resolve the base default under the slug
        // (dynamic slugs route through `base_for_slug`).
        if ManifestRegistry::get(slug).is_some() || dynamic::base_for_slug(slug).is_some() {
            return Self::from_tier(slug, tier);
        }
        Err(ModelError::UnsupportedProvider(slug.to_string()))
    }

    pub fn from_spec(spec: &str) -> Result<Self, ModelError> {
        let (slug, model_id) = spec.split_once('/').ok_or(ModelError::InvalidFormat)?;

        // Precedence: builtin, then dynamic script, then providers.toml custom.
        // Discovery drops any script slug a builtin or custom entry already owns,
        // so a script and a custom provider can never share a slug here.
        if let Some(manifest) = ManifestRegistry::get(slug) {
            return Ok(Self::from_base(manifest, slug, model_id));
        }

        if let Some(model) = dynamic::lookup_model(slug, model_id) {
            return Ok(model);
        }

        if let Some(base) = dynamic::base_for_slug(slug)
            && let Some(manifest) = ManifestRegistry::get(&base.to_string())
        {
            return Ok(Self::from_base(manifest, slug, model_id));
        }

        if let Some(model) = custom::lookup_model(slug, model_id) {
            return Ok(model);
        }

        Err(ModelError::UnsupportedProvider(slug.to_string()))
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

impl From<StoredTokenUsage> for TokenUsage {
    fn from(s: StoredTokenUsage) -> Self {
        Self {
            input: s.input,
            output: s.output,
            cache_creation: s.cache_creation,
            cache_read: s.cache_read,
        }
    }
}

impl From<TokenUsage> for StoredTokenUsage {
    fn from(u: TokenUsage) -> Self {
        Self {
            input: u.input,
            output: u.output,
            cache_creation: u.cache_creation,
            cache_read: u.cache_read,
        }
    }
}

impl TokenUsage {
    pub fn total_input(&self) -> u32 {
        self.input + self.cache_read + self.cache_creation
    }

    pub fn context_tokens(&self) -> u32 {
        self.input + self.output + self.cache_creation + self.cache_read
    }

    pub fn cost(&self, pricing: &ModelPricing, fast: bool) -> f64 {
        let (input, output, cache_write, cache_read) = match &pricing.fast {
            Some(f) if fast => (
                f.input,
                f.output,
                f.input * ModelPricing::CACHE_WRITE_MULTIPLIER,
                f.input * ModelPricing::CACHE_READ_MULTIPLIER,
            ),
            _ => (
                pricing.input,
                pricing.output,
                pricing.cache_write,
                pricing.cache_read,
            ),
        };
        self.input as f64 * input / PER_MILLION
            + self.output as f64 * output / PER_MILLION
            + self.cache_creation as f64 * cache_write / PER_MILLION
            + self.cache_read as f64 * cache_read / PER_MILLION
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
    use test_case::test_case;

    const TIERS: [ModelTier; 4] = [
        ModelTier::Weak,
        ModelTier::Medium,
        ModelTier::Strong,
        ModelTier::Compaction,
    ];

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
            fast: None,
        };
        let usage = TokenUsage {
            input: 1_000_000,
            output: 100_000,
            cache_creation: 200_000,
            cache_read: 500_000,
        };
        let cost = usage.cost(&pricing, false);
        let expected = 3.0 + 1.5 + 0.75 + 0.15;
        assert!((cost - expected).abs() < 1e-10);
    }

    #[test]
    fn fast_mode_applies_premium_rates() {
        let pricing = ModelPricing {
            input: 5.00,
            output: 25.00,
            cache_write: 6.25,
            cache_read: 0.50,
            fast: Some(FastPricing {
                input: 30.00,
                output: 150.00,
            }),
        };
        let usage = TokenUsage {
            input: 1_000_000,
            output: 1_000_000,
            cache_creation: 1_000_000,
            cache_read: 1_000_000,
        };
        let fast = usage.cost(&pricing, true);
        let expected = 30.0 + 150.0 + 37.5 + 3.0;
        assert!((fast - expected).abs() < 1e-10);
        assert!(fast > usage.cost(&pricing, false));
    }

    #[test]
    fn fast_flag_ignored_without_fast_tier() {
        let pricing = ModelPricing {
            input: 3.00,
            output: 15.00,
            cache_write: 3.75,
            cache_read: 0.30,
            fast: None,
        };
        let usage = TokenUsage {
            input: 1_000_000,
            output: 1_000_000,
            cache_creation: 0,
            cache_read: 0,
        };
        assert_eq!(usage.cost(&pricing, true), usage.cost(&pricing, false));
    }

    #[test]
    fn fast_pricing_is_always_a_premium() {
        for manifest in ManifestRegistry::builtins() {
            for entry in manifest.models {
                let Some(fast) = &entry.pricing.fast else {
                    continue;
                };
                assert!(
                    fast.input >= entry.pricing.input && fast.output >= entry.pricing.output,
                    "{}/{}: fast pricing must not be cheaper than standard",
                    manifest.slug,
                    entry.prefixes[0],
                );
            }
        }
    }

    #[test]
    fn spec_roundtrip() {
        for manifest in ManifestRegistry::builtins() {
            if manifest.accepts_arbitrary_models {
                continue;
            }
            let model = Model::from_tier(manifest.slug, ModelTier::Medium).unwrap();
            let round = Model::from_spec(&model.spec()).unwrap();
            assert_eq!(round.id, model.id);
            assert_eq!(round.provider, model.provider);
        }
    }

    #[test]
    fn opencode_from_spec_parses_four_levels() {
        let spec = "opencode/nvidia/openai/gpt-oss-120b";
        let model = Model::from_spec(spec).unwrap();
        assert_eq!(model.provider, Arc::<str>::from("opencode"));
        assert_eq!(model.id, "nvidia/openai/gpt-oss-120b");
        assert_eq!(model.spec(), spec);
    }

    #[test]
    fn opencode_from_spec_parses_three_levels() {
        let spec = "opencode/opencode/big-pickle";
        let model = Model::from_spec(spec).unwrap();
        assert_eq!(model.provider, Arc::<str>::from("opencode"));
        assert_eq!(model.id, "opencode/big-pickle");
        assert_eq!(model.spec(), spec);
    }

    #[test]
    fn from_tier_covers_all_providers() {
        for manifest in ManifestRegistry::builtins() {
            if manifest.accepts_arbitrary_models {
                continue;
            }
            let slug: Arc<str> = Arc::from(manifest.slug);
            for &tier in &TIERS {
                // DeepSeek has no Weak tier model
                if manifest.slug == "deepseek" && tier == ModelTier::Weak {
                    continue;
                }
                // Compaction is user-assigned only, not in static registry
                if tier == ModelTier::Compaction {
                    continue;
                }
                let model = Model::from_tier(manifest.slug, tier).unwrap();
                assert_eq!(model.provider, slug);
                assert_eq!(model.tier, tier);
                let max_output = model.max_output_tokens.unwrap();
                assert!(max_output > 0);
                assert!(model.context_window >= max_output);
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
        for manifest in ManifestRegistry::builtins() {
            if manifest.accepts_arbitrary_models {
                continue;
            }
            let entries = manifest.models;
            for &tier in &TIERS {
                if manifest.slug == "deepseek" && tier == ModelTier::Weak {
                    continue;
                }
                // Compaction is user-assigned only, not in static registry
                if tier == ModelTier::Compaction {
                    continue;
                }
                let count = entries
                    .iter()
                    .filter(|e| e.tier == tier && e.default)
                    .count();
                assert_eq!(
                    count, 1,
                    "{}/{}: expected exactly 1 default, found {count}",
                    manifest.slug, tier
                );
            }
        }
    }

    #[test_case("anthropic/claude-99-turbo", "anthropic", "claude-99-turbo" ; "unknown_anthropic_model_accepted")]
    #[test_case("zai/glm-99", "zai", "glm-99" ; "unknown_zai_model_accepted")]
    #[test_case("openai/gpt-99", "openai", "gpt-99" ; "unknown_openai_model_accepted")]
    #[test_case("synthetic/hf:nonexistent", "synthetic", "hf:nonexistent" ; "unknown_synthetic_model_accepted")]
    #[test_case("ollama/my-custom-model", "ollama", "my-custom-model" ; "unknown_ollama_model_accepted")]
    #[test_case("deepseek/my-custom-model", "deepseek", "my-custom-model" ; "unknown_deepseek_model_accepted")]
    fn unknown_model_accepted(spec: &str, expected_slug: &str, expected_id: &str) {
        let model = Model::from_spec(spec).unwrap();
        assert_eq!(model.provider, Arc::<str>::from(expected_slug));
        assert_eq!(model.id, expected_id);
        let manifest = ManifestRegistry::get(expected_slug).unwrap();
        assert_eq!(model.family, manifest.family);
    }

    #[test]
    fn from_base_unknown_model_uses_provider_fallbacks() {
        // Deliberately fake id so this stays valid when the model table changes.
        let model = Model::from_base(
            ManifestRegistry::get("anthropic").unwrap(),
            "anthropic",
            "claude-nonexistent-99",
        );
        assert_eq!(model.provider, Arc::<str>::from("anthropic"));
        assert_eq!(model.id, "claude-nonexistent-99");
        assert_eq!(model.spec(), "anthropic/claude-nonexistent-99");
        assert_eq!(model.family, ModelFamily::Claude);
        assert_eq!(model.max_output_tokens, Some(128_000));
        assert_eq!(model.context_window, 200_000);
        let p = &model.pricing;
        assert_eq!(
            (p.input, p.output, p.cache_write, p.cache_read),
            (0.0, 0.0, 0.0, 0.0)
        );
    }

    #[test_case("anthropic/claude-opus-4-8",       true  ; "claude")]
    #[test_case("openai/gpt-5.4",                   true  ; "gpt")]
    #[test_case("google/gemini-2.5-pro",            true  ; "gemini")]
    #[test_case("copilot/claude-opus-4.7",          true  ; "copilot_entry_beats_generic_family")]
    #[test_case("zai/glm-5-code",                   false ; "glm_code_text_only")]
    #[test_case("deepseek/deepseek-v4-pro",         false ; "deepseek_text_only")]
    #[test_case("mistral/mistral-medium-latest",    true  ; "mistral_medium")]
    #[test_case("mistral/ministral-14b-latest",     false ; "ministral_text_only")]
    #[test_case("anthropic/claude-nonexistent-99",  true  ; "unknown_model_uses_family_fallback")]
    #[test_case("deepseek/my-custom-model",         false ; "unknown_generic_defaults_off")]
    fn vision_resolved_from_entry_or_family(spec: &str, expected: bool) {
        assert_eq!(Model::from_spec(spec).unwrap().supports_vision(), expected);
    }

    #[test_case("claude-opus-5",    true  ; "entry_with_fast_pricing")]
    #[test_case("claude-opus-5-1m", true  ; "long_context_suffix_still_matches_prefix")]
    #[test_case("claude-opus-4-7",  false ; "fast_withdrawn_from_the_table")]
    #[test_case("claude-sonnet-5",  false ; "entry_without_fast_pricing")]
    #[test_case("claude-opus-99",   false ; "no_entry_at_all")]
    fn supports_fast_follows_anthropic_table(model_id: &str, expected: bool) {
        let model = Model::from_base(
            ManifestRegistry::get("anthropic").unwrap(),
            "anthropic",
            model_id,
        );
        assert_eq!(model.supports_fast(), expected);
    }

    #[test]
    fn supports_fast_false_for_non_anthropic_even_with_fast_pricing() {
        let mut model = Model::from_base(
            ManifestRegistry::get("google").unwrap(),
            "google",
            "gemini-2.5-pro",
        );
        model.pricing.fast = Some(FastPricing {
            input: 30.0,
            output: 150.0,
        });
        assert!(!model.supports_fast());
    }

    #[test]
    fn discovered_context_window_flows_into_from_base_for_unknown_model() {
        use crate::model::ModelInfo;

        let slug: Arc<str> = Arc::from("ollama");
        let model_id = "test-discovered-context-window-model";
        let expected_window: u32 = 131_072;

        // Seed discovered metadata into the global registry
        {
            let mut reg = model_registry().write().unwrap();
            reg.set_known_models(
                &slug,
                vec![ModelInfo {
                    id: model_id.to_string(),
                    context_window: Some(expected_window),
                    max_output_tokens: None,
                    pricing: None,
                    supports_thinking: None,
                    supports_vision: None,
                    provider_info: None,
                }],
            );
        }

        // from_base for this unknown model should pick up the discovered context_window
        let model = Model::from_base(ManifestRegistry::get("ollama").unwrap(), "ollama", model_id);
        assert_eq!(model.id, model_id);
        assert_eq!(model.context_window, expected_window);
        // max_output_tokens falls back to provider default since not discovered
        assert_eq!(model.max_output_tokens, Some(16_384));

        // A dynamic/custom slug shares its base provider's discovery.
        let wrapped = Model::from_base(
            ManifestRegistry::get("ollama").unwrap(),
            "my-ollama-wrap",
            model_id,
        );
        assert_eq!(wrapped.spec(), format!("my-ollama-wrap/{model_id}"));
        assert_eq!(wrapped.context_window, expected_window);
    }
}
